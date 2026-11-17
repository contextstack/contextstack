//! Integration tests that exec real binaries through `sandbox-exec`; macOS only.
#![cfg(target_os = "macos")]

use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn bin_path() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_cs-sandbox").into()
}

const RUNTIME_READ: &str = r#"["/usr", "/bin", "/sbin", "/System", "/Library", "/private/etc", "/private/var/select", "/dev"]"#;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    outside: PathBuf,
    policy: PathBuf,
    /// The wrapper's TMPDIR, so each test can see the per-call tmp dirs come and go.
    tmp_parent: PathBuf,
}

fn io(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn fixture(allow_net: &str) -> Result<Fixture, String> {
    let dir = tempfile::tempdir().map_err(io)?;
    let root = dir.path().canonicalize().map_err(io)?;
    let workspace = root.join("ws");
    let outside = root.join("outside");
    std::fs::create_dir_all(workspace.join("sub")).map_err(io)?;
    std::fs::create_dir_all(&outside).map_err(io)?;
    let tmp_parent = root.join("tmp-parent");
    std::fs::create_dir_all(&tmp_parent).map_err(io)?;
    std::fs::write(workspace.join("a.txt"), "alpha\n").map_err(io)?;
    std::fs::write(outside.join("secret.txt"), "SENTINEL\n").map_err(io)?;
    std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("link.txt"))
        .map_err(io)?;
    let policy = root.join("policy.json");
    let text =
        format!(r#"{{"version": 1, "runtime_read": {RUNTIME_READ}, "allow_net": {allow_net}}}"#);
    std::fs::write(&policy, text).map_err(io)?;
    Ok(Fixture {
        _dir: dir,
        root,
        workspace,
        outside,
        policy,
        tmp_parent,
    })
}

impl Fixture {
    fn command(&self) -> Command {
        let mut command = Command::new(bin_path());
        command
            .env("TMPDIR", &self.tmp_parent)
            .arg("--policy")
            .arg(&self.policy)
            .arg("--cwd")
            .arg(&self.workspace);
        command
    }

    fn run(&self, tool_argv: &[&str]) -> Result<Output, String> {
        self.command()
            .arg("--")
            .args(tool_argv)
            .output()
            .map_err(io)
    }

    /// A tool inside the workspace (readable under the policy) with a descriptor beside it.
    fn tool_with_descriptor(&self, body: &str, descriptor: &str) -> Result<PathBuf, String> {
        use std::os::unix::fs::PermissionsExt;
        let tool = self.workspace.join("tool");
        std::fs::write(&tool, format!("#!/bin/sh\n{body}\n")).map_err(io)?;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
        std::fs::write(self.workspace.join("tool.json"), descriptor).map_err(io)?;
        Ok(tool)
    }

    /// What is left under the wrapper's TMPDIR: empty once every call has been cleaned up.
    fn leftover_tmp(&self) -> Result<Vec<PathBuf>, String> {
        let entries = std::fs::read_dir(&self.tmp_parent).map_err(io)?;
        Ok(entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect())
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn only_report(dir: &Path) -> Result<serde_json::Value, String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(io)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    if entries.len() != 1 {
        return Err(format!("expected one report, found {entries:?}"));
    }
    let path = entries.remove(0);
    let body = std::fs::read_to_string(&path).map_err(io)?;
    serde_json::from_str(&body).map_err(|error| error.to_string())
}

#[test]
fn echo_passes_through_and_the_wrapper_is_silent() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let output = fx.run(&["/bin/echo", "hi", "there"])?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stdout), "hi there\n");
    assert_eq!(text(&output.stderr), "");
    Ok(())
}

#[test]
fn exit_codes_and_signals_pass_through() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let three = fx.run(&["/bin/sh", "-c", "exit 3"])?;
    assert_eq!(three.status.code(), Some(3));
    let killed = fx.run(&["/bin/sh", "-c", "kill -9 $$"])?;
    assert_eq!(killed.status.code(), Some(137), "{}", text(&killed.stderr));
    Ok(())
}

