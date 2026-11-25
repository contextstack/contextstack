//! Seatbelt profile for coding-agent executor runs — same semantics as `cs/ops/bin/_executor.py`
//! `sandbox_cmd` on macOS (`(allow default)` plus write/read narrowing).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::env_deny;
use crate::failure::{Failure, unenforceable};
use crate::policy::HostPolicy;
use crate::seatbelt::FCNTL_DENY;
use crate::ssh_agent_deny;

/// JSON request the executor parent writes beside a run (`executor-request.json`).
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorRequest {
    #[serde(default, deserialize_with = "request_version")]
    pub version: Option<u32>,
    #[serde(default)]
    pub variant: Option<Variant>,
    #[serde(default)]
    pub lookup_path: Vec<PathBuf>,
    #[serde(default)]
    pub interpreters: Vec<String>,
    #[serde(default)]
    pub login_targets: Vec<LoginTarget>,
    #[serde(default)]
    pub deny_write_roots: Vec<PathBuf>,
    #[serde(default)]
    pub toolchains: Option<Toolchains>,
    #[serde(default)]
    pub ssh_auth_sock: Option<PathBuf>,
    #[serde(default)]
    pub lane: Option<Lane>,
    #[serde(default)]
    pub run_protection: Option<RunProtection>,
    #[serde(default)]
    pub adapter: Option<Adapter>,
    #[serde(default)]
    pub lead: Option<Lead>,
    #[serde(default)]
    pub readonly_paths: Vec<PathBuf>,
    #[serde(default)]
    pub caller_grants: Vec<PathGrant>,
    pub workspace: PathBuf,
    pub tmpdir: PathBuf,
    #[serde(default)]
    pub check_scratch: Option<PathBuf>,
    pub home: PathBuf,
    #[serde(default)]
    pub executable: Option<PathBuf>,
    #[serde(default)]
    pub read_links: Vec<PathBuf>,
    #[serde(default)]
    pub read_roots: Vec<PathBuf>,
    #[serde(default)]
    pub home_reads: Vec<PathBuf>,
    #[serde(default)]
    pub sockets_allow: Vec<PathBuf>,
    #[serde(default)]
    pub login_writes: Vec<LoginWriteEntry>,
    #[serde(default)]
    pub protected: Option<PathBuf>,
}

/// Ordered caller path grants formerly carried as raw SBPL extras.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PathGrant {
    pub path: PathBuf,
    pub access: String,
}

fn request_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    u32::deserialize(deserializer).map(Some)
}

/// Versioned base profile selection.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    Native,
    Muse,
    Adapter,
}
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoginTarget {
    pub path: PathBuf,
    pub allow_directory: bool,
}
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Toolchains {
    #[serde(default)]
    pub cargo_configured: bool,
    #[serde(default)]
    pub read_roots: Vec<PathBuf>,
    #[serde(default)]
    pub proxy_paths: Vec<PathBuf>,
    #[serde(default)]
    pub uv_cache_roots: Vec<PathBuf>,
}
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lane {
    #[serde(default)]
    pub common_git_dir: Option<PathBuf>,
    #[serde(default)]
    pub other_worktrees: Vec<PathBuf>,
    #[serde(default)]
    pub own_worktree: Option<PathBuf>,
    #[serde(default)]
    pub own_git_dir: Option<PathBuf>,
    #[serde(default)]
    pub ref_paths: Vec<PathBuf>,
}
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunProtection {
    #[serde(default)]
    pub action_capabilities: bool,
    #[serde(default)]
    pub run_root: Option<PathBuf>,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub tmpdir: Option<PathBuf>,
    #[serde(default)]
    pub refresh_write_paths: Vec<PathBuf>,
}
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Adapter {
    #[serde(default)]
    pub auth_mode: Option<String>,
    #[serde(default)]
    pub auth_paths: Vec<PathBuf>,
    #[serde(default)]
    pub database_parent: Option<PathBuf>,
    #[serde(default)]
    pub localhost_port: Option<LocalhostPort>,
    #[serde(default)]
    pub project_config_paths: Vec<PathBuf>,
}
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum LocalhostPort {
    Number(u16),
    Pattern(String),
}
impl std::fmt::Display for LocalhostPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(port) => write!(f, "{port}"),
            Self::Pattern(port) => f.write_str(port),
        }
    }
}
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lead {
    #[serde(default)]
    pub write_roots: Vec<PathBuf>,
    #[serde(default)]
    pub denied_session_roots: Vec<PathBuf>,
    #[serde(default)]
    pub own_session_roots: Vec<PathBuf>,
    #[serde(default)]
    pub bridge_sockets: Vec<PathBuf>,
}

