//! Request transport and fail-closed protocol contract.
#![cfg(target_os = "macos")]
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn emit(policy: &Path, request: &str, body: &[u8]) -> Result<Output, Box<dyn Error>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cs-sandbox"))
        .arg("--policy")
        .arg(policy)
        .args(["--profile", "cli-executor", "--request", request])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("missing stdin")?;
    stdin.write_all(body)?;
    drop(stdin);
    Ok(child.wait_with_output()?)
}

#[test]
fn stdin_file_parity_and_refusals() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let policy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cs/policy/sandbox.json");
    let root = scratch.path();
    let home = root.join("home");
    let workspace = root.join("workspace");
    let tmpdir = root.join("private-tmp");
    for path in [&home, &workspace, &tmpdir] {
        fs::create_dir(path)?;
    }
    let legacy = serde_json::json!({"workspace":workspace,"tmpdir":tmpdir,"home":home});
    let mut versioned = legacy.clone();
    versioned["version"] = 1.into();
    versioned["variant"] = "native".into();
    versioned["lookup_path"] = serde_json::json!([home.join("bin")]);
    versioned["interpreters"] = serde_json::json!(["node"]);
    versioned["login_targets"] =
        serde_json::json!([{ "path": home.join("missing/auth"), "allow_directory": false }]);
    versioned["toolchains"] = serde_json::json!({"cargo_configured": true});
    versioned["ssh_auth_sock"] = serde_json::Value::Null;
    let mut muse = versioned.clone();
    muse["variant"] = "muse".into();
    muse["readonly_paths"] = serde_json::json!([workspace.join("protected")]);
    let mut adapter = muse.clone();
    adapter["variant"] = "adapter".into();
    adapter["adapter"] = serde_json::json!({"auth_mode":"none","localhost_port":"*"});
    let mut lead = versioned.clone();
    lead["lead"] = serde_json::json!({"write_roots":[workspace.join("runtime")],
        "own_session_roots":[workspace.join("runtime")],"bridge_sockets":[workspace.join("bridge.sock")]});
    for value in [&legacy, &versioned, &muse, &adapter, &lead] {
        let body = serde_json::to_vec(value)?;
        let file = root.join("request.json");
        fs::write(&file, &body)?;
        let stdin = emit(&policy, "-", &body)?;
        let from_file = emit(&policy, file.to_str().ok_or("non-UTF8 path")?, b"")?;
        assert_eq!(
            stdin.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&stdin.stderr)
        );
        assert_eq!(stdin.stdout, from_file.stdout);
        assert_eq!(stdin.stderr, b"");
    }
    for (field, value) in [
        ("unknown", serde_json::json!(true)),
        ("version", serde_json::json!(2)),
        ("version", serde_json::Value::Null),
        ("variant", serde_json::json!("unknown")),
        (
            "login_targets",
            serde_json::json!([{"path":"relative/login","allow_directory":false}]),
        ),
        (
            "toolchains",
            serde_json::json!({"read_roots":["relative/toolchain"]}),
        ),
        ("lane", serde_json::json!({"common_git_dir":"relative/git"})),
        ("adapter", serde_json::json!({})),
        ("adapter", serde_json::json!({"unknown":true})),
    ] {
        let mut invalid = versioned.clone();
        invalid[field] = value;
        let output = emit(&policy, "-", &serde_json::to_vec(&invalid)?)?;
        assert_eq!(output.status.code(), Some(101), "{invalid}");
        assert_eq!(output.stdout, b"");
        assert_ne!(output.stderr, b"");
    }
    for version in [None, Some(1)] {
        for extras in [
            serde_json::json!([]),
            serde_json::json!(["(allow default)"]),
        ] {
            let mut raw = versioned.clone();
            if version.is_none() {
                let fields = raw.as_object_mut().ok_or("request is not an object")?;
                fields.remove("version");
                fields.remove("variant");
            }
            raw["extra_rules"] = extras;
            let output = emit(&policy, "-", &serde_json::to_vec(&raw)?)?;
            assert_eq!(
                output.status.code(),
                Some(101),
                "raw extras accepted: {raw}"
            );
            assert_eq!(output.stdout, b"");
        }
    }
    let mut with_socket = versioned.clone();
    with_socket["ssh_auth_sock"] = serde_json::json!("/explicit-agent.sock");
    let output = emit(&policy, "-", &serde_json::to_vec(&with_socket)?)?;
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("/explicit-agent.sock"));
    Ok(())
}

#[test]
fn malformed_request_and_policy_refusal_emit_nothing() -> Result<(), Box<dyn Error>> {
    let scratch = tempfile::tempdir()?;
    let policy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cs/policy/sandbox.json");
    let root = scratch.path();
    let (home, workspace, tmpdir) = (
        root.join("home"),
        root.join("workspace"),
        root.join("private-tmp"),
    );
    for path in [&home, &workspace, &tmpdir] {
        fs::create_dir(path)?;
    }
    let versioned = serde_json::json!({"version":1,"variant":"native","workspace":workspace,
        "tmpdir":tmpdir,"home":home,"lookup_path":[],"ssh_auth_sock":null});
    let output = emit(&policy, "-", b"{broken")?;
    assert_eq!(output.status.code(), Some(101));
    assert_eq!(output.stdout, b"");
    let denied = root.join("policy.json");
    let mut host: serde_json::Value = serde_json::from_slice(&fs::read(&policy)?)?;
    host["profiles"]
        .as_object_mut()
        .ok_or("missing profiles")?
        .remove("cli-executor");
    fs::write(&denied, serde_json::to_vec(&host)?)?;
    let output = emit(&denied, "-", &serde_json::to_vec(&versioned)?)?;
    assert_eq!(output.status.code(), Some(101));
    assert_eq!(output.stdout, b"");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cli-executor"));
    Ok(())
}