#[test]
fn workspace_file_reads_but_a_symlink_outside_is_denied() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let inside = fx.run(&["/bin/cat", "a.txt"])?;
    assert_eq!(inside.status.code(), Some(0));
    assert_eq!(text(&inside.stdout), "alpha\n");
    let link = fx.run(&["/bin/cat", "link.txt"])?;
    assert_eq!(link.status.code(), Some(1));
    assert_eq!(text(&link.stdout), "");
    assert!(
        text(&link.stderr).contains("Operation not permitted"),
        "{}",
        text(&link.stderr)
    );
    let direct = fx.run(&[
        "/bin/cat",
        fx.outside.join("secret.txt").to_string_lossy().as_ref(),
    ])?;
    assert_eq!(direct.status.code(), Some(1));
    assert_eq!(text(&direct.stdout), "");
    Ok(())
}

#[test]
fn writes_stay_inside_the_workspace_and_a_private_tmp_dir() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let outside_file = fx.outside.join("w.txt");
    let script = format!("echo x > {} && echo wrote", outside_file.display());
    let denied = fx.run(&["/bin/sh", "-c", &script])?;
    assert_ne!(denied.status.code(), Some(0));
    assert!(!outside_file.exists());
    let allowed = fx.run(&[
        "/bin/sh",
        "-c",
        "printf made > sub/out.txt && printf t > \"$TMPDIR/t.txt\" && cat \"$TMPDIR/t.txt\" && printf \" %s %s %s\" \"$TMPDIR\" \"$TMP\" \"$TEMP\"",
    ])?;
    assert_eq!(allowed.status.code(), Some(0), "{}", text(&allowed.stderr));
    assert_eq!(text(&allowed.stderr), "");
    let stdout = text(&allowed.stdout);
    let words: Vec<&str> = stdout.split(' ').collect();
    assert_eq!(words.len(), 4, "{stdout}");
    assert_eq!(words[0], "t");
    assert_eq!(words[1], words[2]);
    assert_eq!(words[1], words[3]);
    let tmp = Path::new(words[1]);
    assert_eq!(tmp.parent(), Some(fx.tmp_parent.as_path()));
    let name = tmp.file_name().map(|n| n.to_string_lossy().into_owned());
    assert!(
        name.as_deref()
            .is_some_and(|n| n.starts_with("cs-sandbox-")),
        "{name:?}"
    );
    assert!(!tmp.exists(), "the call's tmp dir must be removed");
    assert_eq!(
        std::fs::read_to_string(fx.workspace.join("sub/out.txt")).map_err(io)?,
        "made"
    );
    assert!(!fx.workspace.join(".lab-tmp").exists());
    let mode = fx.run(&["/bin/sh", "-c", "/usr/bin/stat -f %Lp \"$TMPDIR\""])?;
    assert_eq!(text(&mode.stdout), "700\n", "{}", text(&mode.stderr));
    assert_eq!(fx.leftover_tmp()?, Vec::<PathBuf>::new());
    Ok(())
}

#[test]
fn the_tmp_dir_is_removed_on_failure_and_signal_and_differs_per_call() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let first = fx.run(&["/bin/sh", "-c", "printf \"$TMPDIR\"; exit 3"])?;
    assert_eq!(first.status.code(), Some(3));
    let second = fx.run(&[
        "/bin/sh",
        "-c",
        "printf \"$TMPDIR\"; mkdir \"$TMPDIR/d\"; kill -9 $$",
    ])?;
    assert_eq!(second.status.code(), Some(137));
    assert_ne!(text(&first.stdout), text(&second.stdout));
    assert_eq!(fx.leftover_tmp()?, Vec::<PathBuf>::new());
    let tool = fx.tool_with_descriptor("exit 0", r#"{"sandbox": {"read": ["../escape"]}}"#)?;
    let refused = fx.run(&[tool.to_string_lossy().as_ref()])?;
    assert_eq!(refused.status.code(), Some(101));
    assert_eq!(fx.leftover_tmp()?, Vec::<PathBuf>::new());
    Ok(())
}