impl ExecutorRequest {
    fn validate(&self) -> Result<(), Failure> {
        let Some(version) = self.version else {
            // New fields must never silently affect a legacy request.
            if self.variant.is_some()
                || !self.lookup_path.is_empty()
                || self.ssh_auth_sock.is_some()
                || !self.interpreters.is_empty()
                || !self.login_targets.is_empty()
                || self
                    .toolchains
                    .as_ref()
                    .is_some_and(|v| *v != Toolchains::default())
                || self.has_caller_inputs()
                || self.has_future_inputs()
            {
                return Err(unenforceable("new executor fields require version 1"));
            }
            return Ok(());
        };
        if version != 1 {
            return Err(unenforceable(format!(
                "unsupported executor request version {version}"
            )));
        }
        if self.variant.is_none() {
            return Err(unenforceable("version 1 requires a variant"));
        }
        if self.variant == Some(Variant::Native)
            && (!self.readonly_paths.is_empty() || self.adapter.is_some())
        {
            return Err(unenforceable("native cannot carry Muse inputs"));
        }
        if self.variant != Some(Variant::Native) && self.lead.is_some() {
            return Err(unenforceable("lead record requires native variant"));
        }
        if self.variant != Some(Variant::Adapter) && self.adapter.is_some() {
            return Err(unenforceable("adapter record requires adapter variant"));
        }
        if self.check_scratch.is_some()
            && (self.variant == Some(Variant::Adapter) || self.lead.is_some())
        {
            return Err(unenforceable("check scratch requires a check profile"));
        }
        for path in [&self.workspace, &self.tmpdir, &self.home]
            .into_iter()
            .chain(self.executable.iter())
            .chain(self.check_scratch.iter())
            .chain(self.lookup_path.iter())
            .chain(self.read_roots.iter())
            .chain(self.home_reads.iter())
            .chain(self.read_links.iter())
            .chain(self.sockets_allow.iter())
            .chain(self.caller_grants.iter().map(|v| &v.path))
            .chain(self.readonly_paths.iter())
            .chain(self.adapter.iter().flat_map(|v| {
                v.auth_paths
                    .iter()
                    .chain(v.database_parent.iter())
                    .chain(v.project_config_paths.iter())
            }))
            .chain(self.lead.iter().flat_map(|v| {
                v.write_roots
                    .iter()
                    .chain(v.denied_session_roots.iter())
                    .chain(v.own_session_roots.iter())
                    .chain(v.bridge_sockets.iter())
            }))
            .chain(self.protected.iter())
            .chain(self.ssh_auth_sock.iter())
            .chain(self.login_targets.iter().map(|target| &target.path))
            .chain(self.deny_write_roots.iter())
            .chain(self.toolchains.iter().flat_map(|v| {
                v.read_roots
                    .iter()
                    .chain(&v.proxy_paths)
                    .chain(&v.uv_cache_roots)
            }))
            .chain(self.lane.iter().flat_map(|v| {
                v.common_git_dir
                    .iter()
                    .chain(v.other_worktrees.iter())
                    .chain(v.own_worktree.iter())
                    .chain(v.own_git_dir.iter())
                    .chain(v.ref_paths.iter())
            }))
            .chain(self.run_protection.iter().flat_map(|v| {
                v.run_root
                    .iter()
                    .chain(v.workspace.iter())
                    .chain(v.tmpdir.iter())
                    .chain(v.refresh_write_paths.iter())
            }))
        {
            if !path.is_absolute() {
                return Err(unenforceable("version 1 requires absolute paths"));
            }
        }
        Ok(())
    }

    fn has_caller_inputs(&self) -> bool {
        !self.caller_grants.is_empty()
            || !self.deny_write_roots.is_empty()
            || self.toolchains.as_ref().is_some_and(|v| {
                !v.read_roots.is_empty()
                    || !v.proxy_paths.is_empty()
                    || !v.uv_cache_roots.is_empty()
            })
            || self.lane.is_some()
            || self.run_protection.is_some()
    }

    fn has_future_inputs(&self) -> bool {
        !self.readonly_paths.is_empty()
            || self.adapter.is_some()
            || self.lead.is_some()
            || self.check_scratch.is_some()
    }
}

/// One `login_writes` template entry: exactly one of `literal`, `subpath`, or `regex`.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoginWriteEntry {
    pub literal: Option<String>,
    pub subpath: Option<String>,
    pub regex: Option<String>,
}

impl LoginWriteEntry {
    fn filter(&self) -> Result<(&'static str, &str), Failure> {
        let mut found = 0usize;
        let mut kind = None;
        let mut raw = None;
        for (name, value) in [
            ("literal", self.literal.as_deref()),
            ("subpath", self.subpath.as_deref()),
            ("regex", self.regex.as_deref()),
        ] {
            if let Some(text) = value {
                found += 1;
                kind = Some(name);
                raw = Some(text);
            }
        }
        if found != 1 {
            return Err(unenforceable(
                "login_writes requires one path filter per entry".to_owned(),
            ));
        }
        let kind = kind.unwrap_or("literal");
        let raw = raw.unwrap_or("");
        if !raw.starts_with("~/") {
            return Err(unenforceable(
                "login_writes requires a home-relative path filter".to_owned(),
            ));
        }
        Ok((kind, raw))
    }
}

/// Refuse emit when the host policy does not list `cli-executor`.
pub fn assert_host_allows_cli_executor(host: &HostPolicy) -> Result<(), Failure> {
    if host.profiles.cli_executor.is_some() {
        return Ok(());
    }
    Err(unenforceable(
        "host profiles does not list cli-executor; the profile was not emitted".to_owned(),
    ))
}

