//! Kernel-level executor confinement tests. These require Linux and bubblewrap.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn hostile_executor_cannot_read_credentials_or_modify_protected_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("owner");
    let work = home.join("work");
    let run = temp.path().join("run");
    fs::create_dir_all(home.join(".aws"))?;
    fs::create_dir_all(work.join("protected"))?;
    fs::create_dir_all(work.join("home/.env"))?;
    fs::create_dir(&run)?;
    fs::write(home.join(".aws/credentials"), "fixture")?;
    fs::write(work.join(".env"), "fixture")?;
    fs::write(work.join("home/.env/credentials"), "fixture")?;
    fs::write(work.join("home/public"), "public")?;
    fs::write(work.join(".env.example"), "public")?;
    fs::write(work.join("protected/config"), "pinned")?;
    let host_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let host_address = host_listener.local_addr()?;
    let probe = std::env::current_exe()?;
    let request = serde_json::json!({
        "version": 1, "variant": "native", "workspace": work, "home": home,
        "tmpdir": run, "protected": work.join("protected"),
        "executable": "/bin/sh", "read_roots": [&probe],
    });
    let report = executor_command(&request)
        .args([
            "--",
            "/bin/sh",
            "-eu",
            "-c",
            r#"
            test "$(cat "$HOME/public")" = public
            test -d "$HOME/.env"
            contents=$(cat .env)
            test -z "$contents"
            if (echo bad > .env) || mv .env renamed-env ||
               cat "$HOME/.env/credentials" || touch "$HOME/.env/new" ||
               cat "$OWNER_HOME/.aws/credentials" || touch "$OWNER_HOME/outside" ||
               echo bad > protected/config ||
               mv protected moved; then
                exit 17
            fi
            stat .env
            test "$(cat .env.example)" = public
            touch ordinary "$TMPDIR/ordinary"
            "$KERNEL_PROBE" --exact kernel_probe --nocapture
            "#,
        ])
        .current_dir(&run)
        .env("HOME", work.join("home"))
        .env("OWNER_HOME", &home)
        .env("KERNEL_PROBE", probe)
        .env("CS_SANDBOX_TEST_HOST_ADDRESS", host_address.to_string())
        .env_remove("CS_SANDBOX_TEST_FORWARD")
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", &run)
        .output()?;
    let stderr = String::from_utf8_lossy(&report.stderr);
    if skip_missing_kernel(&report) {
        assert!(!work.join("ordinary").exists(), "{stderr}");
        return Ok(());
    }
    assert!(
        report.status.success(),
        "{stderr}\n{}",
        String::from_utf8_lossy(&report.stdout)
    );
    assert!(work.join("ordinary").exists());
    assert!(run.join("ordinary").exists());
    assert!(!home.join("outside").exists());
    assert!(!work.join("home/.env/new").exists());
    assert_eq!(fs::read_to_string(work.join(".env"))?, "fixture");
    assert_eq!(fs::read_to_string(work.join("protected/config"))?, "pinned");
    Ok(())
}

#[test]
fn kernel_probe() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(address) = std::env::var("CS_SANDBOX_TEST_HOST_ADDRESS") else {
        return Ok(()); // This probe runs inside the sandbox as a subprocess of the main test.
    };
    let address: SocketAddr = address.parse()?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2));
    if std::env::var_os("CS_SANDBOX_TEST_FORWARD").is_some() {
        let mut stream = stream?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.write_all(b"ping")?;
        let mut reply = [0; 4];
        stream.read_exact(&mut reply)?;
        assert_eq!(&reply, b"pong");
    } else {
        assert!(stream.is_err());
    }
    let ptrace = Command::new("/usr/bin/python3")
        .args([
            "-c",
            "import ctypes, errno; libc = ctypes.CDLL(None, use_errno=True); assert libc.ptrace(0, 0, 0, 0) == -1; assert ctypes.get_errno() == errno.EPERM",
        ])
        .status()?;
    assert!(ptrace.success());
    Ok(())
}

fn executor_command(request: &serde_json::Value) -> Command {
    let policy = concat!(env!("CARGO_MANIFEST_DIR"), "/../cs/policy/sandbox.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cs-sandbox"));
    command.args([
        "--policy",
        policy,
        "--profile",
        "cli-executor",
        "--request-json",
    ]);
    command.arg(request.to_string());
    command
}

fn skip_missing_kernel(report: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&report.stderr);
    let skip = std::env::var("CS_SANDBOX_LINUX_SKIP_OK").as_deref() == Ok("1")
        && report.status.code() == Some(101)
        && (stderr.contains("bwrap is required") || stderr.contains("unprivileged user namespace"));
    if skip {
        eprintln!("Confinement refused on this Linux host: {stderr}");
    }
    skip
}

#[test]
fn declared_loopback_bridge_forwards_only_when_enforceable()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("owner");
    let work = temp.path().join("work");
    let run = temp.path().join("run");
    for path in [&home, &work, &run] {
        fs::create_dir(path)?;
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let probe = std::env::current_exe()?;
    let mut request = serde_json::json!({
        "version": 1, "variant": "native", "workspace": work, "home": home, "tmpdir": run,
        "executable": probe, "loopback_ports": [address.port()], "reserved_ports": [address.port()],
    });
    let run_probe = |request: &serde_json::Value| {
        executor_command(request)
            .arg("--")
            .arg(&probe)
            .args(["--exact", "kernel_probe", "--nocapture"])
            .env("CS_SANDBOX_TEST_HOST_ADDRESS", address.to_string())
            .env("CS_SANDBOX_TEST_FORWARD", "1")
            .output()
    };
    let refused = run_probe(&request)?;
    if skip_missing_kernel(&refused) {
        return Ok(());
    }
    assert_eq!(refused.status.code(), Some(101));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("reserved executor loopback port"));

    request["reserved_ports"] = serde_json::json!([]);
    if !cs_landlock::supports_named_sockets() {
        let report = run_probe(&request)?;
        let stderr = String::from_utf8_lossy(&report.stderr);
        assert_eq!(report.status.code(), Some(101));
        assert!(stderr.contains("Landlock ABI 9"), "{stderr}");
        return Ok(());
    }
    let server = thread::spawn(move || -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut message = [0; 4];
        stream.read_exact(&mut message)?;
        assert_eq!(&message, b"ping");
        stream.write_all(b"pong")
    });
    let report = run_probe(&request)?;
    let stderr = String::from_utf8_lossy(&report.stderr);
    assert!(
        report.status.success(),
        "{stderr}\n{}",
        String::from_utf8_lossy(&report.stdout)
    );
    server.join().map_err(|_| "loopback fixture panicked")??;
    Ok(())
}