#[test]
fn write_env_writes_under_the_allowed_root_and_is_narrowed_outside_it() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let state_root = fx.root.join("state-root");
    let policy = format!(
        r#"{{"version": 1, "runtime_read": {RUNTIME_READ}, "allow_net": ["none"], "write_env": {{"LAB_X": ["{}"]}}}}"#,
        state_root.display()
    );
    std::fs::write(&fx.policy, policy).map_err(io)?;
    // The policy's grandparent (the default code root) holds the state root here; name one apart.
    let code_root = fx.root.join("code");
    let tool = fx.tool_with_descriptor(
        "printf w > \"$LAB_X/f.txt\"",
        r#"{"sandbox": {"write_env": ["LAB_X"]}}"#,
    )?;
    let granted_dir = fx.root.join("state-root/lab/state");
    let granted = fx
        .command()
        .env("LAB_X", &granted_dir)
        .env("LAB_CODE_ROOT", &code_root)
        .arg("--")
        .arg(&tool)
        .output()
        .map_err(io)?;
    assert_eq!(granted.status.code(), Some(0), "{}", text(&granted.stderr));
    assert_eq!(
        std::fs::read_to_string(granted_dir.join("f.txt")).map_err(io)?,
        "w"
    );
    let reports = fx.root.join("reports");
    let outside_dir = fx.outside.join("state");
    let narrowed = fx
        .command()
        .env("LAB_X", &outside_dir)
        .env("LAB_CODE_ROOT", &code_root)
        .arg("--report-dir")
        .arg(&reports)
        .arg("--")
        .arg(&tool)
        .output()
        .map_err(io)?;
    assert_ne!(narrowed.status.code(), Some(0));
    assert!(!outside_dir.exists());
    let report = only_report(&reports)?;
    assert_eq!(report["narrowed"][0]["capability"], "write_env");
    assert_eq!(report["narrowed"][0]["requested"], "LAB_X");
    Ok(())
}

#[test]
fn the_browser_profile_is_granted_only_when_the_host_lists_it() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let tool = fx.tool_with_descriptor(
        "printf '%s|' \"${GSTACK_CHROMIUM_NO_SANDBOX:-unset}\"; cat \"$OUTSIDE/secret.txt\"",
        r#"{"sandbox": {"profile": "browser"}}"#,
    )?;
    let reports = fx.root.join("reports");
    let call = || {
        fx.command()
            .env_remove("GSTACK_CHROMIUM_NO_SANDBOX")
            .env("OUTSIDE", &fx.outside)
            .arg("--report-dir")
            .arg(&reports)
            .arg("--")
            .arg(&tool)
            .output()
            .map_err(io)
    };
    let narrowed = call()?;
    assert_ne!(narrowed.status.code(), Some(0));
    assert_eq!(text(&narrowed.stdout), "unset|");
    let report = only_report(&reports)?;
    assert_eq!(report["narrowed"][0]["capability"], "profile");
    assert_eq!(report["narrowed"][0]["requested"], "browser");
    assert_eq!(
        report["effective_policy"]["profile"],
        serde_json::Value::Null
    );
    std::fs::remove_dir_all(&reports).map_err(io)?;
    let policy = format!(
        r#"{{"version": 1, "runtime_read": {RUNTIME_READ}, "allow_net": ["none"], "profiles": {{"browser": {{"read": ["{}"]}}}}}}"#,
        fx.outside.display()
    );
    std::fs::write(&fx.policy, policy).map_err(io)?;
    let granted = call()?;
    assert_eq!(granted.status.code(), Some(0), "{}", text(&granted.stderr));
    assert_eq!(text(&granted.stdout), "1|SENTINEL\n");
    let report = only_report(&reports)?;
    assert_eq!(report["narrowed"], serde_json::json!([]));
    assert_eq!(report["effective_policy"]["profile"], "browser");
    Ok(())
}

#[test]
fn report_records_the_call() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let reports = fx.root.join("reports");
    let output = fx
        .command()
        .arg("--report-dir")
        .arg(&reports)
        .args(["--", "/bin/sh", "-c", "exit 4"])
        .output()
        .map_err(io)?;
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(text(&output.stderr), "");
    let report = only_report(&reports)?;
    assert_eq!(report["backend"], "seatbelt");
    assert_eq!(report["tool"], "/bin/sh");
    assert_eq!(report["exit"], 4);
    assert_eq!(report["signal"], serde_json::Value::Null);
    assert_eq!(report["cwd"], fx.workspace.to_string_lossy().as_ref());
    assert_eq!(report["effective_policy"]["net"], "none");
    let tmp = report["effective_policy"]["tmp"]
        .as_str()
        .unwrap_or_default();
    assert!(
        tmp.starts_with(&format!("{}/cs-sandbox-", fx.tmp_parent.display())),
        "{tmp}"
    );
    assert!(!Path::new(tmp).exists());
    assert_eq!(report["profile_sha256"].as_str().map(str::len), Some(64));
    assert_eq!(report["narrowed"].as_array().map(Vec::len), Some(0));
    assert!(report["duration_ms"].is_u64());
    Ok(())
}

