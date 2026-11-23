//! Exec the built `cs-tools` binary as the runner does (cwd the workspace, JSON on stdin) on
//! fixture-like inputs, both install forms, and the symlink refusals the Python tools lack; and
//! the `yaml-json` ops helper, which takes a file argument instead.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn bin_path() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_cs-tools").into()
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn exec_with_env(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    stdin: &str,
    env: &[(&str, &str)],
    unset: &[&str],
) -> Out {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    for key in unset {
        cmd.env_remove(key);
    }
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", program.display()));
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin.as_bytes())
            .unwrap_or_else(|e| panic!("stdin: {e}"));
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn exec(program: &Path, args: &[&str], cwd: &Path, stdin: &str) -> Out {
    exec_with_env(program, args, cwd, stdin, &[], &[])
}

fn tool(name: &str, ws: &Path, stdin: &str) -> Out {
    exec(bin_path().as_path(), &[name], ws, stdin)
}

/// The fixtures' standard workspace: a.txt and sub/b.txt.
fn workspace() -> TempDir {
    let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap_or_else(|e| panic!("{e}"));
    fs::create_dir(dir.path().join("sub")).unwrap_or_else(|e| panic!("{e}"));
    fs::write(dir.path().join("sub/b.txt"), "b\n").unwrap_or_else(|e| panic!("{e}"));
    dir
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn fs_read_file_and_call_log() {
    let ws = workspace();
    let out = tool("fs_read", ws.path(), r#"{"path": "a.txt"}"#);
    assert_eq!((out.code, out.stdout.as_str()), (0, "alpha\nbeta\ngamma\n"));
    assert_eq!(
        read(&ws.path().join(".cs-calls.jsonl")),
        "{\"args\": {\"path\": \"a.txt\"}, \"name\": \"fs_read\"}\n"
    );
}

#[test]
fn fs_read_appends_batch_child_call_log_when_parent_pid_matches() {
    let ws = workspace();
    let parent = std::process::id().to_string();
    let out = exec_with_env(
        bin_path().as_path(),
        &["fs_read"],
        ws.path(),
        r#"{"path": "a.txt"}"#,
        &[
            ("CS_BATCH_CHILD_CALLLOG", ".cs-batch-child-calls.jsonl"),
            ("CS_BATCH_PARENT_PID", parent.as_str()),
        ],
        &[],
    );
    assert_eq!((out.code, out.stdout.as_str()), (0, "alpha\nbeta\ngamma\n"));
    assert_eq!(
        read(&ws.path().join(".cs-batch-child-calls.jsonl")),
        "{\"args\": {\"path\": \"a.txt\"}, \"name\": \"fs_read\"}\n"
    );
    assert!(!ws.path().join(".cs-calls.jsonl").exists());
}

#[test]
fn fs_read_skips_call_log_when_default_path_is_a_directory() {
    let ws = workspace();
    fs::create_dir(ws.path().join(".cs-calls.jsonl")).unwrap_or_else(|e| panic!("{e}"));
    let out = tool("fs_read", ws.path(), r#"{"path": "a.txt"}"#);
    assert_eq!((out.code, out.stdout.as_str()), (0, "alpha\nbeta\ngamma\n"));
}

#[test]
fn okf_reports_missing_home_when_root_expands_tilde() {
    let ws = workspace();
    let out = exec_with_env(
        bin_path().as_path(),
        &["okf"],
        ws.path(),
        r#"{"action": "lint", "root": "~"}"#,
        &[],
        &["HOME"],
    );
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("couldn't find HOME environment"));
}

#[test]
fn fs_read_window_and_listing() {
    let ws = workspace();
    let out = tool(
        "fs_read",
        ws.path(),
        r#"{"path": "a.txt", "offset": 2, "limit": 1}"#,
    );
    assert_eq!(out.stdout, "lines 2-2 of 4 — re-read with offset=3\n\nbeta");
    let out = tool("fs_read", ws.path(), r#"{"path": "."}"#);
    assert_eq!(out.stdout, ".cs-calls.jsonl\na.txt\nsub/\n");
}

#[test]
fn fs_read_errors_exit_one_on_stderr() {
    let ws = workspace();
    let out = tool("fs_read", ws.path(), r#"{"path": "nope.txt"}"#);
    assert_eq!(
        (out.code, out.stderr.as_str()),
        (
            1,
            "not a file: nope.txt (path does not exist). Use a path relative to the workspace.\n"
        )
    );
    let out = tool("fs_read", ws.path(), r#"{"path": "../x"}"#);
    assert_eq!(
        (out.code, out.stderr.as_str()),
        (
            1,
            "path escapes workspace: ../x. Use a path relative to the workspace.\n"
        )
    );
    let out = tool("fs_read", ws.path(), r#"{"path": "a.txt", "offset": 9}"#);
    assert_eq!(
        out.stderr,
        "Offset 9 is beyond end of file (4 lines total). Path \"a.txt\" exists.\n"
    );
}

#[test]
fn fs_edit_replace_writes_file_and_prints_diff() {
    let ws = workspace();
    let args = r#"{"path": "a.txt", "edits": [{"oldText": "beta", "newText": "BETA"}]}"#;
    let out = tool("fs_edit", ws.path(), args);
    assert_eq!(out.code, 0);
    assert_eq!(
        out.stdout,
        "--- a.txt\n+++ a.txt\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"
    );
    assert_eq!(read(&ws.path().join("a.txt")), "alpha\nBETA\ngamma\n");
}

#[test]
fn fs_edit_refusals_leave_the_file_alone() {
    let ws = workspace();
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "a.txt", "edits": [{"oldText": "zzz", "newText": "y"}]}"#,
    );
    assert_eq!(
        out.stderr,
        "Could not find edits[0] in a.txt. The oldText must match exactly including all whitespace and newlines.\n"
    );
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "a.txt", "edits": [{"oldText": "a", "newText": "b"}]}"#,
    );
    assert_eq!(
        out.stderr,
        "found 5 occurrences of edits[0] in a.txt. Each oldText must be unique.\n"
    );
    assert_eq!(read(&ws.path().join("a.txt")), "alpha\nbeta\ngamma\n");
}

#[test]
fn symlinks_out_of_the_workspace_are_refused() {
    let outside = TempDir::new().unwrap_or_else(|e| panic!("{e}"));
    fs::write(outside.path().join("secret.txt"), "SECRET\n").unwrap_or_else(|e| panic!("{e}"));
    let ws = workspace();
    let link = |target: &Path, name: &str| {
        symlink(target, ws.path().join(name)).unwrap_or_else(|e| panic!("{e}"));
    };
    link(&outside.path().join("secret.txt"), "link.txt");
    link(outside.path(), "linkdir");
    link(&outside.path().join("calls.jsonl"), ".cs-calls.jsonl");
    link(&ws.path().join("a.txt"), "inner.txt");
    for path in [
        "link.txt",
        "linkdir",
        "linkdir/secret.txt",
        ".cs-calls.jsonl",
    ] {
        let out = tool("fs_read", ws.path(), &format!(r#"{{"path": "{path}"}}"#));
        assert_eq!(
            (out.code, out.stderr),
            (
                1,
                format!("path escapes workspace: {path}. Use a path relative to the workspace.\n")
            )
        );
    }
    let args = r#"{"path": "link.txt", "edits": [{"oldText": "SECRET", "newText": "x"}]}"#;
    assert_eq!(tool("fs_edit", ws.path(), args).code, 1);
    assert_eq!(read(&outside.path().join("secret.txt")), "SECRET\n");
    let out = tool("fs_read", ws.path(), r#"{"path": "missing"}"#);
    assert_eq!(out.code, 1);
    assert!(out.stderr.starts_with("not a file: missing"));
    assert!(!outside.path().join("calls.jsonl").exists());
    let out = tool("fs_read", ws.path(), r#"{"path": "inner.txt"}"#);
    assert_eq!((out.code, out.stdout.as_str()), (0, "alpha\nbeta\ngamma\n"));
}

#[test]
fn argv0_symlink_dispatch_and_usage() {
    let bin_dir = TempDir::new().unwrap_or_else(|e| panic!("{e}"));
    let as_read = bin_dir.path().join("fs_read");
    symlink(bin_path(), &as_read).unwrap_or_else(|e| panic!("{e}"));
    let ws = workspace();
    let out = exec(&as_read, &[], ws.path(), r#"{"path": "sub/b.txt"}"#);
    assert_eq!((out.code, out.stdout.as_str()), (0, "b\n"));
    let out = exec(bin_path().as_path(), &["nope"], ws.path(), "{}");
    assert_eq!(out.code, 2);
    assert!(out.stderr.starts_with("usage: cs-tools"));
}

/// The okf fixtures' corpus at version `v`: index.md, one.md, two.md.
fn corpus(v: &str) -> TempDir {
    let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let root = dir.path().join("corpus");
    fs::create_dir(&root).unwrap_or_else(|e| panic!("{e}"));
    let files = [
        (
            "index.md",
            format!("---\nokf_version: \"{v}\"\n---\n\n# corpus\n"),
        ),
        (
            "one.md",
            format!(
                "---\ntype: \"Legacy Document\"\nschema_version: \"{v}\"\ntitle: One\n---\n\nBody one.\n"
            ),
        ),
        (
            "two.md",
            format!(
                "---\ntype: \"Legacy Document\"\nschema_version: \"{v}\"\n---\n\nNo frontmatter here.\n"
            ),
        ),
    ];
    for (name, text) in files {
        fs::write(root.join(name), text).unwrap_or_else(|e| panic!("{e}"));
    }
    dir
}

#[test]
fn okf_migrates_lints_and_writes_no_call_log() {
    let ws = corpus("0.1");
    let out = tool(
        "okf",
        ws.path(),
        r#"{"action": "migrate", "root": "corpus"}"#,
    );
    let want =
        "{\"ok\":true,\"action\":\"migrate\",\"from\":\"0.1\",\"to\":\"0.2\",\"changed\":2}\n";
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, want, "")
    );
    let one = read(&ws.path().join("corpus/one.md"));
    assert_eq!(
        one,
        "---\ntype: \"Legacy Document\"\nschema_version: \"0.2\"\ntitle: One\n---\n\nBody one.\n"
    );
    let out = tool("okf", ws.path(), r#"{"action": "lint", "root": "corpus"}"#);
    assert_eq!(out.code, 0);
    assert!(out.stdout.starts_with("{\"ok\":true,\"action\":\"lint\""));
    assert!(!ws.path().join(".cs-calls.jsonl").exists());
}

#[test]
fn okf_stale_and_refusals_on_stderr() {
    let ws = corpus("0.2");
    let stale = "---\nschema_version: \"0.2\"\ntype: Note\nstale_after: \"2026-01-01T00:00:00Z\"\n---\n\nStale body.\n";
    fs::write(ws.path().join("corpus/stale.md"), stale).unwrap_or_else(|e| panic!("{e}"));
    let out = tool(
        "okf",
        ws.path(),
        r#"{"action": "stale", "root": "corpus", "now": "2026-01-02T00:00:00Z"}"#,
    );
    let want = "{\"ok\":true,\"action\":\"stale\",\"now\":\"2026-01-02T00:00:00Z\",\"stale\":[{\"path\":\"stale.md\",\"stale_after\":\"2026-01-01T00:00:00Z\",\"overdue_seconds\":86400}]}\n";
    assert_eq!((out.code, out.stdout.as_str()), (0, want));
    for (args, error) in [
        (
            r#"{"action": "nuke"}"#,
            "action must be stamp, migrate, lint, or stale",
        ),
        (
            r#"{"action": "lint", "root": "../.."}"#,
            "root escapes workspace",
        ),
        (
            r#"{"action": "stale", "root": "corpus", "now": "yesterday"}"#,
            "now must be ISO 8601 with an offset",
        ),
    ] {
        let out = tool("okf", ws.path(), args);
        let line = format!("{{\"ok\":false,\"error\":\"{error}\"}}\n");
        assert_eq!((out.code, out.stdout.as_str(), out.stderr), (1, "", line));
    }
}

#[test]
fn okf_refuses_a_corpus_file_linking_out() {
    let outside = TempDir::new().unwrap_or_else(|e| panic!("{e}"));
    fs::write(outside.path().join("doc.md"), "# doc\n").unwrap_or_else(|e| panic!("{e}"));
    let ws = corpus("0.1");
    symlink(
        outside.path().join("doc.md"),
        ws.path().join("corpus/link.md"),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let out = tool("okf", ws.path(), r#"{"action": "stamp", "root": "corpus"}"#);
    let line = "{\"ok\":false,\"error\":\"path escapes workspace: link.md\"}\n";
    assert_eq!((out.code, out.stderr.as_str()), (1, line));
    assert_eq!(read(&outside.path().join("doc.md")), "# doc\n");
    let bin_dir = TempDir::new().unwrap_or_else(|e| panic!("{e}"));
    let as_okf = bin_dir.path().join("okf");
    symlink(bin_path(), &as_okf).unwrap_or_else(|e| panic!("{e}"));
    fs::remove_file(ws.path().join("corpus/link.md")).unwrap_or_else(|e| panic!("{e}"));
    let out = exec(
        &as_okf,
        &[],
        ws.path(),
        r#"{"action": "lint", "root": "corpus", "version": "0.1"}"#,
    );
    assert_eq!(out.code, 0);
}

#[test]
fn yaml_json_both_forms_and_rubys_refusals() {
    let ws = workspace();
    fs::write(
        ws.path().join("t.yaml"),
        "a: [1, 2.5, yes, ~]\nb: {c: 'd'}\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    fs::write(ws.path().join("d.yaml"), "expiry: 2026-11-09\n").unwrap_or_else(|e| panic!("{e}"));
    let as_yaml_json = ws.path().join("yaml-json");
    symlink(bin_path(), &as_yaml_json).unwrap_or_else(|e| panic!("{e}"));
    let want = "{\"a\":[1,2.5,true,null],\"b\":{\"c\":\"d\"}}\n";
    // No stdin is read: an open, empty pipe changes nothing.
    for out in [
        exec(
            bin_path().as_path(),
            &["yaml-json", "t.yaml"],
            ws.path(),
            "",
        ),
        exec(&as_yaml_json, &["t.yaml"], ws.path(), ""),
    ] {
        assert_eq!(
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            (0, want, "")
        );
    }
    let refused = exec(
        bin_path().as_path(),
        &["yaml-json", "d.yaml"],
        ws.path(),
        "",
    );
    assert_eq!(refused.code, 1);
    assert_eq!(refused.stdout, "");
    assert_eq!(
        refused.stderr,
        "yaml-json: Tried to load unspecified class: Date (Psych::DisallowedClass)\n"
    );
    let missing = exec(&as_yaml_json, &["none.yaml"], ws.path(), "");
    assert_eq!(
        (missing.code, missing.stderr.as_str()),
        (
            1,
            "yaml-json: No such file or directory @ rb_sysopen - none.yaml (Errno::ENOENT)\n"
        )
    );
    // Ruby's `File.read(nil)` when no file is named.
    let no_arg = exec(&as_yaml_json, &[], ws.path(), "");
    assert_eq!(
        no_arg.stderr,
        "yaml-json: no implicit conversion of nil into String (TypeError)\n"
    );
}

#[test]
fn fs_read_empty_directory_and_line_size_cap() {
    let ws = workspace();
    std::fs::create_dir(ws.path().join("empty")).unwrap_or_else(|e| panic!("{e}"));
    let out = tool("fs_read", ws.path(), r#"{"path": "empty"}"#);
    assert_eq!(out.stdout, "(empty directory)\n");
    let huge = "x".repeat(50 * 1024 + 1);
    std::fs::write(ws.path().join("big.txt"), format!("{huge}\n"))
        .unwrap_or_else(|e| panic!("{e}"));
    let out = tool("fs_read", ws.path(), r#"{"path": "big.txt"}"#);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("line exceeds 50KB"));
}

#[test]
fn fs_edit_rejects_empty_oldtext_and_missing_file() {
    let ws = workspace();
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "a.txt", "edits": [{"oldText": "", "newText": "x"}]}"#,
    );
    assert!(out.stderr.contains("oldText must not be empty"));
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "missing.txt", "edits": [{"oldText": "a", "newText": "b"}]}"#,
    );
    assert_eq!(out.stderr, "not a file: missing.txt\n");
}

#[test]
fn okf_stamp_lints_a_minimal_corpus() {
    let ws = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let root = ws.path().join("corpus");
    fs::create_dir(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
    fs::write(
        root.join("index.md"),
        "---\nokf_version: \"0.2\"\n---\n\n# corpus\n",
    )
    .unwrap_or_else(|e| panic!("write: {e}"));
    fs::write(root.join("bare.md"), "# bare\n").unwrap_or_else(|e| panic!("write: {e}"));
    let stamp = tool(
        "okf",
        ws.path(),
        r#"{"action":"stamp","root":"corpus","version":"0.2"}"#,
    );
    assert_eq!(stamp.code, 0);
    assert!(stamp.stdout.contains("\"action\":\"stamp\""));
    let lint = tool(
        "okf",
        ws.path(),
        r#"{"action":"lint","root":"corpus","version":"0.2"}"#,
    );
    assert_eq!(lint.code, 0);
    assert!(lint.stdout.contains("\"ok\":true"));
}

#[test]
fn fs_edit_preserves_crlf_after_replace() {
    let ws = workspace();
    fs::write(ws.path().join("crlf.txt"), "a\r\nb\r\n").unwrap_or_else(|e| panic!("write: {e}"));
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "crlf.txt", "edits": [{"oldText": "a", "newText": "A"}]}"#,
    );
    assert_eq!(out.code, 0);
    let content = read(&ws.path().join("crlf.txt"));
    assert!(content.starts_with("A\r\n"));
}

#[test]
fn fs_read_lists_subdirectory_with_trailing_slash() {
    let ws = workspace();
    fs::create_dir(ws.path().join("sub2")).unwrap_or_else(|e| panic!("mkdir: {e}"));
    fs::write(ws.path().join("sub2/note.txt"), "n\n").unwrap_or_else(|e| panic!("write: {e}"));
    let out = tool("fs_read", ws.path(), r#"{"path": "."}"#);
    assert_eq!(out.code, 0);
    assert!(out.stdout.contains("sub2/\n") || out.stdout.contains("sub/\n"));
}

#[test]
fn fs_read_truncation_header_suggests_next_offset() {
    let ws = workspace();
    let mut lines = String::new();
    for i in 0..80 {
        use std::fmt::Write as _;
        writeln!(lines, "line{i}").unwrap_or_else(|e| panic!("write line: {e}"));
    }
    fs::write(ws.path().join("many.txt"), lines).unwrap_or_else(|e| panic!("write: {e}"));
    let out = tool(
        "fs_read",
        ws.path(),
        r#"{"path": "many.txt", "offset": 1, "limit": 5}"#,
    );
    assert_eq!(out.code, 0);
    assert!(out.stdout.contains("lines 1-5 of"));
    assert!(out.stdout.contains("offset=6"));
}

#[test]
fn fs_edit_rejects_identical_replacement_content() {
    let ws = workspace();
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "a.txt", "edits": [{"oldText": "alpha", "newText": "alpha"}]}"#,
    );
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("No changes made"));
}

#[test]
fn fs_edit_strips_utf8_bom_when_writing() {
    let ws = workspace();
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(b"old\n");
    fs::write(ws.path().join("bom.txt"), bom).unwrap_or_else(|e| panic!("write: {e}"));
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "bom.txt", "edits": [{"oldText": "old", "newText": "new"}]}"#,
    );
    assert_eq!(out.code, 0);
    let bytes = fs::read(ws.path().join("bom.txt")).unwrap_or_else(|e| panic!("read: {e}"));
    assert!(bytes.starts_with(b"\xef\xbb\xbfnew"));
}

#[test]
fn fs_read_negative_limit_reads_tail_line() {
    let ws = workspace();
    let out = tool("fs_read", ws.path(), r#"{"path": "a.txt", "limit": -1}"#);
    assert_eq!(out.code, 0);
    assert!(out.stdout.contains("gamma"));
}

#[test]
fn fs_edit_accepts_edits_as_json_string() {
    let ws = workspace();
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "a.txt", "edits": "[{\"oldText\": \"beta\", \"newText\": \"BETA\"}]"}"#,
    );
    assert_eq!(out.code, 0);
    assert!(read(&ws.path().join("a.txt")).contains("BETA"));
}

#[test]
fn fs_edit_accepts_legacy_toplevel_oldtext_fields() {
    let ws = workspace();
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "a.txt", "oldText": "gamma", "newText": "GAMMA", "edits": []}"#,
    );
    assert_eq!(out.code, 0);
    assert!(read(&ws.path().join("a.txt")).contains("GAMMA"));
}

