//! `fs_edit`: exact, unique, non-overlapping text replacements against the original file,
//! answered with a unified diff.

use crate::difflib;
use crate::jail;
use crate::py::{self, Failure};
use serde_json::Value;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
const MAX_DIFF_BYTES: usize = 50 * 1024;

/// One requested replacement.
struct Edit {
    old: String,
    new: String,
}

/// Run the tool on decoded args; `Ok` is stdout (exit 0).
pub fn run(a: &Value) -> Result<Vec<u8>, Failure> {
    let (path_value, edits) = prepare(a)?;
    let p = py::fspath(path_value)?;
    if jail::lexically_escapes(p, None) || jail::resolves_outside(Path::new(p)) {
        return Err(workspace_escape(p));
    }
    let path = Path::new(p);
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Failure::exit(format!("not a file: {p}")));
        }
        Err(e) => return Err(py::os_error(&e, p)),
    };
    if meta.file_type().is_symlink() {
        if jail::link_leaves_workspace(path) {
            return Err(workspace_escape(p));
        }
    } else if !meta.is_file() {
        return Err(Failure::exit(format!("not a file: {p}")));
    }
    let workspace = std::env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|e| open_error(&e, p))?;
    let target = fs::canonicalize(workspace.join(path)).map_err(|e| py::os_error(&e, p))?;
    let file = jail::open_in_repo(&target, &workspace).map_err(|e| open_error(&e, p))?;
    let expected = file.metadata().map_err(|e| py::os_error(&e, p))?;
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| py::os_error(&e, p))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Failure::exit(format!(
            "{p} is {len} bytes; fs_edit refuses files larger than {MAX_FILE_BYTES} bytes",
            len = bytes.len()
        )));
    }
    let raw = String::from_utf8(bytes).map_err(|e| decode_error(e.as_bytes(), &e.utf8_error()))?;
    let (bom, body) = match raw.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", raw.as_str()),
    };
    let crlf = detect_crlf(body);
    let orig = to_lf(body);
    let new = apply(p, &orig, &edits)?;
    let lf_or_crlf = if crlf {
        new.replace('\n', "\r\n")
    } else {
        new.clone()
    };
    let written = format!("{bom}{lf_or_crlf}");
    if written.len() > MAX_FILE_BYTES {
        return Err(Failure::exit(
            "edited output exceeds fs_edit 10 MB size cap",
        ));
    }
    commit(&target, written.as_bytes(), &raw, &expected, || {}).map_err(|e| match e {
        Commit::Changed => {
            Failure::exit("file changed since it was read; not written; retry fs_edit")
        }
        Commit::Io(e) => open_error(&e, p),
    })?;
    let before = difflib::splitlines(&orig);
    let after = difflib::splitlines(&new);
    let mut diff = difflib::unified_diff(&before, &after, p, p, 3);
    if diff.len() > MAX_DIFF_BYTES {
        let mut end = MAX_DIFF_BYTES;
        while !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
        if !diff.ends_with('\n') {
            diff.push('\n');
        }
        diff.push_str("diff truncated\n");
    }
    Ok(diff.into_bytes())
}

/// Why `commit` did not replace the file.
#[derive(Debug)]
enum Commit {
    /// The file on disk is no longer the one that was read (other inode, or other contents).
    Changed,
    /// Creating, writing, syncing or renaming the replacement failed; the original is intact.
    Io(std::io::Error),
}

