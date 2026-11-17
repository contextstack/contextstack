//! Golden profiles: emit mode must match `_executor.py` `sandbox_cmd` for representative runs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

const CASES: &[&str] = &["claude-code", "codex", "cursor-agent", "repo-lane"];

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cs-sandbox"))
}

fn workspace_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent();
    root.map_or_else(
        || panic!("cs-sandbox manifest has no parent directory"),
        Path::to_path_buf,
    )
}

fn reference_script() -> PathBuf {
    workspace_root().join("sandbox/tests/support/executor_profile_reference.py")
}

fn run_reference(args: &[&str]) -> (String, String) {
    let output = Command::new("python3")
        .env("CS_SANDBOX_BIN", bin_path())
        .arg(reference_script())
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("python3 {args:?}: {error}"));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout =
        String::from_utf8(output.stdout).unwrap_or_else(|error| panic!("utf-8 stdout: {error}"));
    (stdout, stderr)
}

fn reference_bundle(case: &str, root: &Path) -> (String, String) {
    let root = root
        .to_str()
        .unwrap_or_else(|| panic!("non-utf8 golden root"));
    let (stdout, stderr) = run_reference(&["bundle", case, root]);
    assert!(
        stderr.is_empty(),
        "reference bundle {case} stderr: {stderr}"
    );
    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("bundle json: {error}"));
    let profile = value
        .get("profile_text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("bundle missing profile_text"))
        .to_owned();
    let mut request = value.clone();
    if let Some(map) = request.as_object_mut() {
        map.remove("profile_text");
    }
    let request =
        serde_json::to_string(&request).unwrap_or_else(|error| panic!("request json: {error}"));
    (profile, request)
}

fn emitted_profile(policy: &Path, request: &Path) -> String {
    let policy = policy
        .to_str()
        .unwrap_or_else(|| panic!("non-utf8 policy path"));
    let request = request
        .to_str()
        .unwrap_or_else(|| panic!("non-utf8 request path"));
    let output = Command::new(bin_path())
        .args([
            "--policy",
            policy,
            "--profile",
            "cli-executor",
            "--request",
            request,
        ])
        .output()
        .unwrap_or_else(|error| panic!("cs-sandbox emit: {error}"));
    assert!(
        output.status.success(),
        "emit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap_or_else(|error| panic!("utf-8 profile: {error}"))
}

#[test]
fn emit_matches_sandbox_cmd_for_representative_templates() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let policy = workspace_root().join("cs/policy/sandbox.json");
    let scratch = TempDir::new().unwrap_or_else(|error| panic!("tempdir: {error}"));
    for case in CASES {
        let case_root = scratch.path().join(case);
        fs::create_dir_all(&case_root).unwrap_or_else(|error| panic!("mkdir: {error}"));
        let request_path = case_root.join("request.json");
        let (reference, request_body) = reference_bundle(case, &case_root);
        fs::write(&request_path, request_body)
            .unwrap_or_else(|error| panic!("write request: {error}"));
        let emitted = emitted_profile(&policy, &request_path);
        assert!(emitted.contains(r#"(deny file-read* file-write* file-link (regex #"/\.env$"))"#));
        assert_eq!(
            emitted, reference,
            "cli-executor profile mismatch for {case}"
        );
    }
}