#[test]
fn unwritable_report_dir_is_one_stderr_line_not_a_failure() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let output = fx
        .command()
        .arg("--report-dir")
        .arg("/dev/null/reports")
        .args(["--", "/bin/echo", "ok"])
        .output()
        .map_err(io)?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stdout), "ok\n");
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("cs-sandbox: report not written:"),
        "{stderr}"
    );
    assert_eq!(stderr.lines().count(), 1);
    Ok(())
}

#[test]
fn wrapper_failures_use_the_contract_exit_codes() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let usage = Command::new(bin_path())
        .args(["--policy", fx.policy.to_string_lossy().as_ref()])
        .output()
        .map_err(io)?;
    assert_eq!(usage.status.code(), Some(2));
    assert!(text(&usage.stderr).starts_with("cs-sandbox: "));
    std::fs::write(fx.root.join("bad.json"), "{nope").map_err(io)?;
    let invalid = Command::new(bin_path())
        .arg("--policy")
        .arg(fx.root.join("bad.json"))
        .args(["--", "/bin/echo", "never"])
        .output()
        .map_err(io)?;
    assert_eq!(invalid.status.code(), Some(101));
    assert_eq!(text(&invalid.stdout), "");
    assert!(
        text(&invalid.stderr).starts_with("cs-sandbox: invalid policy"),
        "{}",
        text(&invalid.stderr)
    );
    let tool = fx.tool_with_descriptor("exit 0", r#"{"sandbox": {"read": ["../escape"]}}"#)?;
    let escape = fx.run(&[tool.to_string_lossy().as_ref()])?;
    assert_eq!(escape.status.code(), Some(101));
    assert!(
        text(&escape.stderr).contains("escapes the workspace"),
        "{}",
        text(&escape.stderr)
    );
    Ok(())
}

#[test]
fn gateway_without_a_socket_variable_is_101() -> Result<(), String> {
    let fx = fixture("[\"none\", \"gateway\"]")?;
    let tool = fx.tool_with_descriptor("exit 0", r#"{"sandbox": {"net": "gateway"}}"#)?;
    let output = fx
        .command()
        .env_remove("CS_GATEWAY_SOCKET")
        .arg("--")
        .arg(&tool)
        .output()
        .map_err(io)?;
    assert_eq!(output.status.code(), Some(101));
    assert!(
        text(&output.stderr).contains("CS_GATEWAY_SOCKET"),
        "{}",
        text(&output.stderr)
    );
    Ok(())
}

fn local_tcp_port() -> Result<(TcpListener, u16), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(io)?;
    let port = listener.local_addr().map_err(io)?.port();
    Ok((listener, port))
}

#[test]
fn requested_net_all_is_narrowed_to_none_and_recorded() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let (_listener, port) = local_tcp_port()?;
    let tool = fx.tool_with_descriptor(
        "exec /usr/bin/nc -z 127.0.0.1 \"$1\"",
        r#"{"sandbox": {"net": "all"}}"#,
    )?;
    let reports = fx.root.join("reports");
    let output = fx
        .command()
        .arg("--report-dir")
        .arg(&reports)
        .arg("--")
        .arg(&tool)
        .arg(port.to_string())
        .output()
        .map_err(io)?;
    assert_ne!(
        output.status.code(),
        Some(0),
        "TCP connect must fail under net none"
    );
    let report = only_report(&reports)?;
    assert_eq!(report["effective_policy"]["net"], "none");
    assert_eq!(report["narrowed"][0]["capability"], "net");
    assert_eq!(report["narrowed"][0]["requested"], "all");
    assert_eq!(report["narrowed"][0]["granted"], "none");
    Ok(())
}

#[test]
fn net_all_when_allowed_reaches_a_local_port() -> Result<(), String> {
    let fx = fixture("[\"none\", \"all\"]")?;
    let (_listener, port) = local_tcp_port()?;
    let tool = fx.tool_with_descriptor(
        "exec /usr/bin/nc -z 127.0.0.1 \"$1\"",
        r#"{"sandbox": {"net": "all"}}"#,
    )?;
    let output = fx.run(&[tool.to_string_lossy().as_ref(), &port.to_string()])?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    Ok(())
}