fn allow_write_subpath(path: &Path) -> Result<String, Failure> {
    Ok(format!(
        "(allow file-write* (subpath {}))",
        crate::sbpl::quote(path)?
    ))
}

fn deny_write_subpath(path: &Path) -> Result<String, Failure> {
    Ok(format!(
        "(deny file-write* (subpath {}))",
        crate::sbpl::quote(path)?
    ))
}

fn estate_read_rules(
    home: &Path,
    read_roots: &[PathBuf],
    v1: bool,
) -> Result<Vec<String>, Failure> {
    // Keep future estate state private; only tools and granted workspaces are readable.
    let named_estate = home.join(".contextstack");
    let estate = resolve(&named_estate, v1)?;
    let current = estate.join("current");
    let mut exceptions = vec![current.clone(), resolve(&current, v1)?];
    exceptions.extend(
        read_roots
            .iter()
            .map(|root| resolve(root, v1))
            .collect::<Result<Vec<_>, Failure>>()?
            .into_iter()
            .filter(|root| root.starts_with(&estate) && root != &estate),
    );
    let mut seen_exceptions = std::collections::BTreeSet::new();
    let filters = exceptions
        .iter()
        .filter(|path| seen_exceptions.insert((*path).clone()))
        .map(|path| Ok(format!("(require-not (subpath {}))", path_quote(path, v1)?)))
        .collect::<Result<Vec<_>, Failure>>()?
        .join(" ");
    let mut targets = vec![named_estate.clone()];
    if estate != named_estate {
        targets.push(estate.clone());
    }
    let mut rules: Vec<String> = targets
        .iter()
        .map(|target| {
            Ok(format!(
                "(deny file-read* file-write* file-link (require-all (subpath {}) {filters}))",
                path_quote(target, v1)?
            ))
        })
        .collect::<Result<Vec<_>, Failure>>()?;
    // Path resolution stats every parent of an exception: metadata only on those ancestors.
    let mut seen_ancestors = std::collections::BTreeSet::new();
    let mut ancestors = vec![named_estate];
    let mut unique = std::collections::BTreeSet::new();
    for path in exceptions
        .iter()
        .filter(|path| unique.insert((*path).clone()))
    {
        for parent in path.ancestors().skip(1) {
            if !parent.starts_with(&estate) {
                break;
            }
            ancestors.push(parent.to_path_buf());
        }
    }
    for parent in ancestors {
        if seen_ancestors.insert(parent.clone()) {
            rules.push(format!(
                "(allow file-read-metadata (literal {}))",
                path_quote(&parent, v1)?
            ));
        }
    }
    Ok(rules)
}

/// Home-relative secret paths every agent profile denies (mirrors `_cli_native.HOSTED_PATHS`).
const PROTECTED_READS: &[&str] = &[
    ".contextstack/credentials",
    ".contextstack/state/director",
    ".contextstack/dev-state/director",
    ".contextstack/gateway",
    ".contextstack/config/gateway/owner-token",
    ".contextstack/config/node.env",
    ".contextstack/config/.cs-init-node-token",
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gh",
    ".netrc",
    ".zshrc.secrets",
    "Library/Keychains",
    ".claude/.credentials.json",
    ".codex/auth.json",
    ".gemini/settings.json",
    ".gemini/oauth_creds.json",
    ".gemini/google_accounts.json",
    ".gemini/gemini-credentials.json",
    ".gemini/installation_id",
    ".pi/agent/auth.json",
    ".pi/agent/settings.json",
    ".pi/agent/models.json",
    ".config/goose/config.yaml",
    ".config/goose/chatgpt_codex",
    ".config/goose/gemini_oauth",
    ".config/goose/xai_oauth",
    ".grok/auth.json",
];

const HOSTED_READS: &[&str] = &[
    ".contextstack/director-db-backup",
    ".contextstack/browser",
    ".contextstack/state/browser-state",
    ".contextstack/config/lead-credential",
    ".contextstack/config/hosted-lead.json",
    ".contextstack/state/lead-sessions",
    ".contextstack/dev-state/lead-sessions",
];