#[test]
fn bare_binary_without_tool_name_exits_two_with_usage() {
    let ws = workspace();
    let out = exec(bin_path().as_path(), &[], ws.path(), "");
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("usage:"));
}

#[test]
fn fs_edit_rejects_empty_edits_array_without_touching_file() {
    let ws = workspace();
    let before = read(&ws.path().join("a.txt"));
    let out = tool("fs_edit", ws.path(), r#"{"path": "a.txt", "edits": []}"#);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("edits must contain at least one replacement")
    );
    assert_eq!(read(&ws.path().join("a.txt")), before);
}

#[test]
fn fs_edit_rejects_overlapping_replacements_in_one_request() {
    let ws = workspace();
    std::fs::write(ws.path().join("overlap.txt"), "abcdef")
        .unwrap_or_else(|e| panic!("write: {e}"));
    let before = read(&ws.path().join("overlap.txt"));
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "overlap.txt", "edits": [{"oldText": "cde", "newText": "X"}, {"oldText": "bc", "newText": "Y"}]}"#,
    );
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("overlap"));
    assert_eq!(read(&ws.path().join("overlap.txt")), before);
}

#[test]
fn fs_read_reports_unlistable_directory_as_exit_one() {
    let ws = workspace();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = ws.path().join("locked");
        std::fs::create_dir(&locked).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let mut perms = std::fs::metadata(&locked)
            .unwrap_or_else(|e| panic!("meta: {e}"))
            .permissions();
        perms.set_mode(0o0);
        std::fs::set_permissions(&locked, perms).unwrap_or_else(|e| panic!("chmod: {e}"));
        let out = tool("fs_read", ws.path(), r#"{"path": "locked"}"#);
        assert_eq!(out.code, 1);
        assert!(out.stderr.contains("cannot list directory"));
    }
}