#[test]
fn net_all_denies_unlisted_unix_but_allows_tcp() -> Result<(), String> {
    let fx = fixture("[\"none\", \"all\"]")?;
    let granted = fx.root.join("granted.sock");
    let other = fx.root.join("other.sock");
    let granted_listener = UnixListener::bind(&granted).map_err(io)?;
    let other_listener = UnixListener::bind(&other).map_err(io)?;
    let (_tcp, port) = local_tcp_port()?;
    let tool = fx.tool_with_descriptor(
        "case \"$1\" in\n tcp) exec /usr/bin/nc -z 127.0.0.1 \"$2\" ;;\n *) exec /usr/bin/nc -U \"$2\" < /dev/null ;;\nesac",
        r#"{"sandbox": {"net": "all", "sockets": ["b-one"]}}"#,
    )?;
    let policy = format!(
        r#"{{"version": 1, "runtime_read": {RUNTIME_READ}, "allow_net": ["none", "all"],
            "sockets": {{"b-one": "{}"}}}}"#,
        granted.display()
    );
    std::fs::write(&fx.policy, policy).map_err(io)?;
    let tcp = fx
        .command()
        .arg("--")
        .arg(&tool)
        .args(["tcp", &port.to_string()])
        .output()
        .map_err(io)?;
    assert_eq!(tcp.status.code(), Some(0), "{}", text(&tcp.stderr));
    let granted_text = granted.display().to_string();
    let mut child = fx
        .command()
        .arg("--")
        .arg(&tool)
        .args(["granted", &granted_text])
        .spawn()
        .map_err(io)?;
    assert!(
        accepted_while(&granted_listener, &mut child)?,
        "a granted unix socket must remain reachable under net all"
    );
    let other_text = other.display().to_string();
    let mut child = fx
        .command()
        .arg("--")
        .arg(&tool)
        .args(["other", &other_text])
        .spawn()
        .map_err(io)?;
    assert!(
        !accepted_while(&other_listener, &mut child)?,
        "an unlisted unix socket must be denied under net all"
    );
    Ok(())
}

#[test]
fn net_all_resolves_hostname_via_mdnsresponder() -> Result<(), String> {
    use std::net::ToSocketAddrs;

    let fx = fixture("[\"none\", \"all\"]")?;
    let src = fx.root.join("dnsprobe.c");
    std::fs::write(
        &src,
        r#"#include <errno.h>
#include <netdb.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
int main(void) {
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s < 0) {
        fprintf(stderr, "socket: %s\n", strerror(errno));
        return errno == EPERM ? 13 : 2;
    }
    struct sockaddr_un u = {0};
    u.sun_family = AF_UNIX;
    strcpy(u.sun_path, "/private/var/run/mDNSResponder");
    if (connect(s, (struct sockaddr *)&u, sizeof(u)) < 0) {
        int err = errno;
        close(s);
        fprintf(stderr, "mDNSResponder connect: %s\n", strerror(err));
        return err == EPERM ? 13 : 3;
    }
    close(s);
    struct addrinfo *res = NULL;
    int rc = getaddrinfo("example.com", "443", NULL, &res);
    if (rc != 0) {
        fprintf(stderr, "getaddrinfo: %s (errno=%s)\n", gai_strerror(rc), strerror(errno));
        return errno == EPERM ? 13 : 4;
    }
    freeaddrinfo(res);
    printf("ok\n");
    return 0;
}
"#,
    )
    .map_err(io)?;
    let bin = fx.workspace.join("dnsprobe");
    let cc = Command::new("/usr/bin/cc")
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .output()
        .map_err(io)?;
    assert_eq!(cc.status.code(), Some(0), "{}", text(&cc.stderr));

    std::fs::write(
        fx.workspace.join("dnsprobe.json"),
        r#"{"sandbox": {"net": "none"}}"#,
    )
    .map_err(io)?;
    let denied = fx.run(&[bin.to_string_lossy().as_ref()])?;
    assert_eq!(
        denied.status.code(),
        Some(13),
        "net none must deny mDNSResponder with EPERM: {}",
        text(&denied.stderr)
    );

    std::fs::write(
        fx.workspace.join("dnsprobe.json"),
        r#"{"sandbox": {"net": "all"}}"#,
    )
    .map_err(io)?;
    let host_resolves = ("example.com", 443).to_socket_addrs().is_ok();
    let allowed = fx.run(&[bin.to_string_lossy().as_ref()])?;
    let stderr = text(&allowed.stderr);
    assert_ne!(
        allowed.status.code(),
        Some(13),
        "net all must not sandbox-deny mDNSResponder: {stderr}"
    );
    assert!(
        !stderr.contains("Operation not permitted"),
        "unexpected sandbox denial in DNS resolution: {stderr}"
    );
    if host_resolves {
        assert_eq!(allowed.status.code(), Some(0), "{stderr}");
        assert_eq!(text(&allowed.stdout), "ok\n");
    }
    Ok(())
}