fn executable_paths(
    executable: &Path,
    lookup: Option<&[PathBuf]>,
) -> Result<(PathBuf, Vec<PathBuf>), Failure> {
    let selected = if executable.components().count() == 1 {
        let paths = lookup.map_or_else(
            || {
                std::env::var_os("PATH")
                    .map(|paths| std::env::split_paths(&paths).collect())
                    .unwrap_or_default()
            },
            <[PathBuf]>::to_vec,
        );
        paths
            .into_iter()
            .map(|dir| dir.join(executable))
            .find(|p| {
                use std::os::unix::fs::PermissionsExt;
                p.metadata().is_ok_and(|m| {
                    m.is_file() && (lookup.is_none() || m.permissions().mode() & 0o111 != 0)
                })
            })
            .unwrap_or_else(|| executable.to_path_buf())
    } else {
        executable.to_path_buf()
    };
    let mut path = if selected.is_absolute() {
        selected
    } else {
        std::env::current_dir()
            .map_err(|e| unenforceable(e.to_string()))?
            .join(selected)
    };
    if lookup.is_some() {
        path = normalize_path(&path);
    }
    let mut hops = Vec::new();
    for iteration in 0..64 {
        let mut prefixes: Vec<_> = path.ancestors().collect();
        prefixes.reverse();
        let hop = prefixes.into_iter().find_map(|prefix| {
            std::fs::read_link(prefix)
                .ok()
                .map(|target| (prefix.to_path_buf(), target))
        });
        let Some((prefix, target)) = hop else { break };
        let suffix = path
            .strip_prefix(&prefix)
            .map_err(|e| unenforceable(e.to_string()))?;
        let joined = prefix
            .parent()
            .unwrap_or(Path::new("/"))
            .join(target)
            .join(suffix);
        hops.push(prefix);
        path = normalize_path(&joined);
        if iteration == 63 {
            return Err(unenforceable("executable symlink loop".to_owned()));
        }
    }
    Ok((path, hops))
}

fn executable_home_rules(
    home: &Path,
    executable: Option<&Path>,
    interpreter: bool,
    request: &ExecutorRequest,
) -> Result<Vec<String>, Failure> {
    let v1 = request.version.is_some();
    let Some(executable) = executable else {
        return Ok(Vec::new());
    };
    let (path, mut hops) = executable_paths(
        executable,
        request.version.map(|_| request.lookup_path.as_slice()),
    )?;
    let mut interpreter_rules = Vec::new();
    if request.version.is_some() && !interpreter {
        use std::io::{BufRead, Read};
        if let Ok(file) = std::fs::File::open(&path) {
            let mut header = Vec::new();
            let _ = std::io::BufReader::new(file)
                .take(4096)
                .read_until(b'\n', &mut header);
            if let Ok(header) = std::str::from_utf8(&header)
                && let Some(body) = header.strip_prefix("#!")
                && let Some(mut words) = shebang_words(body)
            {
                if words.first().is_some_and(|word| {
                    Path::new(word)
                        .file_name()
                        .is_some_and(|name| name == "env")
                }) {
                    words.remove(0);
                    if words.first().is_some_and(|word| word == "-S") {
                        words.remove(0);
                    }
                    if words.first().is_some_and(|word| word.starts_with('-')) {
                        words.clear();
                    }
                }
                if let Some(command) = words.first() {
                    interpreter_rules =
                        executable_home_rules(home, Some(Path::new(command)), true, request)?;
                }
            }
        }
    }
    if !path.starts_with(home) {
        return Ok(interpreter_rules);
    }
    let Some(mut install) = path.parent() else {
        return Ok(Vec::new());
    };
    if install.file_name().is_some_and(|name| name == "versions") {
        install = &path;
    }
    for parent in path.ancestors().skip(1) {
        if parent
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "versions")
        {
            install = parent;
            break;
        }
    }
    let broad = install == home
        || home.starts_with(install)
        || install.parent() == Some(home)
        || install == home.join(".local/bin");
    let mut rules = Vec::new();
    hops.push(path.clone());
    let mut seen = Vec::new();
    for hop in hops {
        if !seen.contains(&hop) {
            rules.push(format!(
                "(allow file-read* (literal {}))",
                path_quote(&hop, v1)?
            ));
            seen.push(hop);
        }
    }
    if !broad && !interpreter {
        rules.push(format!(
            "(allow file-read* (subpath {}))",
            path_quote(install, v1)?
        ));
    }
    let mut ancestors = Vec::new();
    for target in &seen {
        for parent in target.ancestors().skip(1) {
            if parent.starts_with(home) && !ancestors.contains(&parent) {
                ancestors.push(parent);
                rules.push(format!(
                    "(allow file-read-metadata (literal {}))",
                    path_quote(parent, v1)?
                ));
            }
        }
    }
    rules.extend(interpreter_rules);
    Ok(rules)
}