#[test]
fn fs_edit_rejects_edit_missing_oldtext_or_newtext() {
    let ws = workspace();
    let out = tool(
        "fs_edit",
        ws.path(),
        r#"{"path": "a.txt", "edits": [{"oldText": "beta"}]}"#,
    );
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("oldText and newText"));
}

#[test]
fn fs_read_lists_subdirectory_by_path_without_trailing_slash() {
    let ws = workspace();
    let out = tool("fs_read", ws.path(), r#"{"path": "sub"}"#);
    assert_eq!(out.code, 0);
    assert!(out.stdout.contains("b.txt"));
}

#[test]
fn fs_read_rejects_invalid_json_on_stdin() {
    let ws = workspace();
    let out = tool("fs_read", ws.path(), "{not-json");
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("JSON::ParserError") || out.stderr.contains("json.decoder"));
}

#[test]
fn fs_edit_rejects_non_object_json_args() {
    let ws = workspace();
    let out = tool("fs_edit", ws.path(), "[]");
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("AttributeError") || out.stderr.contains("get"));
}

#[test]
fn okf_lint_accepts_colon_offset_on_stale_after() {
    let ws = corpus("0.2");
    std::fs::write(
        ws.path().join("corpus/doc.md"),
        "---\nschema_version: \"0.2\"\ntype: \"Legacy Document\"\nstale_after: \"2027-06-01T00:00:00+00:00\"\n---\n\nBody.\n",
    )
    .unwrap_or_else(|e| panic!("write: {e}"));
    let out = tool(
        "okf",
        ws.path(),
        r#"{"action":"lint","root":"corpus","version":"0.2"}"#,
    );
    assert_eq!(out.code, 0);
}