#[test]
fn gateway_reaches_only_the_named_unix_socket() -> Result<(), String> {
    let fx = fixture("[\"none\", \"gateway\"]")?;
    let socket_path = fx.root.join("gw.sock");
    let listener = UnixListener::bind(&socket_path).map_err(io)?;
    listener.set_nonblocking(true).map_err(io)?;
    let (_tcp, port) = local_tcp_port()?;
    let tool = fx.tool_with_descriptor(
        "if [ \"$1\" = unix ]; then exec /usr/bin/nc -U \"$2\" < /dev/null; else exec /usr/bin/nc -z 127.0.0.1 \"$2\"; fi",
        r#"{"sandbox": {"net": "gateway"}}"#,
    )?;
    let mut child = fx
        .command()
        .env("CS_GATEWAY_SOCKET", &socket_path)
        .arg("--")
        .arg(&tool)
        .arg("unix")
        .arg(&socket_path)
        .spawn()
        .map_err(io)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut accepted = false;
    while Instant::now() < deadline {
        if let Ok((stream, _)) = listener.accept() {
            drop(stream);
            accepted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = child.wait().map_err(io)?;
    assert!(
        accepted,
        "no connection arrived on the gateway socket (child {status})"
    );
    let tcp = fx
        .command()
        .env("CS_GATEWAY_SOCKET", &socket_path)
        .arg("--")
        .arg(&tool)
        .arg("tcp")
        .arg(port.to_string())
        .output()
        .map_err(io)?;
    assert_ne!(
        tcp.status.code(),
        Some(0),
        "TCP must be denied under net gateway"
    );
    Ok(())
}

/// Whether `listener` accepts a connection while `child` runs (10 s at most); the child is reaped.
fn accepted_while(
    listener: &UnixListener,
    child: &mut std::process::Child,
) -> Result<bool, String> {
    listener.set_nonblocking(true).map_err(io)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok((stream, _)) = listener.accept() {
            drop(stream);
            child.wait().map_err(io)?;
            return Ok(true);
        }
        if child.try_wait().map_err(io)?.is_some() {
            return Ok(listener.accept().is_ok());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().map_err(io)?;
    child.wait().map_err(io)?;
    Ok(false)
}

#[test]
fn a_named_socket_is_the_only_endpoint_a_tool_reaches() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let granted = fx.root.join("granted.sock");
    let other = fx.root.join("other.sock");
    let granted_listener = UnixListener::bind(&granted).map_err(io)?;
    let other_listener = UnixListener::bind(&other).map_err(io)?;
    let policy = format!(
        r#"{{"version": 1, "runtime_read": {RUNTIME_READ}, "allow_net": ["none"],
            "sockets": {{"b-one": "{}", "b-two": "{}"}}}}"#,
        granted.display(),
        other.display()
    );
    std::fs::write(&fx.policy, policy).map_err(io)?;
    let (_tcp, port) = local_tcp_port()?;
    let tool = fx.tool_with_descriptor(
        "case \"$1\" in\n granted) exec /usr/bin/nc -U \"$CS_SOCKET_B_ONE\" < /dev/null ;;\n other) exec /usr/bin/nc -U \"$2\" < /dev/null ;;\n *) exec /usr/bin/nc -z 127.0.0.1 \"$2\" ;;\nesac",
        r#"{"sandbox": {"net": "none", "sockets": ["b-one"]}}"#,
    )?;
    let spawn = |args: &[&str]| fx.command().arg("--").arg(&tool).args(args).spawn();
    let mut child = spawn(&["granted"]).map_err(io)?;
    assert!(
        accepted_while(&granted_listener, &mut child)?,
        "the granted socket, named by CS_SOCKET_B_ONE, must be reachable"
    );
    let other_text = other.display().to_string();
    let mut child = spawn(&["other", &other_text]).map_err(io)?;
    assert!(
        !accepted_while(&other_listener, &mut child)?,
        "a socket the host lists but the descriptor did not request must be unreachable"
    );
    let tcp = fx
        .command()
        .arg("--")
        .arg(&tool)
        .args(["tcp", &port.to_string()])
        .output()
        .map_err(io)?;
    assert_ne!(tcp.status.code(), Some(0), "TCP must stay denied");
    // Without the request, the granted socket is unreachable too.
    let bare = fx.tool_with_descriptor(
        "exec /usr/bin/nc -U \"$1\" < /dev/null",
        r#"{"sandbox": {"net": "none"}}"#,
    )?;
    let granted_text = granted.display().to_string();
    let mut child = fx
        .command()
        .arg("--")
        .arg(&bare)
        .arg(&granted_text)
        .spawn()
        .map_err(io)?;
    assert!(
        !accepted_while(&granted_listener, &mut child)?,
        "net none without the request must not reach the socket"
    );
    Ok(())
}

