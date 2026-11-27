//! End-to-end native host and registration contracts.
#![allow(clippy::unwrap_used)]

use std::process::Command;

use serde_json::Value;

const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HOST: &str = env!("CARGO_BIN_EXE_browser-host");

#[test]
fn manifests_for_each_platform() {
    for (os, executable, destination) in [
        (
            "macos",
            "/opt/browser-host",
            "~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.contextstack.browser.json",
        ),
        (
            "linux",
            "/opt/browser-host",
            "~/.config/google-chrome/NativeMessagingHosts/com.contextstack.browser.json",
        ),
        (
            "windows",
            r"C:\ContextStack\browser-host.exe",
            r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.contextstack.browser",
        ),
    ] {
        let result = Command::new(HOST)
            .args(["manifest", os, ID, executable])
            .output()
            .unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(destination));
        let manifest: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(
            manifest,
            serde_json::json!({
                "name": "com.contextstack.browser",
                "description": "ContextStack browser native messaging host",
                "path": executable,
                "type": "stdio",
                "allowed_origins": [format!("chrome-extension://{ID}/")],
            })
        );
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        os::unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::UnixStream,
        },
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use ring::hmac;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::{HOST, ID};

    const SOCKET: &str = ".contextstack/browser/native-sockets/public.sock";
    const LOG: &str = ".contextstack/browser/log/broker.jsonl";
    const ON_SET: &str = "https://example.com/";
    const OFF_SET: &str = "https://other.example/path";
    const TASKS: &str = "https://tasks.google.com";
    const TASKS_SOCKET: &str = ".contextstack/browser/native-sockets/google-tasks.sock";

    struct Fixture {
        home: TempDir,
        relative_home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let home = tempfile::Builder::new()
                .prefix(".browser-test-")
                .tempdir_in(".")
                .unwrap();
            let relative_home = PathBuf::from(home.path().file_name().unwrap());
            fs::create_dir_all(home.path().join(".contextstack/browser")).unwrap();
            fs::write(
                home.path().join(".contextstack/browser/native-host.json"),
                json!({"extension_id":ID,"policy":"policy.json","development":true}).to_string(),
            )
            .unwrap();
            fs::write(home.path().join("policy.json"),
                json!({"version":1,"sets":{"public":["https://example.com"],"other":["https://other.example"],"google-tasks":[TASKS]}}).to_string()).unwrap();
            Self {
                home,
                relative_home,
            }
        }

        fn command(&self, caller: &str) -> Command {
            let mut command = Command::new(HOST);
            command
                .current_dir(self.home.path())
                .env("HOME", ".")
                .arg(caller);
            command
        }
    }

    struct Driver(Child);

    impl Driver {
        fn receive(&mut self) -> Value {
            let output = self.0.stdout.as_mut().unwrap();
            let mut header = [0; 4];
            output.read_exact(&mut header).unwrap();
            let mut body = vec![0; u32::from_le_bytes(header) as usize];
            output.read_exact(&mut body).unwrap();
            serde_json::from_slice(&body).unwrap()
        }

        fn send(&mut self, message: &Value) {
            let input = self.0.stdin.as_mut().unwrap();
            let body = serde_json::to_vec(message).unwrap();
            input
                .write_all(&u32::try_from(body.len()).unwrap().to_le_bytes())
                .unwrap();
            input.write_all(&body).unwrap();
        }
    }

    impl Drop for Driver {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn native_host_does_not_publish_a_model_socket() {
        let fixture = Fixture::new();
        let mut command = fixture.command(&format!("chrome-extension://{ID}/"));
        let _driver = Driver(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let trusted = fixture
            .relative_home
            .join(".contextstack/browser/native-sockets/public.sock");
        let model = fixture
            .relative_home
            .join(".contextstack/public-browser/sockets/public.sock");
        let legacy = fixture
            .relative_home
            .join(".contextstack/browser/sockets/public.sock");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !trusted.exists() && !model.exists() && !legacy.exists() && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !legacy.exists(),
            "native host published its legacy model socket"
        );
        assert!(!model.exists(), "native host published the model socket");
        assert!(
            fs::metadata(trusted).is_ok_and(|metadata| metadata.file_type().is_socket()),
            "trusted action socket unavailable"
        );
    }

    #[test]
    fn production_policy_comes_from_the_installed_release() {
        let fixture = Fixture::new();
        let state = fixture.home.path().join(".contextstack");
        fs::write(
            state.join("browser/native-host.json"),
            json!({"extension_id": ID, "policy": "policy.json"}).to_string(),
        )
        .unwrap();
        let caller = format!("chrome-extension://{ID}/");
        let declined = fixture.command(&caller).output().unwrap();
        assert!(!declined.status.success());
        assert!(String::from_utf8_lossy(&declined.stderr).contains("installed release"));

        let release = state.join("releases/fixture");
        fs::create_dir_all(release.join("cs/policy")).unwrap();
        fs::write(
            release.join("cs/policy/browser-native.json"),
            json!({"version":1,"sets":{"google-tasks":[TASKS]}}).to_string(),
        )
        .unwrap();
        std::os::unix::fs::symlink("releases/fixture", state.join("current")).unwrap();
        fs::write(
            state.join("browser/native-host.json"),
            json!({"extension_id": ID}).to_string(),
        )
        .unwrap();
        let mut driver = Driver(
            fixture
                .command(&caller)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let socket = fixture.relative_home.join(TASKS_SOCKET);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() && Instant::now() < deadline {
            assert!(
                driver.0.try_wait().unwrap().is_none(),
                "installed policy did not load"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(fs::metadata(socket).unwrap().file_type().is_socket());
        assert!(!fixture.relative_home.join(SOCKET).exists());
    }

    fn ask(path: &Path, url: &str) -> UnixStream {
        let mut stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(stream, "{}", json!({"verb":"open","url":url})).unwrap();
        stream
    }

    fn reply(stream: UnixStream) -> Value {
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn refuses_unpinned_caller() {
        let fixture = Fixture::new();
        let result = fixture
            .command("chrome-extension://bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/")
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert_eq!(result.stdout, [] as [u8; 0]);
        assert!(String::from_utf8_lossy(&result.stderr).contains("extension caller"));
        assert!(!fixture.home.path().join(SOCKET).exists());
        let status = fixture
            .home
            .path()
            .join(".contextstack/browser/log/host-start.json");
        let failure: Value = serde_json::from_slice(&fs::read(&status).unwrap()).unwrap();
        assert_eq!(failure["error"], "caller");
        assert_eq!(
            fs::metadata(status).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn render_for_scheduled_reader(path: &Path, driver: &mut Driver) -> Value {
        let client = ask(path, TASKS);
        let open = driver.receive();
        assert_eq!(
            open,
            json!({"type":"open","id":1,"url":TASKS,"origins":[TASKS]})
        );
        driver.send(&json!({"type":"navigation","id":1,"event":1,"url":TASKS}));
        assert_eq!(
            driver.receive(),
            json!({"type":"navigation_decision","id":1,"event":1,"allow":true})
        );
        for (event, url, parent) in [
            (2, "about:srcdoc", Some(TASKS)),
            (3, "blob:https://tasks.google.com/frame", None),
        ] {
            driver.send(&json!({"type":"navigation","id":1,"event":event,
                               "url":url,"parent_origin":parent}));
            assert_eq!(driver.receive()["allow"], true);
        }
        driver.send(&json!({"type":"result","id":1,"final_url":TASKS,"status":200,"title":"Example","text":"Page text"}));
        let evidence = reply(client);
        assert_eq!(evidence["title"], "Example");
        assert_eq!(evidence["text"], "Page text");
        assert_eq!(
            evidence["navigations"],
            json!([TASKS, "about:srcdoc", "blob:https://tasks.google.com/frame"])
        );
        evidence
    }

    fn install_action_key(browser: &Path, secret: [u8; 32]) {
        let key_path = browser.canonicalize().unwrap().join("action.key");
        fs::write(&key_path, hex::encode(secret)).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            browser.join("native-host.json"),
            json!({"extension_id":ID,"policy":"policy.json","development":true,"action_key_file":key_path})
                .to_string(),
        )
        .unwrap();
    }

    fn spawn_driver(fixture: &Fixture) -> Driver {
        Driver(
            fixture
                .command(&format!("chrome-extension://{ID}/"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }

    fn send_when_listening(path: &Path, deadline: Instant, request: &Value) -> UnixStream {
        let mut stream = loop {
            if let Ok(stream) = UnixStream::connect(path) {
                break stream;
            }
            assert!(
                Instant::now() < deadline,
                "could not connect to browser socket"
            );
            thread::sleep(Duration::from_millis(10));
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(stream, "{request}").unwrap();
        stream
    }

    /// Checks that capture needs a host proof, then returns the signed capture's screenshot hash.
    fn capture_private_screenshot(
        browser: &Path,
        driver: &mut Driver,
        send: &impl Fn(Value) -> UnixStream,
        key: &hmac::Key,
        digest: &str,
    ) -> String {
        let mut request = json!({"verb":"capture_action","url":ON_SET,"target":"#order",
            "action":"buy","job_id":"job","run_id":"run","digest":"spoofed"});
        assert_eq!(
            reply(send(request.clone()))["error"],
            "browser capture requires host authorization"
        );
        let wire = json!(["contextstack-browser-capture-request-v1", request]).to_string();
        request["host_proof"] = json!(hex::encode(hmac::sign(key, wire.as_bytes()).as_ref()));
        let capture = send(request);
        let frame = driver.receive();
        assert_eq!(frame["type"], "capture_action");
        assert_eq!(frame["approval"]["target"], "#order");
        assert!(frame["approval"].get("digest").is_none());
        let jpeg = [0xff, 0xd8, 0xff, 0xd9];
        let sha = hex::encode(Sha256::digest(jpeg));
        let summary = json!([{"name":"qty","value":"100","hidden":true}]);
        driver.send(
            &json!({"id":frame["id"],"type":"proposal_snapshot","digest":digest,
            "page_url":ON_SET,"values_summary":summary,"image":"data:image/jpeg;base64,/9j/2Q=="}),
        );
        let captured = reply(capture);
        assert_eq!(captured["capture"]["screenshot_sha"], sha);
        assert_eq!(captured["capture"]["values_summary"], summary);
        assert_eq!(
            fs::read(browser.join("action-screenshots").join(&sha)).unwrap(),
            jpeg
        );
        let capture_wire =
            json!(["contextstack-browser-capture-v1", captured["capture"]]).to_string();
        assert!(
            hmac::verify(
                key,
                capture_wire.as_bytes(),
                &hex::decode(captured["capture_proof"].as_str().unwrap()).unwrap()
            )
            .is_ok()
        );
        sha
    }

    fn approved_execution(key: &hmac::Key, digest: &str, sha: &str) -> Value {
        let id = "01ABCDEFGHIJKL0123456789AB";
        let origin = "https://example.com";
        let stamp = chrono::Utc::now().to_rfc3339();
        let wire = json!([
            "contextstack-browser-action-v1",
            id,
            origin,
            "buy",
            "#order",
            digest,
            sha,
            stamp,
            stamp
        ]);
        let proof = hex::encode(hmac::sign(key, wire.to_string().as_bytes()).as_ref());
        json!({"verb":"execute_action","url":ON_SET,"approval":{
            "id":id,"kind":"browser_action","status":"approved","outcome":{"status":"unknown"},
            "origin":origin,"action":"buy","target":"#order","field_values_digest":digest,
            "screenshot_sha":sha,"decided_at":stamp,"claimed_at":stamp,"claim_proof":proof,
        }})
    }

    #[test]
    fn captures_a_private_screenshot_and_cancels_a_valid_claim_on_digest_mismatch() {
        let fixture = Fixture::new();
        let browser = fixture.home.path().join(".contextstack/browser");
        let secret = [23; 32];
        install_action_key(&browser, secret);
        let mut driver = spawn_driver(&fixture);
        let path = fixture.relative_home.join(SOCKET);
        let deadline = Instant::now() + Duration::from_secs(5);
        let send = |request: Value| send_when_listening(&path, deadline, &request);
        let digest = "a".repeat(64);
        let key = hmac::Key::new(hmac::HMAC_SHA256, &secret);
        let sha = capture_private_screenshot(&browser, &mut driver, &send, &key, &digest);

        let action = send(approved_execution(&key, &digest, &sha));
        let frame = driver.receive();
        assert_eq!(frame["type"], "inspect_action");
        driver.send(&json!({"id":frame["id"],"type":"action_snapshot","digest":"b".repeat(64)}));
        assert_eq!(
            driver.receive(),
            json!({"id":frame["id"],"type":"cancel_action"})
        );
        let refused = reply(action);
        assert_eq!(refused["error_kind"], "action_refused");
        assert_eq!(refused["error"], "form digest mismatch");
    }

    fn wait_for_sockets(fixture: &Fixture, driver: &mut Driver, path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::metadata(path)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o777 == 0o600)
            || !fixture.relative_home.join(SOCKET).exists()
        {
            assert!(
                driver.0.try_wait().unwrap().is_none(),
                "host exited during startup"
            );
            assert!(
                Instant::now() < deadline,
                "host failed to create a private socket"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_refuses_off_set_open_and_unverified_action(fixture: &Fixture, path: &Path) {
        let denied = reply(ask(path, OFF_SET));
        assert_eq!(denied["ok"], false);
        assert_eq!(denied["error_kind"], "navigation_blocked");

        let mut action = UnixStream::connect(fixture.relative_home.join(SOCKET)).unwrap();
        action
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(
            action,
            "{}",
            json!({"verb":"execute_action","url":ON_SET,"target":"form",
            "approval":{"status":"approved","claimed_at":"2026-11-27T00:00:00Z"}})
        )
        .unwrap();
        let denied = reply(action);
        assert_eq!(denied["ok"], false);
        assert_eq!(denied["error"], "browser action requires a verified claim");
    }

    fn assert_blocks_off_set_navigations(driver: &mut Driver, path: &Path) {
        let redirect = ask(path, TASKS);
        assert_eq!(driver.receive()["id"], 2);
        driver.send(&json!({"type":"navigation","id":2,"event":1,"url":OFF_SET}));
        assert_eq!(
            driver.receive(),
            json!({"type":"navigation_decision","id":2,"event":1,"allow":false})
        );
        let denied = reply(redirect);
        assert_eq!(denied["ok"], false);
        assert_eq!(denied["error_kind"], "navigation_blocked");

        let final_url = ask(path, TASKS);
        assert_eq!(driver.receive()["id"], 3);
        driver.send(&json!({"type":"result","id":3,"final_url":OFF_SET,"status":200,"title":"Private","text":"Must not leak"}));
        let denied = reply(final_url);
        assert_eq!(denied["ok"], false);
        assert_eq!(denied["error_kind"], "navigation_blocked");
        assert!(denied.get("text").is_none());
    }

    fn assert_sign_in_reads_as_logged_out(driver: &mut Driver, tasks: &Path) {
        let redirected = reply(ask(tasks, OFF_SET));
        assert_eq!(redirected["ok"], false);
        let login_url = "https://accounts.google.com/signin?private=canary";
        let redirected = ask(tasks, TASKS);
        let open = driver.receive();
        assert_eq!(open["origins"], json!([TASKS]));
        driver.send(&json!({"type":"navigation","id":open["id"],"event":1,"url":login_url,"main_frame":true}));
        assert_eq!(driver.receive()["allow"], false);
        assert_eq!(
            reply(redirected),
            json!({"ok":false,"error_kind":"logged_out"})
        );
        let signed_out = ask(tasks, TASKS);
        let open = driver.receive();
        driver.send(&json!({"type":"sign_in_required","id":open["id"]}));
        assert_eq!(
            reply(signed_out),
            json!({"ok":false,"error_kind":"logged_out"})
        );
    }

    fn assert_permission_error_and_status(fixture: &Fixture, driver: &mut Driver, path: &Path) {
        driver.send(&json!({"type":"status","id":0,"origins":["https://example.test/*"]}));
        let permitted = ask(path, TASKS);
        let open = driver.receive();
        driver.send(&json!({"type":"error","id":open["id"],
            "error_kind":"origin_not_permitted","message":"must not leak"}));
        let refused = reply(permitted);
        assert_eq!(refused["error_kind"], "origin_not_permitted");
        assert!(!refused.to_string().contains("must not leak"));
        let status = fixture
            .home
            .path()
            .join(".contextstack/browser/extension-status.json");
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(status).unwrap()).unwrap(),
            json!({"origins":["https://example.test/*"]})
        );
    }

    fn assert_broker_log(fixture: &Fixture) {
        let log = fs::read_to_string(fixture.home.path().join(LOG)).unwrap();
        assert!(!log.contains("private=canary"));
        let rows: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 9);
        assert_eq!(rows[1]["verb"], "execute_action");
        assert_eq!(rows[1]["ok"], false);
        assert_eq!(rows[2]["ok"], true);
        for index in [0, 3, 4] {
            assert_eq!(rows[index]["set"], "google-tasks");
            assert_eq!(rows[index]["ok"], false);
            assert_eq!(rows[index]["error_kind"], "navigation_blocked");
        }
    }

    #[test]
    fn socket_parity_and_origin_enforcement() {
        let fixture = Fixture::new();
        let mut driver = spawn_driver(&fixture);
        let path = fixture.relative_home.join(TASKS_SOCKET);
        wait_for_sockets(&fixture, &mut driver, &path);
        assert_refuses_off_set_open_and_unverified_action(&fixture, &path);
        render_for_scheduled_reader(&path, &mut driver);
        assert_blocks_off_set_navigations(&mut driver, &path);
        let tasks = fixture.relative_home.join(TASKS_SOCKET);
        assert_sign_in_reads_as_logged_out(&mut driver, &tasks);
        assert_permission_error_and_status(&fixture, &mut driver, &path);
        assert_broker_log(&fixture);
        drop(driver.0.stdin.take());
        assert!(driver.0.wait().unwrap().success());
        assert!(!path.exists());
    }
}