#[test]
fn in_workspace_file_symlinks_are_safely_read_and_edited() {
    let ws = workspace();
    fs::write(ws.path().join("target.txt"), "before\n").unwrap_or_else(|e| panic!("{e}"));
    symlink("target.txt", ws.path().join("alias.txt")).unwrap_or_else(|e| panic!("{e}"));
    let read = tool("fs_read", ws.path(), r#"{"path":"alias.txt"}"#);
    assert_eq!((read.code, read.stdout.as_str()), (0, "before\n"));
    let edit = tool(
        "fs_edit",
        ws.path(),
        r#"{"path":"alias.txt","oldText":"before","newText":"after"}"#,
    );
    assert_eq!(edit.code, 0, "{}", edit.stderr);
    assert_eq!(
        fs::read_to_string(ws.path().join("target.txt")).unwrap_or_else(|e| panic!("{e}")),
        "after\n"
    );
    assert!(
        fs::symlink_metadata(ws.path().join("alias.txt"))
            .unwrap_or_else(|e| panic!("{e}"))
            .file_type()
            .is_symlink()
    );
}

#[test]
fn repo_search_confines_relative_and_absolute_paths_to_the_view() -> std::io::Result<()> {
    let workspace = TempDir::new()?;
    let view = TempDir::new()?;
    let docs = view.path().join("docs");
    fs::create_dir(&docs)?;
    fs::write(docs.join("facts.txt"), "snapshot fact\n")?;
    let outside = workspace.path().join("outside");
    fs::create_dir(&outside)?;
    symlink(&outside, view.path().join("link"))?;

    let view_env = view
        .path()
        .to_str()
        .ok_or(std::io::ErrorKind::InvalidData)?;
    let search = |path: &Path| {
        let args = serde_json::json!({"mode": "text", "query": "snapshot fact", "path": path});
        exec_with_env(
            &bin_path(),
            &["repo_search"],
            workspace.path(),
            &args.to_string(),
            &[("CS_REPO_VIEW", view_env)],
            &[],
        )
    };

    for path in [Path::new("docs"), &docs] {
        let out = search(path);
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stdout.ends_with("/docs/facts.txt:1: snapshot fact\n"));
    }
    for path in [
        outside,
        "../outside".into(),
        view.path().join("docs/../docs"),
        "link".into(),
        view.path().join("link"),
    ] {
        let out = search(&path);
        assert_eq!(out.code, 1, "{}", path.display());
        assert!(
            out.stderr.contains("outside the repository view"),
            "{}",
            out.stderr
        );
    }
    Ok(())
}