impl From<std::io::Error> for Commit {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Replace the canonical regular file `target` with `contents` atomically: write a temp file in
/// the same directory with `target`'s permission bits, `fsync` it, recheck that `target` is still
/// the inode `read_meta` describes and still holds `read_bytes`, `rename` the temp over it, then
/// `fsync` the directory. A kill or a full disk before the rename leaves the original untouched
/// (the temp file is removed on every error path). `before_rename` runs just before the recheck
/// (a test seam for a concurrent writer).
fn commit(
    target: &Path,
    contents: &[u8],
    read_bytes: &str,
    read_meta: &fs::Metadata,
    before_rename: impl FnOnce(),
) -> Result<(), Commit> {
    let dir = target.parent().ok_or(Commit::Changed)?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".fs_edit.tmp")
        .tempfile_in(dir)?;
    tmp.write_all(contents)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(read_meta.mode() & 0o7777))?;
    tmp.as_file().sync_all()?;
    before_rename();
    let current = fs::symlink_metadata(target)?;
    if !current.is_file() || current.dev() != read_meta.dev() || current.ino() != read_meta.ino() {
        return Err(Commit::Changed);
    }
    let mut now = Vec::new();
    jail::open_in_repo(target, dir)?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut now)?;
    if now != read_bytes.as_bytes() {
        return Err(Commit::Changed);
    }
    tmp.persist(target).map_err(|e| Commit::Io(e.error))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn open_error(error: &std::io::Error, path: &str) -> Failure {
    if error.kind() == std::io::ErrorKind::PermissionDenied
        && error.to_string() == jail::WORKSPACE_OPEN_DENIED
    {
        return workspace_escape(path);
    }
    py::os_error(error, path)
}

fn workspace_escape(path: &str) -> Failure {
    let mut error = format!("path escapes workspace: {path}");
    if jail::repo_view().is_some_and(|root| {
        Path::new(path).starts_with(&root)
            || fs::canonicalize(path).is_ok_and(|target| target.starts_with(&root))
    }) {
        error.push_str(". The repository view is read-only. Write deliverables in your workspace with a relative path, e.g. answer.md.");
    }
    if let Ok(workspace) = std::env::current_dir() {
        let basename = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("answer.md");
        error = format!(
            "{error}\nYour workspace is {}; write there or use a relative path such as {basename}.",
            workspace.display()
        );
    }
    Failure::exit(error)
}

fn is_single(v: &Value) -> bool {
    v.get("oldText").is_some_and(Value::is_string) && v.get("newText").is_some_and(Value::is_string)
}

/// Normalise the accepted argument shapes to a list of edits, in Python's order of checks.
fn prepare(a: &Value) -> Result<(&Value, Vec<Edit>), Failure> {
    if a.to_string().len() > MAX_FILE_BYTES {
        return Err(Failure::exit("edit input exceeds fs_edit 10 MB size cap"));
    }
    let mut edits = py::get(a, "edits")?.cloned().unwrap_or(Value::Null);
    if let Value::String(text) = &edits {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if parsed.is_array() {
                edits = parsed;
            } else if is_single(&parsed) {
                edits = Value::Array(vec![parsed]);
            }
        }
    } else if is_single(&edits) {
        edits = Value::Array(vec![edits]);
    }
    let old_text = py::get(a, "oldText")?.and_then(Value::as_str);
    let new_text = py::get(a, "newText")?.and_then(Value::as_str);
    if let (Some(old), Some(new)) = (old_text, new_text) {
        let mut list = match edits {
            Value::Array(items) => items,
            _ => Vec::new(),
        };
        let mut legacy = serde_json::Map::new();
        legacy.insert("oldText".to_owned(), Value::String(old.to_owned()));
        legacy.insert("newText".to_owned(), Value::String(new.to_owned()));
        list.push(Value::Object(legacy));
        edits = Value::Array(list);
    }
    let Value::Array(items) = edits else {
        return Err(Failure::exit("edits must contain at least one replacement"));
    };
    if items.is_empty() {
        return Err(Failure::exit("edits must contain at least one replacement"));
    }
    let mut out = Vec::with_capacity(items.len());
    for e in &items {
        let (Some(old), Some(new)) = (
            e.get("oldText").and_then(Value::as_str),
            e.get("newText").and_then(Value::as_str),
        ) else {
            return Err(Failure::exit("each edit needs oldText and newText"));
        };
        out.push(Edit {
            old: old.to_owned(),
            new: new.to_owned(),
        });
    }
    Ok((py::getitem(a, "path")?, out))
}

/// The file's dominant ending: CRLF when its first `\n` belongs to a `\r\n`.
fn detect_crlf(s: &str) -> bool {
    match (s.find("\r\n"), s.find('\n')) {
        (Some(crlf), Some(lf)) => crlf < lf,
        _ => false,
    }
}

