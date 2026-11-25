//! `_calllog.record`: one `{"args": ..., "name": ...}` line per call, appended to
//! `.cs-calls.jsonl` (or a batch parent's child log) in the workspace. Failures are silent.

use crate::jail;
use crate::pyjson;
use serde_json::{Map, Value};
use std::env;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

const LOG: &str = ".cs-calls.jsonl";
const CHILD_LOG: &str = ".cs-batch-child-calls.jsonl";

/// A direct batch parent may isolate composed child-call evidence in its own file.
fn destination() -> &'static str {
    let candidate = env::var("CS_BATCH_CHILD_CALLLOG").unwrap_or_default();
    let parent = env::var("CS_BATCH_PARENT_PID").unwrap_or_default();
    let ppid = std::os::unix::process::parent_id().to_string();
    if candidate == CHILD_LOG && parent == ppid {
        return CHILD_LOG;
    }
    LOG
}

/// The exact line Python writes: `json.dumps({"name", "args"}, sort_keys=True) + "\n"`.
pub fn line(name: &str, args: &Value) -> String {
    let mut record = Map::new();
    record.insert("name".to_owned(), Value::String(name.to_owned()));
    record.insert("args".to_owned(), args.clone());
    let mut text = pyjson::dumps_sorted(&Value::Object(record));
    text.push('\n');
    text
}

/// Append the record. Unlike the Python, a log that is a symlink leaving the workspace is
/// skipped rather than followed.
pub fn record(name: &str, args: &Value) {
    let dest = destination();
    if jail::link_leaves_workspace(Path::new(dest)) {
        return;
    }
    let Ok(mut file) = OpenOptions::new().append(true).create(true).open(dest) else {
        return;
    };
    let _ = file.write_all(line(name, args).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jail::CwdRestore;
    use serde_json::json;

    #[test]
    fn line_matches_fixture_bytes() {
        let args = json!({"path": "a.txt", "offset": 2, "limit": 1});
        assert_eq!(
            line("fs_read", &args),
            "{\"args\": {\"limit\": 1, \"offset\": 2, \"path\": \"a.txt\"}, \"name\": \"fs_read\"}\n"
        );
    }

    #[test]
    fn record_appends_jsonl_line_in_workspace() {
        use std::fs;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let _cwd = CwdRestore::chdir(dir.path());
        record("fs_read", &json!({"path": "a.txt"}));
        let text = fs::read_to_string(dir.path().join(".cs-calls.jsonl"))
            .unwrap_or_else(|e| panic!("read: {e}"));
        assert!(text.contains("fs_read"));
    }

    #[test]
    fn record_skips_unwritable_destination_without_panicking() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let _cwd = CwdRestore::chdir(dir.path());
        std::fs::create_dir(dir.path().join(LOG)).unwrap_or_else(|e| panic!("mkdir log: {e}"));
        record("fs_read", &json!({"path": "/nonexistent-root/nope.txt"}));

        assert!(dir.path().join(LOG).is_dir());
    }

    #[test]
    fn record_silently_skips_when_call_log_path_is_not_a_file() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let _cwd = CwdRestore::chdir(dir.path());
        std::fs::create_dir(dir.path().join(LOG)).unwrap_or_else(|e| panic!("mkdir log: {e}"));
        record("fs_read", &json!({"path": "a.txt"}));
    }
}
