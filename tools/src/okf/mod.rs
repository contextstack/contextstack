//! `okf`: deterministic OKF corpus operations (stamp, migrate, lint, stale). A port of the
//! Historical Ruby okf behaviour, byte
//! for byte: stdout, the JSON error object on stderr, exit codes and file writes. It writes
//! no call log, as the Ruby did not.

mod corpus;
mod front;
pub mod psych;
pub mod rb;
mod rtime;
mod scanner;
mod utf8;

use corpus::RootError;
pub use corpus::errno;
use rb::{Raise, Val, inspect, inspect_str, json_str, json_str_array, to_s, to_s_bytes};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// What the process prints and its exit code.
pub struct Outcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: u8,
}

/// How an action ends early: okf's `fail!` (a JSON error on stderr) or an uncaught Ruby
/// exception (its message line on stderr). Both exit 1.
enum Stop {
    Fail(String),
    Raise(Raise),
}

impl From<Raise> for Stop {
    fn from(r: Raise) -> Self {
        Self::Raise(r)
    }
}

type Step<T> = Result<T, Stop>;

/// Run okf on the raw stdin bytes.
pub fn run(input: &[u8]) -> Outcome {
    let mut out = Vec::new();
    let result = dispatch(input, &mut out);
    let (stderr, code) = match result {
        Ok(code) => (Vec::new(), code),
        Err(Stop::Fail(message)) => {
            let line = format!("{{\"ok\":false,\"error\":{}}}\n", json_str(&message));
            (line.into_bytes(), 1)
        }
        Err(Stop::Raise(r)) => (
            format!("okf: {} ({})\n", r.message, r.class).into_bytes(),
            1,
        ),
    };
    Outcome {
        stdout: out,
        stderr,
        code,
    }
}

/// A JSON arg as Ruby's `to_s` renders it.
fn arg_s(v: &Value) -> String {
    match to_s(&from_json(v)) {
        Val::Str(s) => s,
        other => inspect(&other),
    }
}

fn from_json(v: &Value) -> Val {
    match v {
        Value::Null => Val::Nil,
        Value::Bool(b) => Val::Bool(*b),
        Value::Number(n) if n.is_f64() => Val::Float(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => Val::Int(n.to_string()),
        Value::String(s) => Val::Str(s.clone()),
        Value::Array(items) => Val::Array(items.iter().map(from_json).collect()),
        Value::Object(map) => Val::Hash(
            map.iter()
                .map(|(k, v)| (Val::Str(k.clone()), from_json(v)))
                .collect(),
        ),
    }
}

/// `args.fetch(key, default).to_s`.
fn fetch_s(args: &serde_json::Map<String, Value>, key: &str, default: &str) -> String {
    args.get(key).map_or_else(|| default.to_string(), arg_s)
}

fn dispatch(input: &[u8], out: &mut Vec<u8>) -> Step<u8> {
    let text: &[u8] = if input.is_empty() { b"{}" } else { input };
    let parsed: Value =
        serde_json::from_slice(text).map_err(|e| Raise::new("JSON::ParserError", e.to_string()))?;
    let args = match parsed {
        Value::Object(args) => args,
        Value::Array(_) => {
            let message = "no implicit conversion of String into Integer";
            return Err(Raise::new("TypeError", message).into());
        }
        _ => return Err(Raise::new("NoMethodError", "undefined method 'fetch'").into()),
    };
    let action = args
        .get("action")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let root = match corpus::root(&fetch_s(&args, "root", ".")) {
        Ok(root) => root,
        Err(RootError::Fail(m)) => return Err(Stop::Fail(m.to_string())),
        Err(RootError::Raise(r)) => return Err(r.into()),
    };
    let action = match action.as_str() {
        Some(name @ ("stamp" | "migrate" | "lint" | "stale")) => name.to_string(),
        _ => {
            return Err(Stop::Fail(
                "action must be stamp, migrate, lint, or stale".to_string(),
            ));
        }
    };
    let now = match (action.as_str(), args.get("now")) {
        ("stale", None) => return Err(Raise::new("KeyError", "key not found: \"now\"").into()),
        (_, now) => now.map(arg_s).unwrap_or_default(),
    };
    let files = corpus::markdown_files(&root);
    if let Some(rel) = corpus::escaping(&root, &files) {
        return Err(Stop::Fail(format!("path escapes workspace: {rel}")));
    }
    let ctx = Corpus { root, files };
    match action.as_str() {
        "stamp" => ctx.stamp(&fetch_s(&args, "version", "0.1"), out),
        "migrate" => ctx.migrate(
            &fetch_s(&args, "from", "0.1"),
            &fetch_s(&args, "to", "0.2"),
            out,
        ),
        "lint" => ctx.lint(&fetch_s(&args, "version", "0.2"), out),
        _ => ctx.stale(&now, out),
    }
}

/// `read_utf8`.
fn read_utf8(path: &Path) -> Step<String> {
    use std::io::Read as _;
    let mut file = crate::jail::open_for_read(path)
        .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?;
    String::from_utf8(bytes).map_err(|_| Stop::Fail(format!("{}: invalid UTF-8", path.display())))
}

// Unique, exclusively created sibling temps cannot follow planted symlinks. Drop
// removes uncommitted temps if any staging operation fails.
fn stage(path: &Path, content: &str) -> Step<(tempfile::NamedTempFile, PathBuf)> {
    use std::io::Write as _;
    let target = if path.exists() {
        crate::jail::open_for_read(path)
            .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?;
        fs::canonicalize(path)
    } else {
        path.parent()
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map(|parent| parent.join(path.file_name().unwrap_or_default()))
    }
    .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?;
    if crate::jail::resolves_outside(target.parent().unwrap_or(Path::new("."))) {
        return Err(Stop::Fail("path escapes workspace".into()));
    }
    let mut temp = tempfile::NamedTempFile::new_in(target.parent().unwrap_or(Path::new(".")))
        .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?;
    if target.exists() {
        let permissions = fs::metadata(&target)
            .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?
            .permissions();
        temp.as_file()
            .set_permissions(permissions)
            .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?;
    }
    temp.write_all(content.as_bytes())
        .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?;
    temp.flush()
        .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_sysopen", path)))?;
    Ok((temp, target))
}

fn publish(temp: tempfile::NamedTempFile, path: &Path) -> Step<()> {
    temp.persist(path)
        .map_err(|e| Stop::Raise(corpus::errno(&e.error, "rb_file_s_rename", path)))?;
    Ok(())
}

fn write(path: &Path, content: &str) -> Step<()> {
    let (temp, target) = stage(path, content)?;
    publish(temp, &target)
}

fn basename_lower(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn reserved(path: &Path) -> bool {
    matches!(basename_lower(path).as_str(), "index.md" | "log.md")
}

/// The top-level mapping with keys `to_s`'d, as okf's `transform_keys(&:to_s)` leaves it.
type Meta = Vec<(Vec<u8>, Val)>;

fn meta_get<'a>(meta: &'a Meta, key: &str) -> Option<&'a Val> {
    meta.iter()
        .find(|(k, _)| k == key.as_bytes())
        .map(|(_, v)| v)
}