/// Read isolation rules mirroring `_cli_native.read_rules`.
fn read_rules(request: &ExecutorRequest, read_roots: &[PathBuf]) -> Result<Vec<String>, Failure> {
    let v1 = request.version.is_some();
    let home = resolve(&request.home, v1)?;
    let home_reads = &request.home_reads;
    // Deny secret reads and all writes before adding staged login read exceptions.
    let mut rules = if v1 {
        crate::seatbelt::home_read_rules_configured(
            &home,
            read_roots,
            home_reads,
            true,
            request
                .toolchains
                .as_ref()
                .is_some_and(|t| t.cargo_configured),
        )?
    } else {
        crate::seatbelt::home_read_rules(&home, read_roots, home_reads)?
    };
    rules.extend(executable_home_rules(
        &home,
        request.executable.as_deref(),
        false,
        request,
    )?);
    if v1 {
        rules.extend(caller_rules(request)?);
    }
    if v1 {
        for command in &request.interpreters {
            rules.extend(executable_home_rules(
                &home,
                Some(Path::new(command)),
                true,
                request,
            )?);
        }
    }
    let env_roots: Vec<_> = read_roots
        .iter()
        .map(|path| {
            if v1 {
                crate::sbpl::resolve(path)
            } else {
                Ok(path.clone())
            }
        })
        .collect::<Result<Vec<_>, Failure>>()?;
    rules.extend(env_deny::seatbelt_rules(&env_roots)?);
    for rel in PROTECTED_READS.iter().chain(HOSTED_READS) {
        let path = home.join(rel);
        let mut targets = Vec::new();
        targets.push(path.clone());
        let resolved = resolve(&path, v1)?;
        if resolved != path {
            targets.push(resolved);
        }
        let mut seen = std::collections::BTreeSet::new();
        for target in targets {
            let key = target.to_string_lossy().into_owned();
            if seen.insert(key.clone()) {
                rules.push(format!(
                    "(deny file-read* (subpath {}))",
                    path_quote(Path::new(&key), v1)?
                ));
            }
        }
    }
    rules.extend(estate_read_rules(&home, read_roots, v1)?);
    let keychains = resolve(&home.join("Library/Keychains"), v1)?;
    let mut seen_links = std::collections::BTreeSet::new();
    for link in &request.read_links {
        let target = resolve(link, v1)?;
        if !seen_links.insert(target.clone()) {
            continue;
        }
        let key = target.to_string_lossy().into_owned();
        let quoted = path_quote(Path::new(&key), v1)?;
        if target == keychains {
            rules.push(format!("(allow file-read-metadata (literal {quoted}))"));
            let pattern = format!("^{}/login\\.keychain-db[^/]*$", escape(&key, v1));
            let escaped = pattern.replace('"', r#"\""#);
            rules.push(format!(r#"(allow file-read* (regex #"{escaped}"))"#));
        } else {
            let kind = if target.is_dir() {
                "subpath"
            } else {
                "literal"
            };
            rules.push(format!("(allow file-read* ({kind} {quoted}))"));
        }
    }
    rules.extend(if v1 {
        crate::seatbelt::cli_socket_rules_native(&request.sockets_allow, true)?
    } else {
        crate::seatbelt::cli_socket_rules(&request.sockets_allow)?
    });
    rules.extend(socket_deny_rules());
    Ok(rules)
}

fn action_capability_rules(tmpdir: &Path) -> Result<[String; 2], Failure> {
    // Alternate state roots may live outside the home read boundary.
    let own_tmp = crate::sbpl::quote(&crate::sbpl::resolve(tmpdir)?)?;
    Ok([
        format!(
            r#"(deny file-read* file-write* file-link (require-all (regex #"/lane-mcp-capability-[^/]+$") (require-not (subpath {own_tmp}))))"#
        ),
        r#"(deny file-read* file-write* file-link (regex #"/action-mcp-binding$"))"#.to_owned(),
    ])
}

fn socket_deny_rules() -> Vec<String> {
    let mut rules = Vec::new();
    // Keep the short socket namespace denied even outside the protected state root.
    let pattern = r"/(lead-sessions/[^/]+/sock|cs-lead-[0-9]+-[a-f0-9]+|lead-mcp-[^/]+)(/|$)";
    for operation in [
        "file-read*",
        "file-write*",
        "network-bind",
        "network-outbound",
    ] {
        rules.push(format!(r#"(deny {operation} (regex #"{pattern}"))"#));
    }
    rules.push(r#"(deny file-write-unlink (regex #"/lead-sessions(/[^/]+)?$"))"#.to_owned());
    rules
}

fn check_scratch_rules(request: &ExecutorRequest) -> Result<Vec<String>, Failure> {
    let Some(path) = &request.check_scratch else {
        return Ok(Vec::new());
    };
    let scratch = path
        .canonicalize()
        .map_err(|error| unenforceable(format!("check scratch: {error}")))?;
    let workspace = crate::sbpl::resolve(&request.workspace)?;
    if !scratch.is_dir()
        || workspace.starts_with(&scratch)
        || scratch.starts_with(&workspace)
        || crate::sbpl::resolve(&request.home)?.starts_with(&scratch)
    {
        return Err(unenforceable("check scratch must be a separate directory"));
    }
    let scratch = crate::sbpl::quote(&scratch)?;
    Ok(vec![
        format!("(allow file-read* file-write* (subpath {scratch}))"),
        "(deny network-bind (local unix-socket))".to_owned(),
        format!("(allow network-bind (require-all (local unix-socket) (subpath {scratch})))"),
        format!("(allow network-outbound (require-all (remote unix-socket) (subpath {scratch})))"),
    ])
}

fn ssh_rules(request: &ExecutorRequest) -> Result<Vec<String>, Failure> {
    if request.version.is_some() {
        ssh_agent_deny::native_rules(&request.home, request.ssh_auth_sock.as_deref())
    } else {
        ssh_agent_deny::seatbelt_rules(&request.home)
    }
}

/// Login write exceptions mirroring `_cli_native.login_write_rules`.
fn login_write_rules(
    entries: &[LoginWriteEntry],
    real_home: &Path,
    v1: bool,
) -> Result<Vec<String>, Failure> {
    let home = resolve(real_home, v1)?;
    let mut rules = Vec::new();
    for entry in entries {
        let (kind, raw) = entry.filter()?;
        let Some((parent, name)) = raw[2..].rsplit_once('/') else {
            if v1 || raw == "~/" || raw.len() <= 2 {
                return Err(unenforceable("invalid login_writes path".to_owned()));
            }
            continue;
        };
        if (if v1 {
            parent.split('/').any(|part| part == "..")
        } else {
            parent.contains("..")
        }) || name.is_empty()
            || name.contains('/')
        {
            return Err(unenforceable("invalid login_writes path".to_owned()));
        }
        let directory = home.join(parent);
        match kind {
            "regex" => {
                let pattern = format!("^{}/{}$", escape(&directory.to_string_lossy(), v1), name);
                let escaped = pattern.replace('"', r#"\""#);
                rules.push(format!(r#"(allow file-write* (regex #"{escaped}"))"#));
                rules.push(format!(
                    r#"(deny file-write-create (require-all (vnode-type SYMLINK) (regex #"{escaped}")))"#
                ));
            }
            "literal" | "subpath" => {
                let path = directory.join(name);
                let key = path.to_string_lossy().into_owned();
                let quoted = path_quote(Path::new(&key), v1)?;
                rules.push(format!("(allow file-write* ({kind} {quoted}))"));
                rules.push(format!(
                    "(deny file-write-create (require-all (vnode-type SYMLINK) ({kind} {quoted})))"
                ));
            }
            _ => return Err(unenforceable("invalid login_writes filter".to_owned())),
        }
    }
    Ok(rules)
}

/// Goose keeps its logins as directories; those links keep their directory scope.
const DIR_LOGIN_LINKS: [&str; 3] = [
    ".config/goose/chatgpt_codex",
    ".config/goose/gemini_oauth",
    ".config/goose/xai_oauth",
];

/// Refresh may rewrite a login file in place, but must not replace its inode or
/// any parent entry (hardlink, rename or unlink swaps). Mirrors `_cli_native.py`
/// `login_write_rules` over `_login_targets`.
fn login_swap_rules(request: &ExecutorRequest) -> Result<Vec<String>, Failure> {
    let v1 = request.version.is_some();
    let home = resolve(&request.home, request.version.is_some())?;
    // The staged Keychain folder is a read grant, not a login file.
    let mut dir_links: Vec<PathBuf> = DIR_LOGIN_LINKS.iter().map(|rel| home.join(rel)).collect();
    dir_links.push(home.join("Library/Keychains"));
    let mut targets: Vec<PathBuf> = Vec::new();
    for link in request
        .read_links
        .iter()
        .filter(|_| request.version.is_none())
    {
        if link.is_absolute() && !dir_links.contains(link) && !targets.contains(link) {
            targets.push(link.clone());
        }
    }
    for entry in request
        .login_writes
        .iter()
        .filter(|_| request.version.is_none())
    {
        if let Some(raw) = entry
            .literal
            .as_deref()
            .and_then(|raw| raw.strip_prefix("~/"))
        {
            let path = home.join(raw);
            if !targets.contains(&path) {
                targets.push(path);
            }
        }
    }
    if request.version.is_some() {
        targets.extend(
            request
                .login_targets
                .iter()
                .filter(|target| !target.allow_directory)
                .map(|target| target.path.clone()),
        );
    }
    let mut rules = Vec::new();
    for target in targets {
        for entry in target.ancestors() {
            if entry == home || entry == Path::new("/") {
                break;
            }
            let key = entry.to_string_lossy().into_owned();
            let quoted = path_quote(Path::new(&key), v1)?;
            rules.push(format!(
                "(deny file-link file-write-create file-write-unlink (literal {quoted}))"
            ));
        }
    }
    // Rename sources cannot be restricted by Seatbelt. These logins are copied into
    // private run homes, and only the host may write a validated refresh back.
    for relative in [
        ".claude/.credentials.json",
        ".codex/auth.json",
        ".pi/agent/auth.json",
        ".grok/auth.json",
    ] {
        let target = home.join(relative);
        for entry in target.ancestors() {
            if entry == home || entry == Path::new("/") {
                break;
            }
            let quoted = path_quote(entry, v1)?;
            rules.push(format!("(deny file-write* file-link (literal {quoted}))"));
        }
    }
    Ok(rules)
}

fn protected_rules(protected: &Path, v1: bool) -> Result<Vec<String>, Failure> {
    let protected = resolve(protected, v1)?;
    let mut rules = vec![format!(
        "(deny file-write* (subpath {}))",
        path_quote(&protected, v1)?
    )];
    for parent in protected.ancestors().skip(usize::from(v1)) {
        if parent == Path::new("/") {
            break;
        }
        let key = parent.to_string_lossy().into_owned();
        rules.push(format!(
            "(deny file-write-unlink (literal {}))",
            path_quote(Path::new(&key), v1)?
        ));
    }
    Ok(rules)
}

/// The full Seatbelt text for one executor run.
pub fn seatbelt_profile(request: &ExecutorRequest) -> Result<String, Failure> {
    request.validate()?;
    if matches!(request.variant, Some(Variant::Muse | Variant::Adapter)) {
        return variants::muse_profile(request);
    }
    let workspace = request
        .workspace
        .canonicalize()
        .map_err(|error| unenforceable(format!("workspace: {error}")))?;
    let tmpdir = request
        .tmpdir
        .canonicalize()
        .map_err(|error| unenforceable(format!("tmpdir: {error}")))?;
    let mut lines = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        "(deny file-write*)".to_owned(),
        allow_write_subpath(&workspace)?,
        allow_write_subpath(&tmpdir)?,
        "(allow file-write* (subpath \"/private/tmp\"))".to_owned(),
        "(allow file-write* (subpath \"/tmp\"))".to_owned(),
        "(allow file-write* (subpath \"/dev\"))".to_owned(),
        "(allow file-write* (subpath \"/private/var/folders\"))".to_owned(),
        deny_write_subpath(&request.home.join(".claude"))?,
        deny_write_subpath(&request.home.join(".codex"))?,
        deny_write_subpath(&request.home.join(".gemini"))?,
        deny_write_subpath(&request.home.join(".cursor"))?,
        deny_write_subpath(&request.home.join("Library/Keychains"))?,
        FCNTL_DENY.to_owned(),
    ];
    // The run's private tmp (staged logins) stays readable under the estate deny.
    let mut read_roots = vec![workspace.clone(), request.tmpdir.clone()];
    read_roots.extend(request.read_roots.iter().cloned());
    lines.extend(read_rules(request, &read_roots)?);
    lines.extend(check_scratch_rules(request)?);
    if request
        .run_protection
        .as_ref()
        .is_some_and(|run| run.action_capabilities)
    {
        lines.extend(action_capability_rules(&tmpdir)?);
    }
    lines.extend(ssh_rules(request)?);
    if let Some(protected) = request.protected.as_ref() {
        lines.extend(protected_rules(protected, request.version.is_some())?);
    }
    lines.extend(login_write_rules(
        &request.login_writes,
        &request.home,
        request.version.is_some(),
    )?);
    lines.extend(login_swap_rules(request)?);
    if request.version.is_some() {
        lines.extend(toolchain_rules(request)?);
    }
    if let Some(lead) = &request.lead {
        lines.extend(variants::lead_suffix(request, lead)?);
    }
    Ok(format!("{}\n", lines.join("\n")))
}

#[path = "cli_executor_variants.rs"]
mod variants;

#[path = "cli_executor_typed.rs"]
mod typed;
use typed::{caller_rules, toolchain_rules};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn check_scratch_exception_follows_env_denies() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let root = root.path().canonicalize()?;
        let (workspace, home, scratch) = (root.join("ws"), root.join("home"), root.join("check"));
        let tmpdir = root.join("run-tmp");
        std::fs::create_dir(&workspace)?;
        std::fs::create_dir(&scratch)?;
        std::fs::create_dir(&tmpdir)?;
        let quoted = crate::sbpl::quote(&scratch).map_err(|error| error.message().to_owned())?;
        let rules = [
            format!("(allow file-read* file-write* (subpath {quoted}))"),
            "(deny network-bind (local unix-socket))".to_owned(),
            format!("(allow network-bind (require-all (local unix-socket) (subpath {quoted})))"),
            format!(
                "(allow network-outbound (require-all (remote unix-socket) (subpath {quoted})))"
            ),
        ];
        for variant in ["native", "muse"] {
            let mut request = serde_json::json!({
                "version": 1, "variant": variant, "workspace": &workspace,
                "tmpdir": &tmpdir, "home": &home
            });
            let render = |request| -> Result<String, Box<dyn std::error::Error>> {
                Ok(seatbelt_profile(&serde_json::from_value(request)?)
                    .map_err(|error| error.message().to_owned())?)
            };
            let agent = render(request.clone())?;
            request["check_scratch"] = serde_json::to_value(&scratch)?;
            let check = render(request)?;
            for name in [r"/\.env$", r"/[^/]+\.env$"] {
                let deny = format!(r#"(deny file-read* file-write* file-link (regex #"{name}"))"#);
                let offset = check.rfind(&deny).ok_or("missing global env deny")?;
                for rule in &rules {
                    assert!(
                        check[offset..].lines().any(|line| line == rule),
                        "{variant}: {rule}"
                    );
                }
            }
            let remaining: Vec<_> = check
                .lines()
                .filter(|line| !rules.iter().any(|rule| rule == line))
                .collect();
            assert_eq!(remaining, agent.lines().collect::<Vec<_>>(), "{variant}");
        }
        Ok(())
    }

    #[test]
    fn versioned_protocol_is_strict() -> Result<(), serde_json::Error> {
        let base = serde_json::json!({"workspace":"/workspace", "tmpdir":"/tmp/private", "home":"/owner",
            "version":1, "variant":"native", "lookup_path":[], "ssh_auth_sock":null,
            "toolchains":{"cargo_configured":false,"read_roots":[],"proxy_paths":[]}});
        let parsed: ExecutorRequest = serde_json::from_value(base.clone())?;
        assert!(parsed.validate().is_ok());
        for (key, value) in [
            ("version", serde_json::json!(2)),
            ("variant", serde_json::Value::Null),
            ("readonly_paths", serde_json::json!(["/readonly"])),
            ("adapter", serde_json::json!({})),
        ] {
            let mut invalid = base.clone();
            invalid[key] = value;
            let request: ExecutorRequest = serde_json::from_value(invalid)?;
            assert_eq!(
                request.validate().err().map(|e| e.code()),
                Some(101),
                "{key}"
            );
        }
        for (key, value) in [
            ("version", serde_json::Value::Null),
            ("unknown", serde_json::json!(true)),
            ("adapter", serde_json::json!({"unknown":true})),
        ] {
            let mut invalid = base.clone();
            invalid[key] = value;
            assert!(serde_json::from_value::<ExecutorRequest>(invalid).is_err());
        }
        assert!(serde_json::from_str::<ExecutorRequest>("{broken").is_err());
        Ok(())
    }

    #[test]
    fn home_subpath_rules_refuse_control_characters() {
        assert!(deny_write_subpath(Path::new("/Users/me\n/evil")).is_err());
    }

    #[test]
    fn login_write_entry_rejects_multiple_filters() {
        let entry = LoginWriteEntry {
            literal: Some("~/a".to_owned()),
            subpath: Some("~/b".to_owned()),
            regex: None,
        };
        assert!(entry.filter().is_err());
    }

    #[test]
    fn copied_logins_and_parents_stay_denied_after_refresh_grants() -> Result<(), serde_json::Error>
    {
        let request: ExecutorRequest = serde_json::from_value(serde_json::json!({
            "workspace": "/workspace",
            "tmpdir": "/private",
            "home": "/Users/fixture",
            "login_writes": [
                {"literal": "~/.codex/auth.json"},
                {"regex": "~/.codex/\\.auth-[A-Za-z0-9]{6}"},
                {"literal": "~/.pi/agent/auth.json"},
                {"subpath": "~/.pi/agent/auth.json.lock"}
            ]
        }))?;
        let rules = login_swap_rules(&request)
            .map_err(|e| <serde_json::Error as serde::de::Error>::custom(e.message()))?;
        let expected: Vec<String> = [
            ".codex/auth.json",
            ".codex",
            ".pi/agent/auth.json",
            ".pi/agent",
            ".pi",
            ".grok/auth.json",
            ".grok",
        ]
        .iter()
        .map(|relative| {
            format!("(deny file-write* file-link (literal \"/Users/fixture/{relative}\"))")
        })
        .collect();
        assert!(rules.ends_with(&expected));
        Ok(())
    }

    #[test]
    fn codex_login_regex_uses_anchored_home_directory() -> Result<(), Failure> {
        let home = PathBuf::from("/Users/test");
        let rules = login_write_rules(
            &[LoginWriteEntry {
                literal: None,
                subpath: None,
                regex: Some("~/.codex/\\.auth-[A-Za-z0-9]{6}".to_owned()),
            }],
            &home,
            false,
        )?;
        assert_eq!(rules.len(), 2);
        assert!(rules[0].starts_with("(allow file-write* (regex #\"^"));
        assert!(rules[0].contains(".codex"));
        assert!(
            rules[1].starts_with(
                "(deny file-write-create (require-all (vnode-type SYMLINK) (regex #\"^"
            )
        );
        Ok(())
    }
}

/// Python 3.11+ `re.escape` for `login_write` regex directory prefixes (`/` stays literal).
mod regex {
    pub fn escape(text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 8);
        for ch in text.chars() {
            if matches!(
                ch,
                '.' | '-'
                    | '\\'
                    | '^'
                    | '$'
                    | '*'
                    | '+'
                    | '?'
                    | '|'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '\n'
            ) {
                out.push('\\');
            }
            out.push(ch);
        }
        out
    }
}

fn resolve(path: &Path, v1: bool) -> Result<PathBuf, Failure> {
    if v1 {
        crate::sbpl::resolve(path)
    } else {
        Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
    }
}
fn escape(text: &str, v1: bool) -> String {
    if v1 {
        crate::sbpl::regex_escape(text)
    } else {
        regex::escape(text)
    }
}

#[cfg(test)]
#[path = "cli_executor_tests.rs"]
mod native_tests;

/// shlex.split(..., comments=False, posix=True), as used on Python launcher headers.
fn shebang_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => word.push(ch),
            (_, '\\') => {
                let next = chars.next()?;
                if quote == Some('"') && !['"', '\\'].contains(&next) {
                    word.push('\\');
                }
                word.push(next);
                started = true;
            }
            (None, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            (None, ' ' | '\t' | '\r' | '\n') => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

// Keep legacy JSON literal bytes (including escaped controls) at the protocol boundary.
// All v1 literals refuse control characters before emitting policy text.
fn path_quote(path: &Path, v1: bool) -> Result<String, Failure> {
    if v1 {
        crate::sbpl::quote(path)
    } else {
        serde_json::to_string(&path.to_string_lossy()).map_err(|e| unenforceable(e.to_string()))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}