fn to_lf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Match every edit against `orig`, reject empty, missing, ambiguous or overlapping ones, and
/// apply the rest.
fn apply(p: &str, orig: &str, edits: &[Edit]) -> Result<String, Failure> {
    let mut spans: Vec<(usize, usize, String, usize)> = Vec::with_capacity(edits.len());
    for (i, e) in edits.iter().enumerate() {
        let old = to_lf(&e.old);
        let new = to_lf(&e.new);
        if old.is_empty() {
            return Err(Failure::exit(format!("oldText must not be empty in {p}")));
        }
        let n = orig.matches(old.as_str()).count();
        if n == 0 {
            return Err(Failure::exit(format!(
                "Could not find edits[{i}] in {p}. The oldText must match exactly including all whitespace and newlines."
            )));
        }
        if n != 1 {
            return Err(Failure::exit(format!(
                "found {n} occurrences of edits[{i}] in {p}. Each oldText must be unique."
            )));
        }
        let idx = orig.find(old.as_str()).unwrap_or(0);
        spans.push((idx, idx + old.len(), new, i));
    }
    spans.sort();
    for pair in spans.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        if prev.1 > cur.0 {
            return Err(Failure::exit(format!(
                "edits[{}] and edits[{}] overlap in {p}. Merge them into one edit or target disjoint regions.",
                prev.3, cur.3
            )));
        }
    }
    let final_len = spans
        .iter()
        .try_fold(orig.len(), |len, (start, end, replacement, _)| {
            len.checked_sub(end - start)?.checked_add(replacement.len())
        });
    if final_len.is_none_or(|len| len > MAX_FILE_BYTES) {
        return Err(Failure::exit(
            "edited output exceeds fs_edit 10 MB size cap",
        ));
    }
    let mut new = orig.to_owned();
    for (start, end, replacement, _) in spans.iter().rev() {
        new.replace_range(*start..*end, replacement);
    }
    if new == orig {
        return Err(Failure::exit(format!(
            "No changes made to {p}. The replacements produced identical content."
        )));
    }
    Ok(new)
}