#[test]
fn narrowed_runtime_read_denies_secret_env_and_paths_outside_it() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let code = fx.root.join("cs");
    let readable = code.join("tools/bin");
    std::fs::create_dir_all(&readable).map_err(io)?;
    std::fs::write(readable.join("ok.txt"), "readable\n").map_err(io)?;
    std::fs::write(readable.join(".env"), "SECRET=1\n").map_err(io)?;
    std::fs::write(readable.join(".env.local"), "SECRET=1\n").map_err(io)?;
    std::fs::write(readable.join(".env_prod"), "SECRET=1\n").map_err(io)?;
    std::fs::write(readable.join(".env-local"), "SECRET=1\n").map_err(io)?;
    std::fs::write(readable.join(".envrc"), "SECRET=1\n").map_err(io)?;
    std::fs::write(readable.join(".envrc.local"), "SECRET=1\n").map_err(io)?;
    std::fs::write(readable.join("foo.env"), "SECRET=1\n").map_err(io)?;
    std::fs::write(readable.join("x.env.bak"), "SECRET=1\n").map_err(io)?;
    std::fs::write(readable.join(".env.example"), "ok\n").map_err(io)?;
    std::fs::write(fx.workspace.join(".env"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join(".env_prod"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join(".env-local"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join(".envrc"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join(".envrc.local"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join("bar.env"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join("x.env.bak"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join(".env.example"), "ws-ok\n").map_err(io)?;
    std::fs::write(code.join("tools/.env.example"), "outside-read-root\n").map_err(io)?;
    std::fs::write(fx.outside.join(".env.example"), "sibling-example\n").map_err(io)?;
    std::fs::write(
        &fx.policy,
        format!(
            r#"{{
  "version": 1,
  "runtime_read": ["{}", "{}"],
  "allow_net": ["none"]
}}"#,
            readable.to_string_lossy(),
            "/usr"
        ),
    )
    .map_err(io)?;
    let ok = fx.run(&[
        "/bin/cat",
        readable.join("ok.txt").to_string_lossy().as_ref(),
    ])?;
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok.stderr));
    assert_eq!(text(&ok.stdout), "readable\n");
    for denied_path in [
        readable.join(".env"),
        readable.join(".env.local"),
        readable.join(".env_prod"),
        readable.join(".env-local"),
        readable.join(".envrc"),
        readable.join(".envrc.local"),
        readable.join("foo.env"),
        readable.join("x.env.bak"),
        fx.workspace.join(".env"),
        fx.workspace.join(".env_prod"),
        fx.workspace.join(".env-local"),
        fx.workspace.join(".envrc"),
        fx.workspace.join(".envrc.local"),
        fx.workspace.join("bar.env"),
        fx.workspace.join("x.env.bak"),
        code.join("tools/.env.example"),
        fx.outside.join(".env.example"),
        fx.outside.join("secret.txt"),
    ] {
        let out = fx.run(&["/bin/cat", denied_path.to_string_lossy().as_ref()])?;
        assert_ne!(
            out.status.code(),
            Some(0),
            "expected {} to be denied: {}",
            denied_path.display(),
            text(&out.stdout)
        );
        assert_eq!(text(&out.stdout), "");
    }
    let example = fx.run(&[
        "/bin/cat",
        readable.join(".env.example").to_string_lossy().as_ref(),
    ])?;
    assert_eq!(example.status.code(), Some(0), "{}", text(&example.stderr));
    assert_eq!(text(&example.stdout), "ok\n");
    let ws_example = fx.run(&[
        "/bin/cat",
        fx.workspace.join(".env.example").to_string_lossy().as_ref(),
    ])?;
    assert_eq!(
        ws_example.status.code(),
        Some(0),
        "{}",
        text(&ws_example.stderr)
    );
    assert_eq!(text(&ws_example.stdout), "ws-ok\n");
    Ok(())
}

#[test]
fn workspace_secret_env_cannot_be_read_via_rename_or_link() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    std::fs::write(fx.workspace.join(".env"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join(".env.local"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join(".envrc"), "SECRET=1\n").map_err(io)?;
    std::fs::write(fx.workspace.join("bar.env"), "SECRET=1\n").map_err(io)?;
    for bypass in [
        "mv .env renamed && cat renamed",
        "mv .env.local renamed && cat renamed",
        "mv .envrc renamed && cat renamed",
        "mv bar.env renamed && cat renamed",
        "ln .env hl && cat hl",
        "ln .env.local hl && cat hl",
        "ln .envrc hl && cat hl",
        "ln bar.env hl && cat hl",
        "echo x > new.env",
        "mv a.txt .env",
    ] {
        let out = fx.run(&["/bin/sh", "-c", bypass])?;
        assert_ne!(
            out.status.code(),
            Some(0),
            "expected workspace .env bypass {bypass:?} to be denied: {}",
            text(&out.stdout)
        );
        assert!(
            !text(&out.stdout).contains("SECRET=1"),
            "secret leaked via {bypass:?}: {}",
            text(&out.stdout)
        );
    }
    assert!(fx.workspace.join(".env").is_file());
    assert!(fx.workspace.join(".env.local").is_file());
    assert!(fx.workspace.join(".envrc").is_file());
    assert!(fx.workspace.join("bar.env").is_file());
    assert!(!fx.workspace.join("renamed").exists());
    assert!(!fx.workspace.join("hl").exists());
    Ok(())
}

#[test]
fn narrowed_runtime_read_still_execs_a_tool_outside_the_list() -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let fx = fixture(r#"["none", "all"]"#)?;
    let code = fx.root.join("cs");
    std::fs::create_dir_all(code.join("tools/bin")).map_err(io)?;
    let script = code.join("fixture/multi");
    let Some(parent) = script.parent() else {
        return Err("fixture script has no parent".to_owned());
    };
    std::fs::create_dir_all(parent).map_err(io)?;
    std::fs::write(&script, "#!/bin/sh\necho exe-narrowed $*\n").map_err(io)?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
    std::fs::write(
        &fx.policy,
        format!(
            r#"{{
  "version": 1,
  "runtime_read": ["{}", "/usr", "/bin"],
  "allow_net": ["none", "all"]
}}"#,
            code.join("tools/bin").to_string_lossy()
        ),
    )
    .map_err(io)?;
    let out = fx.run(&[script.to_string_lossy().as_ref(), "one", "two"])?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "exe-narrowed one two\n");
    Ok(())
}

#[test]
fn probe_reports_seatbelt_without_limits() -> Result<(), String> {
    let fx = fixture("[\"none\"]")?;
    let output = Command::new(bin_path())
        .arg("--probe")
        .arg("--policy")
        .arg(&fx.policy)
        .output()
        .map_err(io)?;
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    assert_eq!(value["backend"], "seatbelt");
    assert_eq!(value["platform"], "macos");
    assert_eq!(value["fs"], true);
    assert_eq!(value["net"]["gateway"], true);
    assert_eq!(value["limits"]["memory"], false);
    assert_eq!(value["limits"]["processes"], false);
    assert_eq!(value["limits"]["wall_clock"], "caller");
    Ok(())
}