fn has(meta: &Meta, key: &str) -> bool {
    meta_get(meta, key).is_some()
}

/// `meta[key].to_s` as bytes (nil for a missing key, so empty).
fn meta_s(meta: &Meta, key: &str) -> Vec<u8> {
    meta_get(meta, key).map(to_s_bytes).unwrap_or_default()
}

fn transform_keys(v: Val) -> Option<Meta> {
    let Val::Hash(pairs) = v else {
        return None;
    };
    let mut meta: Meta = Vec::new();
    for (k, v) in pairs {
        let key = to_s_bytes(&k);
        match meta.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = v,
            None => meta.push((key, v)),
        }
    }
    Some(meta)
}

/// `parse_yaml`: nil for no frontmatter; a recorded error for invalid YAML or a non-mapping.
fn parse_yaml(raw: Option<&str>, relative: &str, errors: &mut Vec<String>) -> Step<Option<Meta>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match psych::safe_load(raw) {
        Ok(v) => {
            let meta = v.and_then(transform_keys);
            if meta.is_none() {
                errors.push(format!("{relative}: frontmatter must be a mapping"));
            }
            Ok(meta)
        }
        Err(e) if e.is_psych() => {
            let first = e.message.lines().next().unwrap_or("").trim();
            errors.push(format!("{relative}: invalid YAML ({}: {first})", e.class));
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// `yaml_mapping`: the mapping or nil; a nil raw is Ruby's uncaught `TypeError`.
fn yaml_mapping(raw: Option<&str>) -> Step<Option<Meta>> {
    let Some(raw) = raw else {
        return Err(Raise::new("TypeError", "no implicit conversion of nil into String").into());
    };
    match psych::safe_load(raw) {
        Ok(v) => Ok(v.and_then(transform_keys)),
        Err(e) if e.is_psych() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `valid_instant?`: a String (a Time's `to_s` never qualifies) ending in Z or an offset
/// that `Time.iso8601` accepts.
fn valid_instant(v: Option<&Val>) -> bool {
    let Some(text) = v.and_then(Val::string_bytes) else {
        return false;
    };
    let text = String::from_utf8_lossy(text);
    let b = text.as_bytes();
    let n = b.len();
    let zoned = text.ends_with('Z')
        || (n >= 6
            && matches!(b[n - 6], b'+' | b'-')
            && b[n - 5].is_ascii_digit()
            && b[n - 4].is_ascii_digit()
            && b[n - 3] == b':'
            && b[n - 2].is_ascii_digit()
            && b[n - 1].is_ascii_digit());
    zoned && rtime::iso8601(&text).is_some()
}

fn status_ok(meta: &Meta) -> bool {
    matches!(
        meta_s(meta, "status").as_slice(),
        b"draft" | b"stable" | b"deprecated"
    )
}

/// `hash.fetch(key, nil).to_s.empty?` (a non-Hash has nothing).
fn blank_field(v: Option<&Val>, key: &str) -> bool {
    v.and_then(|h| h.fetch(key))
        .is_none_or(|x| to_s_bytes(x).is_empty())
}

fn canonical_generated(v: &Val) -> bool {
    if !v.is_hash() || blank_field(Some(v), "by") {
        return false;
    }
    match v.fetch("at") {
        None | Some(Val::Nil) => true,
        Some(at) => valid_instant(Some(at)),
    }
}

/// `validate_optional`.
fn validate_optional(meta: &Meta, rel: &str, errors: &mut Vec<String>) {
    if has(meta, "status") && !status_ok(meta) {
        errors.push(format!(
            "{rel}: status must be draft, stable, or deprecated"
        ));
    }
    if has(meta, "stale_after") && !valid_instant(meta_get(meta, "stale_after")) {
        errors.push(format!(
            "{rel}: stale_after must be ISO 8601 with an offset"
        ));
    }
    if let Some(generated) = meta_get(meta, "generated") {
        if !generated.is_hash() || blank_field(Some(generated), "by") {
            errors.push(format!(
                "{rel}: generated.by is required when generated is present"
            ));
        }
        let at = generated.fetch("at").filter(|a| a.truthy());
        if at.is_some() && !valid_instant(at) {
            errors.push(format!(
                "{rel}: generated.at must be ISO 8601 with an offset"
            ));
        }
    }
    if let Some(verified) = meta_get(meta, "verified") {
        let events: Vec<Val> = match verified {
            Val::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        for (i, event) in events.iter().enumerate() {
            if blank_field(Some(event), "by") {
                errors.push(format!("{rel}: verified[{i}].by is required"));
            }
            if !valid_instant(event.fetch("at")) {
                errors.push(format!(
                    "{rel}: verified[{i}].at must be ISO 8601 with an offset"
                ));
            }
        }
    }
    let Some(sources) = meta_get(meta, "sources") else {
        return;
    };
    let sources: Vec<Val> = match sources {
        Val::Hash(_) | Val::Recursive(true) => vec![sources.clone()],
        Val::Array(items) => items.clone(),
        Val::Recursive(false) => Vec::new(),
        _ => {
            errors.push(format!("{rel}: sources must be a mapping or list"));
            return;
        }
    };
    for (i, source) in sources.iter().enumerate() {
        if blank_field(Some(source), "resource") {
            errors.push(format!("{rel}: sources[{i}].resource is required"));
        }
    }
}

/// Ruby's `\s+` to one space, then `strip`.
fn normalize_body(body: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for c in body.chars() {
        if rb::is_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        out.push(c);
    }
    rb::strip(&out).to_string()
}

struct Corpus {
    root: PathBuf,
    files: Vec<PathBuf>,
}

impl Corpus {
    fn rel(&self, path: &Path) -> String {
        corpus::relative(path, &self.root)
    }

    fn root_index(&self, path: &Path) -> bool {
        path.parent() == Some(self.root.as_path()) && basename_lower(path) == "index.md"
    }

    fn root_index_content(&self, version: &str) -> Step<String> {
        let body = if let Some(path) = corpus::index_entry(&self.root) {
            let text = read_utf8(&path)?;
            front::split(&text).1.to_string()
        } else {
            let name = self
                .root
                .file_name()
                .map_or_else(|| "/".into(), |n| n.to_string_lossy());
            format!("# {name}\n")
        };
        Ok(format!(
            "---\nokf_version: {}\n---\n\n{}",
            json_str(version),
            front::trim_leading_newlines(&body)
        ))
    }

    fn normalize_index_name(&self) -> Step<()> {
        if let Some(existing) = corpus::index_entry(&self.root) {
            let path = self.root.join("index.md");
            if existing.file_name() != path.file_name() {
                fs::rename(&existing, &path)
                    .map_err(|e| Stop::Raise(corpus::errno(&e, "rb_file_s_rename", &existing)))?;
            }
        }
        Ok(())
    }

    fn ensure_root_index(&self, version: &str) -> Step<()> {
        let content = self.root_index_content(version)?;
        let path = corpus::index_entry(&self.root).unwrap_or_else(|| self.root.join("index.md"));
        write(&path, &content)?;
        self.normalize_index_name()
    }

    fn stamp(&self, version: &str, out: &mut Vec<u8>) -> Step<u8> {
        let mut changed = 0;
        for path in &self.files {
            if reserved(path) {
                continue;
            }
            let text = read_utf8(path)?;
            let (raw, body) = front::split(&text);
            let updated = front::add_key(raw, "schema_version", &json_str(version));
            let updated = front::add_key(Some(&updated), "type", "\"Legacy Document\"");
            if raw == Some(updated.as_str()) {
                continue;
            }
            write(path, &front::render(&updated, body))?;
            changed += 1;
        }
        self.ensure_root_index(version)?;
        let line = format!(
            "{{\"ok\":true,\"action\":\"stamp\",\"version\":{},\"changed\":{changed}}}\n",
            json_str(version)
        );
        out.extend_from_slice(line.as_bytes());
        Ok(0)
    }

    fn migrate(&self, from: &str, to: &str, out: &mut Vec<u8>) -> Step<u8> {
        let mut errors = Vec::new();
        let mut writes: Vec<(&PathBuf, String)> = Vec::new();
        for path in &self.files {
            let rel = self.rel(path);
            let text = read_utf8(path)?;
            let (raw, body) = front::split(&text);
            if reserved(path) {
                if self.root_index(path) {
                    continue;
                }
                if raw.is_some() {
                    writes.push((path, front::trim_leading_newlines(body).to_string()));
                }
                continue;
            }
            let mut normalized = raw.map(str::to_string);
            let mut meta = yaml_mapping(normalized.as_deref())?;
            if meta.is_none() {
                let repaired = front::repair_legacy(raw.unwrap_or(""));
                meta = yaml_mapping(Some(&repaired))?;
                normalized = Some(repaired);
            }
            let normalized = normalized.unwrap_or_default();
            let Some(meta) = meta else {
                parse_yaml(Some(&normalized), &rel, &mut errors)?;
                continue;
            };
            let actual = meta_s(&meta, "schema_version");
            if actual != from.as_bytes() && actual != to.as_bytes() {
                let found = match meta_get(&meta, "schema_version").map(to_s) {
                    Some(Val::Bin(b)) => inspect(&Val::Bin(b)),
                    _ => inspect_str(&String::from_utf8_lossy(&actual)),
                };
                errors.push(format!(
                    "{rel}: expected schema_version {}, found {found}",
                    inspect_str(from)
                ));
                continue;
            }
            let mut updated = front::replace_key(&normalized, "schema_version", &json_str(to));
            if has(&meta, "status") && !status_ok(&meta) {
                let escape = if has(&meta, "legacy_status") {
                    "legacy_status_v01"
                } else {
                    "legacy_status"
                };
                updated = front::rename_key(&updated, "status", escape);
            }
            if meta_get(&meta, "generated").is_some_and(|g| !canonical_generated(g)) {
                let escape = if has(&meta, "legacy_generated") {
                    "legacy_generated_v01"
                } else {
                    "legacy_generated"
                };
                updated = front::rename_key(&updated, "generated", escape);
            }
            if raw == Some(updated.as_str()) {
                continue;
            }
            writes.push((path, front::render(&updated, body)));
        }
        if !errors.is_empty() {
            return Err(Stop::Fail(first(&errors, 40).join("; ")));
        }
        // Stage the root index too: failures reading or writing it must happen
        // before any concept is replaced. Renames are atomic per file, not a
        // corpus-wide crash transaction; an OS failure during publish can still
        // require rerunning the idempotent migration.
        let index_path =
            corpus::index_entry(&self.root).unwrap_or_else(|| self.root.join("index.md"));
        let index_content = self.root_index_content(to)?;
        let mut staged = Vec::with_capacity(writes.len() + 1);
        for (path, content) in &writes {
            staged.push(stage(path, content)?);
        }
        staged.push(stage(&index_path, &index_content)?);
        for (temp, path) in staged {
            publish(temp, &path)?;
        }
        self.normalize_index_name()?;
        let line = format!(
            "{{\"ok\":true,\"action\":\"migrate\",\"from\":{},\"to\":{},\"changed\":{}}}\n",
            json_str(from),
            json_str(to),
            writes.len()
        );
        out.extend_from_slice(line.as_bytes());
        Ok(0)
    }

    fn lint(&self, version: &str, out: &mut Vec<u8>) -> Step<u8> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut groups: Vec<Vec<String>> = Vec::new();
        let mut group_of: HashMap<String, usize> = HashMap::new();
        let mut concepts = 0;
        let root_index = corpus::index_entry(&self.root);
        if let Some(root_index) = root_index {
            let text = read_utf8(&root_index)?;
            let index_name = root_index
                .file_name()
                .map_or_else(|| "index.md".into(), |n| n.to_string_lossy().into_owned());
            let meta = parse_yaml(front::split(&text).0, &index_name, &mut errors)?;
            let ok = meta
                .as_ref()
                .is_some_and(|m| meta_s(m, "okf_version") == version.as_bytes());
            if !ok {
                errors.push(format!(
                    "index.md: okf_version must equal {}",
                    inspect_str(version)
                ));
            }
            if meta
                .as_ref()
                .is_some_and(|m| m.iter().any(|(k, _)| k != b"okf_version"))
            {
                errors.push("index.md: only okf_version is permitted".to_string());
            }
        } else {
            warnings.push("index.md: root index is recommended".to_string());
        }
        for path in &self.files {
            let rel = self.rel(path);
            let text = read_utf8(path)?;
            let (raw, body) = front::split(&text);
            let base = basename_lower(path);
            if base == "log.md" {
                if raw.is_some() {
                    errors.push(format!("{rel}: log.md must not carry frontmatter"));
                }
                continue;
            }
            if base == "index.md" {
                if !self.root_index(path) && raw.is_some() {
                    errors.push(format!(
                        "{rel}: only the bundle-root index.md may carry frontmatter"
                    ));
                }
                continue;
            }
            let Some(meta) = parse_yaml(raw, &rel, &mut errors)? else {
                continue;
            };
            concepts += 1;
            if rb::strip(&String::from_utf8_lossy(&meta_s(&meta, "type"))).is_empty() {
                errors.push(format!("{rel}: type is required"));
            }
            if meta_s(&meta, "schema_version") != version.as_bytes() {
                errors.push(format!(
                    "{rel}: schema_version must equal {}",
                    inspect_str(version)
                ));
            }
            validate_optional(&meta, &rel, &mut errors);
            let normalized = normalize_body(body);
            if !normalized.is_empty() {
                let next = groups.len();
                let slot = *group_of.entry(normalized).or_insert(next);
                if slot == next {
                    groups.push(Vec::new());
                }
                groups[slot].push(rel);
            }
        }
        let duplicates: Vec<&Vec<String>> = groups.iter().filter(|g| g.len() > 1).collect();
        if !duplicates.is_empty() {
            warnings.push(format!(
                "{} duplicate content groups require sweep-time dedupe review",
                duplicates.len()
            ));
        }
        let dup_json: Vec<String> = duplicates
            .iter()
            .take(25)
            .map(|g| json_str_array(g))
            .collect();
        let line = format!(
            "{{\"ok\":{},\"action\":\"lint\",\"version\":{},\"files\":{},\"concepts\":{concepts},\"errors\":{},\"warnings\":{},\"duplicate_groups\":[{}]}}\n",
            errors.is_empty(),
            json_str(version),
            self.files.len(),
            json_str_array(first(&errors, 100)),
            json_str_array(first(&warnings, 100)),
            dup_json.join(",")
        );
        out.extend_from_slice(line.as_bytes());
        Ok(u8::from(!errors.is_empty()))
    }

    /// `stale`, whose `rescue ArgumentError` turns any `ArgumentError` inside it (a bad `now`,
    /// or `Float()` on a `!!float` value) into the `now` refusal.
    fn stale(&self, now: &str, out: &mut Vec<u8>) -> Step<u8> {
        let bad_now = || Stop::Fail("now must be ISO 8601 with an offset".to_string());
        let result = self.stale_rows(now, out);
        match result {
            Err(Stop::Raise(r)) if r.class == "ArgumentError" => Err(bad_now()),
            other => other,
        }
    }

    fn stale_rows(&self, now: &str, out: &mut Vec<u8>) -> Step<u8> {
        let instant = rtime::iso8601(now)
            .ok_or_else(|| Raise::new("ArgumentError", "invalid xmlschema format"))?;
        let mut rows: Vec<(String, String, i64)> = Vec::new();
        let mut errors = Vec::new();
        for path in &self.files {
            if reserved(path) {
                continue;
            }
            let rel = self.rel(path);
            let text = read_utf8(path)?;
            let meta = parse_yaml(front::split(&text).0, &rel, &mut errors)?;
            let Some(value) = meta.as_ref().and_then(|m| meta_get(m, "stale_after")) else {
                continue;
            };
            if !valid_instant(Some(value)) {
                errors.push(format!("{rel}: invalid stale_after"));
                continue;
            }
            let text =
                String::from_utf8_lossy(value.string_bytes().unwrap_or_default()).into_owned();
            let Some(deadline) = rtime::iso8601(&text) else {
                continue;
            };
            if instant.cmp_instant(&deadline).is_ge() {
                rows.push((deadline.iso8601(), rel, instant.seconds_since(&deadline)));
            }
        }
        if !errors.is_empty() {
            return Err(Stop::Fail(first(&errors, 40).join("; ")));
        }
        rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let items: Vec<String> = rows
            .iter()
            .map(|(at, rel, overdue)| {
                format!(
                    "{{\"path\":{},\"stale_after\":{},\"overdue_seconds\":{overdue}}}",
                    json_str(rel),
                    json_str(at)
                )
            })
            .collect();
        let line = format!(
            "{{\"ok\":true,\"action\":\"stale\",\"now\":{},\"stale\":[{}]}}\n",
            json_str(&instant.iso8601()),
            items.join(",")
        );
        out.extend_from_slice(line.as_bytes());
        Ok(0)
    }
}

fn first(items: &[String], n: usize) -> &[String] {
    &items[..items.len().min(n)]
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::jail::CWD_TEST_LOCK;
    use std::fs;
    use std::path::Path;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    struct CwdRestore {
        prev: std::path::PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl CwdRestore {
        fn chdir(path: &Path) -> Self {
            let lock = CWD_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::current_dir().unwrap_or_else(|e| panic!("cwd: {e}"));
            std::env::set_current_dir(path).unwrap_or_else(|e| panic!("chdir: {e}"));
            Self { prev, _lock: lock }
        }
    }

    impl Drop for CwdRestore {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    fn corpus_workspace() -> TempDir {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = dir.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("bare.md"), "# bare\nno frontmatter yet.\n")
            .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("bad.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstatus: bogus\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        dir
    }

    fn out_text(o: &super::Outcome) -> (String, String, u8) {
        (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            o.code,
        )
    }

    #[test]
    fn stamp_adds_schema_version_and_updates_root_index() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stamp","root":"corpus","version":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.starts_with("{\"ok\":true,\"action\":\"stamp\""));
        let bare = fs::read_to_string(ws.path().join("corpus/bare.md"))
            .unwrap_or_else(|e| panic!("read: {e}"));
        assert!(bare.contains("schema_version: \"0.2\""));
        assert!(bare.contains("type: \"Legacy Document\""));
    }

    #[test]
    fn lint_reports_validation_errors_and_ok_false() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!(stderr, "");
        assert_eq!(code, 1);
        assert!(stdout.contains("\"ok\":false"));
        assert!(stdout.contains("status must be draft, stable, or deprecated"));
    }

    #[test]
    fn migrate_refuses_unexpected_schema_version() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("wrong.md"),
            "---\nschema_version: \"0.9\"\ntype: \"Legacy Document\"\n---\n\nx\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stdout.as_str()), (1, ""));
        assert!(stderr.contains("expected schema_version"));
    }

    #[test]
    fn dispatch_rejects_non_object_args_and_bad_json() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(b"[]");
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("TypeError"));
        let o = run(b"{not json");
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("JSON::ParserError"));
    }

    #[test]
    fn stale_without_now_raises_key_error() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stale","root":"corpus"}"#);
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("key not found: \"now\""));
    }

    #[test]
    fn lint_warns_when_root_index_is_missing() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("one.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, _) = out_text(&o);
        assert!(stdout.contains("root index is recommended"));
    }

    #[test]
    fn stale_refuses_invalid_stale_after_on_a_document() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("stale.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstale_after: \"not-a-time\"\n---\n\nx\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stale","root":"corpus","now":"2026-01-02T00:00:00Z"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stdout.as_str()), (1, ""));
        assert!(stderr.contains("invalid stale_after"));
    }

    #[test]
    fn migrate_escapes_legacy_status_during_schema_bump() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("doc.md"),
            "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\nstatus: bogus\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"action\":\"migrate\""));
        let updated =
            fs::read_to_string(root.join("doc.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(updated.contains("legacy_status:"));
    }

    #[test]
    fn lint_reports_duplicate_content_groups_as_warnings() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let body = "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\n---\n\nShared.\n";
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("one.md"), body).unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("two.md"), body).unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, _) = out_text(&o);
        assert!(stdout.contains("duplicate content groups"));
    }

    #[test]
    fn dispatch_refuses_unknown_action_name() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"wipe","root":"corpus"}"#);
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("action must be stamp, migrate, lint, or stale"));
    }

    #[test]
    fn dispatch_rejects_scalar_json_input_with_no_method_error() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(b"42");
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("NoMethodError"));
    }

    #[test]
    fn lint_rejects_log_md_with_frontmatter() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("log.md"),
            "---\nschema_version: \"0.2\"\n---\n\nentries\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("log.md must not carry frontmatter"));
    }

    #[test]
    fn lint_rejects_nested_bundle_index_frontmatter() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir_all(root.join("sub")).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("sub/index.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\n---\n\nnested\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("only the bundle-root index.md may carry frontmatter"));
    }

    #[test]
    fn lint_rejects_extra_keys_on_root_index() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\nextra: 1\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("one.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("only okf_version is permitted"));
    }

    #[test]
    fn lint_requires_type_and_matching_schema_on_concepts() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("no_type.md"),
            "---\nschema_version: \"0.2\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("wrong_ver.md"),
            "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("type is required"));
        assert!(stdout.contains("schema_version must equal"));
    }

    #[test]
    fn lint_validates_optional_generated_verified_and_sources() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("rich.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstatus: stable\nstale_after: \"not-zoned\"\ngenerated:\n  at: \"2026-01-01T00:00:00Z\"\nverified:\n  - at: \"2026-01-01T00:00:00Z\"\nsources: 1\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("stale_after must be ISO 8601"));
        assert!(stdout.contains("generated.by is required"));
        assert!(stdout.contains("verified[0].by is required"));
        assert!(stdout.contains("sources must be a mapping or list"));
    }

    #[test]
    fn migrate_succeeds_for_matching_schema_and_updates_version() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("doc.md"),
            "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\nstatus: stable\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"action\":\"migrate\""));
        let text = fs::read_to_string(root.join("doc.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(text.contains("schema_version: \"0.2\""));
        assert!(text.contains("status: stable"));
    }

    #[test]
    fn migrate_renames_noncanonical_generated_to_legacy_field() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("doc.md"),
            "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\ngenerated:\n  at: \"not-a-time\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"changed\""));
        let text = fs::read_to_string(root.join("doc.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(text.contains("legacy_generated:"));
    }

    #[test]
    fn stale_lists_documents_past_stale_after_deadline() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("old.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstale_after: \"2020-01-01T00:00:00Z\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stale","root":"corpus","now":"2026-06-01T00:00:00Z"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"action\":\"stale\""));
        assert!(stdout.contains("old.md"));
        assert!(stdout.contains("overdue_seconds"));
    }

    #[test]
    fn stale_refuses_unzoned_now_argument() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stale","root":"corpus","now":"yesterday"}"#);
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("now must be ISO 8601 with an offset"));
    }

    #[test]
    fn run_reports_invalid_utf8_read_as_fail_json() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("badbytes.md"), b"\xff\xfe").unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stamp","root":"corpus","version":"0.2"}"#);
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("invalid UTF-8"));
    }

    #[test]
    fn lint_rejects_root_index_okf_version_mismatch() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("one.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("okf_version must equal"));
    }

    #[test]
    fn lint_flags_missing_source_resource_and_bad_verified_at() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("meta.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nverified:\n  by: agent\n  at: yesterday\nsources:\n  - {}\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("verified[0].at must be ISO 8601"));
        assert!(stdout.contains("sources[0].resource is required"));
    }

    #[test]
    fn stamp_renames_nonstandard_index_filename_to_index_md() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("INDEX.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("bare.md"), "# bare\n").unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stamp","root":"corpus","version":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"action\":\"stamp\""));
        assert!(root.join("index.md").is_file());
        let index =
            fs::read_to_string(root.join("index.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(index.contains("okf_version: \"0.2\""));
    }

    #[test]
    fn migrate_strips_frontmatter_from_reserved_log_md() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("log.md"),
            "---\nschema_version: \"0.1\"\n---\n\nlog body\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"action\":\"migrate\""));
        let log = fs::read_to_string(root.join("log.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(!log.starts_with("---"));
        assert!(log.contains("log body"));
    }

    #[test]
    fn lint_records_non_mapping_frontmatter_as_error() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("list.md"), "---\n- 1\n---\n\nBody.\n")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("frontmatter must be a mapping"));
    }

    #[test]
    fn lint_requires_resource_on_hash_shaped_sources() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("src.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nsources:\n  url: x\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("sources[0].resource is required"));
    }

    #[test]
    fn migrate_uses_v01_escape_when_legacy_generated_already_exists() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("doc.md"),
            "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\nlegacy_generated: {}\ngenerated:\n  at: \"not-a-time\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"action\":\"migrate\""));
        let text = fs::read_to_string(root.join("doc.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(text.contains("legacy_generated_v01:"));
    }

    #[test]
    fn empty_stdin_defaults_to_object_and_refuses_missing_action() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(b"");
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("action must be stamp, migrate, lint, or stale"));
    }

    #[test]
    fn lint_rejects_invalid_generated_at_timestamp() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("gen.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\ngenerated:\n  by: agent\n  at: \"2026-01-01T00:00:00\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("generated.at must be ISO 8601"));
    }

    #[test]
    fn stamp_stringifies_numeric_version_from_json() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stamp","root":"corpus","version":0.2}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"version\":\"0.2\""));
    }

    #[test]
    fn migrate_renames_to_legacy_status_v01_when_already_present() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("doc.md"),
            "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\nlegacy_status: x\nstatus: bogus\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (_, _, code) = out_text(&o);
        assert_eq!(code, 0);
        let text = fs::read_to_string(root.join("doc.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(text.contains("legacy_status_v01:"));
    }

    #[test]
    fn lint_passes_optional_metadata_when_values_are_canonical() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("good.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstatus: stable\nstale_after: \"2027-01-01T00:00:00Z\"\ngenerated:\n  by: agent\nverified:\n  - by: agent\n    at: \"2026-01-01T00:00:00Z\"\nsources:\n  - resource: note\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"ok\":true"));
    }

    #[test]
    fn migrate_repairs_legacy_scalar_frontmatter_before_bump() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("legacy.md"),
            "---\nschema_version: 0.1\ntitle: It's: odd\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"action\":\"migrate\""));
        let text =
            fs::read_to_string(root.join("legacy.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(text.contains("schema_version: \"0.2\""));
    }

    #[test]
    fn stale_returns_empty_when_nothing_is_overdue() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("fresh.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstale_after: \"2099-01-01T00:00:00Z\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stale","root":"corpus","now":"2026-01-01T00:00:00Z"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"stale\":[]") || stdout.contains("\"stale\": []"));
    }

    #[test]
    fn migrate_staging_failure_cleans_temps_without_publishing() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("{e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("{e}"));
        fs::create_dir(root.join("index.md")).unwrap_or_else(|e| panic!("{e}"));
        let original = "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\n---\nBody\n";
        fs::write(root.join("doc.md"), original).unwrap_or_else(|e| panic!("{e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        assert_ne!(o.code, 0);
        assert_eq!(
            fs::read_to_string(root.join("doc.md")).unwrap_or_else(|e| panic!("{e}")),
            original
        );
        assert_eq!(
            fs::read_dir(&root)
                .unwrap_or_else(|e| panic!("{e}"))
                .count(),
            2
        );
    }

    #[test]
    fn migrate_bad_root_index_leaves_all_documents_unchanged() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("{e:?}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("{e:?}"));
        let original = "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\n---\nBody\n";
        fs::write(root.join("doc.md"), original).unwrap_or_else(|e| panic!("{e:?}"));
        fs::write(root.join("index.md"), [0xff]).unwrap_or_else(|e| panic!("{e:?}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        assert_ne!(o.code, 0);
        assert_eq!(
            fs::read_to_string(root.join("doc.md")).unwrap_or_else(|e| panic!("{e:?}")),
            original
        );
        assert_eq!(
            fs::read_dir(&root)
                .unwrap_or_else(|e| panic!("{e:?}"))
                .count(),
            2
        );
    }

    #[test]
    fn lint_finds_root_index_with_any_case() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("INDEX.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("doc.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"ok\":true"));
        assert!(!stdout.contains("root index is recommended"));
    }

    #[test]
    fn lint_accepts_empty_sources_list_without_resource_errors() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("doc.md"),
            "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nsources: []\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"ok\":true"));
    }

    #[test]
    fn lint_rejects_scalar_sources_field() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("bad.md"), "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nsources: not-a-list\n---\n\nBody.\n").unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("sources must be a mapping or list"));
    }

    #[test]
    fn stamp_creates_index_title_from_directory_name_when_missing() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(root.join("note.md"), "# note\n").unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stamp","root":"corpus","version":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"action\":\"stamp\""));
        let index =
            fs::read_to_string(root.join("index.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(index.contains("okf_version: \"0.2\""));
        assert!(index.contains("# corpus"));
    }

    #[test]
    fn migrate_skips_unchanged_file_when_frontmatter_already_matches_target() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let body = "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstatus: stable\n---\n\nBody.\n";
        fs::write(root.join("done.md"), body).unwrap_or_else(|e| panic!("write: {e}"));
        let before =
            fs::read_to_string(root.join("done.md")).unwrap_or_else(|e| panic!("read: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("\"changed\":0"));
        assert_eq!(
            fs::read_to_string(root.join("done.md")).unwrap_or_else(|e| panic!("read: {e}")),
            before
        );
    }

    #[test]
    fn migrate_collects_psych_syntax_errors_for_unrepairable_frontmatter() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("broken.md"),
            "---\nschema_version: \"0.1\"\ntype: Note\n: [bad\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stdout.as_str()), (1, ""));
        assert!(stderr.contains("invalid YAML") || stderr.contains("Psych::"));
    }

    #[test]
    fn lint_flags_missing_verified_by_on_event_list() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("ev.md"), "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nverified:\n  - at: \"2026-01-01T00:00:00Z\"\n---\n\nBody.\n").unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("verified[0].by is required"));
    }

    #[test]
    fn lint_rejects_unzoned_stale_after_timestamp() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("stale.md"), "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstale_after: \"2026-01-01T00:00:00\"\n---\n\nBody.\n").unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("stale_after must be ISO 8601"));
    }

    #[test]
    fn migrate_schema_binary_value_prints_inspect_in_error() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("bin.md"),
            "---\nschema_version: !!binary \"0.9\"\ntype: \"Legacy Document\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stdout.as_str()), (1, ""));
        assert!(stderr.contains("expected schema_version"));
    }

    #[test]
    fn stamp_accepts_json_array_as_version_via_inspect() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stamp","root":"corpus","version":[1,2]}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("version"));
    }

    #[test]
    fn migrate_keeps_canonical_generated_without_renaming() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.1\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("gen.md"), "---\nschema_version: \"0.1\"\ntype: \"Legacy Document\"\ngenerated:\n  by: agent\n---\n\nBody.\n").unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"migrate","root":"corpus","from":"0.1","to":"0.2"}"#);
        let (_, _, code) = out_text(&o);
        assert_eq!(code, 0);
        let text = fs::read_to_string(root.join("gen.md")).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(text.contains("generated:"));
        assert!(!text.contains("legacy_generated"));
    }

    #[test]
    fn dispatch_surfaces_tilde_user_expand_error_as_stderr() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"~nobody"}"#);
        let (_, stderr, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stderr.contains("ArgumentError"));
    }

    #[test]
    fn lint_requires_type_on_concept_documents() {
        let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = ws.path().join("corpus");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("index.md"),
            "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            root.join("note.md"),
            "---\nschema_version: \"0.2\"\n---\n\nBody.\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"lint","root":"corpus","version":"0.2"}"#);
        let (stdout, _, code) = out_text(&o);
        assert_eq!(code, 1);
        assert!(stdout.contains("type is required"));
    }

    #[test]
    fn stamp_stringifies_boolean_version_from_json() {
        let ws = corpus_workspace();
        let _guard = CwdRestore::chdir(ws.path());
        let o = run(br#"{"action":"stamp","root":"corpus","version":true}"#);
        let (stdout, stderr, code) = out_text(&o);
        assert_eq!((code, stderr.as_str()), (0, ""));
        assert!(stdout.contains("true"));
    }
}