/// Python's strict UTF-8 `UnicodeDecodeError` line for a file read in one piece.
pub(crate) fn decode_error(bytes: &[u8], e: &std::str::Utf8Error) -> Failure {
    let start = e.valid_up_to();
    let lead = bytes.get(start).copied().unwrap_or(0);
    let (end, reason) = match e.error_len() {
        None => (bytes.len(), "unexpected end of data"),
        Some(_) if (0x80..=0xc1).contains(&lead) || lead >= 0xf5 => {
            (start + 1, "invalid start byte")
        }
        Some(len) => (start + len, "invalid continuation byte"),
    };
    let msg = if end - start == 1 {
        format!("'utf-8' codec can't decode byte 0x{lead:02x} in position {start}: {reason}")
    } else {
        format!(
            "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
            end - 1
        )
    };
    Failure::raise("UnicodeDecodeError", &msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jail::CwdRestore;
    use serde_json::json;

    fn edits(a: &Value) -> Result<Vec<(String, String)>, Failure> {
        prepare(a).map(|(_, list)| list.into_iter().map(|e| (e.old, e.new)).collect())
    }

    #[test]
    fn prepare_accepts_every_documented_shape() {
        let pair = Ok(vec![("a".to_owned(), "b".to_owned())]);
        assert_eq!(
            edits(&json!({"path": "p", "edits": [{"oldText": "a", "newText": "b"}]})),
            pair
        );
        assert_eq!(
            edits(&json!({"path": "p", "edits": {"oldText": "a", "newText": "b"}})),
            pair
        );
        assert_eq!(
            edits(&json!({"path": "p", "edits": "{\"oldText\":\"a\",\"newText\":\"b\"}"})),
            pair
        );
        assert_eq!(
            edits(&json!({"path": "p", "oldText": "a", "newText": "b"})),
            pair
        );
        assert_eq!(
            edits(&json!({"path": "p", "edits": "not json"})),
            Err(Failure::exit("edits must contain at least one replacement"))
        );
        assert_eq!(
            edits(&json!({"path": "p", "edits": [{"oldText": "a"}]})),
            Err(Failure::exit("each edit needs oldText and newText"))
        );
        assert_eq!(
            edits(&json!({"path": "p", "edits": []})),
            Err(Failure::exit("edits must contain at least one replacement"))
        );
        assert_eq!(
            edits(&json!([1])),
            Err(Failure::raise(
                "AttributeError",
                "'list' object has no attribute 'get'"
            ))
        );
    }

    #[test]
    fn apply_rejects_like_python() {
        let e = |old: &str, new: &str| Edit {
            old: old.to_owned(),
            new: new.to_owned(),
        };
        assert_eq!(
            apply("d.txt", "a\na\n", &[e("a", "b")]),
            Err(Failure::exit(
                "found 2 occurrences of edits[0] in d.txt. Each oldText must be unique."
            ))
        );
        assert_eq!(
            apply("f", "abcdef", &[e("cde", "X"), e("bc", "Y")]),
            Err(Failure::exit(
                "edits[1] and edits[0] overlap in f. Merge them into one edit or target disjoint regions."
            ))
        );
        assert_eq!(
            apply("f", "abc", &[e("b", "b")]),
            Err(Failure::exit(
                "No changes made to f. The replacements produced identical content."
            ))
        );
        assert_eq!(
            apply("f", "abcdef", &[e("ef", "2"), e("ab", "1")]),
            Ok("1cd2".to_owned())
        );
    }

    #[test]
    fn decode_error_reports_single_byte_invalid_start() {
        let bad = b"\xff";
        let Err(err) = String::from_utf8(bad.to_vec()) else {
            panic!("utf8");
        };
        let failure = decode_error(err.as_bytes(), &err.utf8_error());
        assert!(failure.0.contains("invalid start byte"));
        assert!(failure.0.contains("position 0"));
    }

    #[test]
    fn decode_error_reports_truncated_utf8_as_unexpected_end() {
        let bad = b"\xc2".to_vec();
        let Err(err) = String::from_utf8(bad) else {
            panic!("expected invalid utf8");
        };
        let failure = decode_error(err.as_bytes(), &err.utf8_error());
        assert!(failure.0.contains("unexpected end of data"));
    }

    #[test]
    fn decode_error_formats_byte_range_when_invalid_sequence_spans_multiple_bytes() {
        let bad = b"\xf0\x9f\x92".to_vec();
        let Err(err) = String::from_utf8(bad) else {
            panic!("expected invalid utf8");
        };
        let failure = decode_error(err.as_bytes(), &err.utf8_error());
        assert!(failure.0.contains("bytes in position"));
    }

    #[test]
    fn decode_error_reports_multibyte_invalid_continuation() {
        let bad = b"a\xc2\x41".to_vec();
        let Err(err) = String::from_utf8(bad) else {
            panic!("expected invalid utf8");
        };
        let failure = decode_error(err.as_bytes(), &err.utf8_error());
        assert!(failure.0.contains("invalid continuation byte"));
        assert!(failure.0.contains("position 1"));
    }

    fn read_back(path: &Path) -> (String, fs::Metadata) {
        let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read: {e}"));
        let meta = fs::metadata(path).unwrap_or_else(|e| panic!("meta: {e}"));
        (text, meta)
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("ls: {e}"))
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".fs_edit.tmp"))
            .collect()
    }

    #[test]
    fn commit_renames_a_new_inode_into_place_keeping_permissions() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = dir.path().canonicalize().unwrap_or_else(|e| panic!("{e}"));
        let target = root.join("a.txt");
        fs::write(&target, "old\n").unwrap_or_else(|e| panic!("{e}"));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640))
            .unwrap_or_else(|e| panic!("{e}"));
        let (raw, meta) = read_back(&target);
        commit(&target, b"new\n", &raw, &meta, || {}).unwrap_or_else(|e| panic!("{e:?}"));
        let (text, after) = read_back(&target);
        assert_eq!(text, "new\n");
        assert_ne!(
            after.ino(),
            meta.ino(),
            "expected a rename, not an in-place write"
        );
        assert_eq!(after.mode() & 0o7777, 0o640);
        assert_eq!(leftovers(&root), Vec::<String>::new());
    }

    #[test]
    fn commit_refuses_when_the_file_changed_after_it_was_read() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = dir.path().canonicalize().unwrap_or_else(|e| panic!("{e}"));
        let target = root.join("a.txt");
        fs::write(&target, "old\n").unwrap_or_else(|e| panic!("{e}"));
        let (raw, meta) = read_back(&target);
        // Same inode, other contents.
        let got = commit(&target, b"new\n", &raw, &meta, || {
            fs::write(&target, "theirs\n").unwrap_or_else(|e| panic!("{e}"));
        });
        assert!(matches!(got, Err(Commit::Changed)), "{got:?}");
        assert_eq!(read_back(&target).0, "theirs\n");
        // Replaced by another inode with the same contents.
        let (raw, meta) = read_back(&target);
        let replacement = root.join("replacement.txt");
        fs::write(&replacement, "theirs\n").unwrap_or_else(|e| panic!("{e}"));
        let got = commit(&target, b"new\n", &raw, &meta, || {
            fs::rename(&replacement, &target).unwrap_or_else(|e| panic!("{e}"));
        });
        assert!(matches!(got, Err(Commit::Changed)), "{got:?}");
        assert_eq!(read_back(&target).0, "theirs\n");
        assert_eq!(leftovers(&root), Vec::<String>::new());
    }

    #[test]
    fn commit_failure_leaves_the_original_intact() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = dir.path().canonicalize().unwrap_or_else(|e| panic!("{e}"));
        let target = root.join("a.txt");
        fs::write(&target, "old\n").unwrap_or_else(|e| panic!("{e}"));
        let (raw, meta) = read_back(&target);
        // The rename fails (the target became a directory); the temp file must not linger.
        let got = commit(&target, b"new\n", &raw, &meta, || {
            fs::remove_file(&target).unwrap_or_else(|e| panic!("{e}"));
            fs::create_dir(&target).unwrap_or_else(|e| panic!("{e}"));
        });
        assert!(got.is_err());
        assert!(target.is_dir());
        assert_eq!(leftovers(&root), Vec::<String>::new());
    }

    #[test]
    fn refuses_inputs_larger_than_cap() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("big.txt");
        let mut file = fs::File::create(&path).unwrap_or_else(|e| panic!("{e:?}"));
        let chunk = [b'a'; 8192];
        for _ in 0..=MAX_FILE_BYTES / chunk.len() {
            file.write_all(&chunk).unwrap_or_else(|e| panic!("{e:?}"));
        }
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        let _cwd = CwdRestore::chdir(&root);
        let err = run(&json!({"path": "big.txt", "edits": [{"oldText": "a", "newText": "b"}]}))
            .err()
            .unwrap_or_else(|| panic!("expected error"));
        assert!(err.0.contains("refuses files larger than"));
    }

    #[test]
    fn emitted_diff_is_capped() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("{e:?}"));
        let old = "old\n".repeat(10_000);
        let new = "🦀new\n".repeat(10_000);
        fs::write(dir.path().join("f"), &old).unwrap_or_else(|e| panic!("{e:?}"));
        let _cwd = CwdRestore::chdir(dir.path());
        let result = run(&json!({"path": "f", "oldText": old, "newText": new}));

        let diff = String::from_utf8(result.unwrap_or_else(|e| panic!("{e:?}")))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(diff.ends_with("diff truncated\n"));
        assert!(diff.len() <= MAX_DIFF_BYTES + 32);
        assert_eq!(
            fs::read_to_string(dir.path().join("f")).unwrap_or_else(|e| panic!("{e:?}")),
            new
        );
    }

    #[test]
    fn endings_and_decode_errors() {
        assert!(detect_crlf("a\r\nb\n"));
        assert!(!detect_crlf("a\nb\r\n"));
        assert!(!detect_crlf("a\rb"));
        assert_eq!(to_lf("a\r\nb\rc"), "a\nb\nc");
        let bad = b"ab\xffc".to_vec();
        let err = String::from_utf8(bad).err();
        let failure = err.map(|e| decode_error(e.as_bytes(), &e.utf8_error()));
        assert_eq!(
            failure,
            Some(Failure::raise(
                "UnicodeDecodeError",
                "'utf-8' codec can't decode byte 0xff in position 2: invalid start byte"
            ))
        );
    }
}