#[test]
fn absolute_write_refusals_explain_where_to_write() -> std::io::Result<()> {
    let ws = workspace();
    let repo = workspace();
    let root = repo.path().to_string_lossy();
    let path = repo.path().join("a.txt");
    let input = serde_json::json!({
        "path": path,
        "content": "changed",
        "oldText": "alpha",
        "newText": "changed"
    });
    let write = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cs/tools/bin/fs_write");
    let workspace = ws.path().canonicalize()?;
    for (program, args) in [(write, &[][..]), (bin_path(), &["fs_edit"][..])] {
        for repo_view in [false, true] {
            let env = [("CS_REPO_VIEW", root.as_ref())];
            let out = exec_with_env(
                &program,
                args,
                ws.path(),
                &input.to_string(),
                if repo_view { &env } else { &[] },
                if repo_view { &[] } else { &["CS_REPO_VIEW"] },
            );
            let repo_help = if repo_view {
                ". The repository view is read-only. Write deliverables in your workspace with a relative path, e.g. answer.md."
            } else {
                ""
            };
            assert_eq!(out.code, 1);
            assert_eq!(
                out.stderr,
                format!(
                    "path escapes workspace: {}{repo_help}\nYour workspace is {}; write there or use a relative path such as a.txt.\n",
                    path.display(),
                    workspace.display()
                )
            );
        }
    }
    assert_eq!(fs::read_to_string(path)?, "alpha\nbeta\ngamma\n");
    Ok(())
}
