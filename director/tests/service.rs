//! The built binary, end to end over HTTP on a loopback port: the access model, the
//! client views, and a hand-made job pulled and reported by a node. Node protocol details are judged
//! by `tests/director-spec/run --impl rust`; these cases cover what that suite does not.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

type Res = Result<(), Box<dyn std::error::Error>>;
/// An HTTP answer: status, lower-cased headers, body.
type Raw = (u16, Vec<(String, String)>, Vec<u8>);
fn bin_path() -> std::path::PathBuf {
    // Set when this integration test is compiled, including `cargo test --release`.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_cs-director"))
}

/// A running Director on a temp data dir; killed on drop.
struct Director {
    child: Child,
    port: u16,
    data: PathBuf,
    tmp: tempfile::TempDir,
}

impl Director {
    fn start() -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with(|_| Ok(Vec::new()))
    }

    /// Start with extra `serve` flags, made in the temp dir before the Director starts.
    fn start_with(
        extra: impl FnOnce(&Path) -> Result<Vec<String>, Box<dyn std::error::Error>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("data");
        let flags = extra(tmp.path())?;
        let mut child = Command::new(bin_path())
            .args([
                "serve",
                "--listen",
                "127.0.0.1:0",
                "--allow-origin",
                "http://127.0.0.1:8740",
            ])
            .arg("--data-dir")
            .arg(&data)
            .args(flags)
            .env_clear()
            .env("HOME", tmp.path())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child.stderr.take().ok_or("no stderr")?;
        let mut lines = BufReader::new(stderr).lines();
        let port = loop {
            let line = lines
                .next()
                .ok_or("the Director exited before listening")??;
            if let Some(rest) = line.split("://127.0.0.1:").nth(1) {
                break rest.split_whitespace().next().ok_or("no port")?.parse()?;
            }
        };
        std::thread::spawn(move || lines.for_each(drop));
        Ok(Self {
            child,
            port,
            data,
            tmp,
        })
    }

    fn admin(&self, args: &[&str]) -> Result<Value, Box<dyn std::error::Error>> {
        let out = Command::new(bin_path())
            .args(args)
            .arg("--data-dir")
            .arg(&self.data)
            .output()?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned().into());
        }
        Ok(serde_json::from_slice(&out.stdout)?)
    }

    fn local_token(&self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(std::fs::read_to_string(self.data.join("local-credential"))?
            .trim()
            .to_string())
    }

    /// One HTTP/1.1 request; returns (status, headers lower-cased, body).
    fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Raw, Box<dyn std::error::Error>> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port))?;
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (k, v) in headers {
            let _ = write!(req, "{k}: {v}\r\n");
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes())?;
        s.write_all(body)?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("no header end")?;
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .ok_or("no status")?
            .parse()?;
        let headers = lines
            .filter_map(|l| l.split_once(": "))
            .map(|(k, v)| (k.to_lowercase(), v.to_string()))
            .collect();
        Ok((status, headers, raw[split + 4..].to_vec()))
    }

    /// A client call with a bearer token; returns (status, JSON).
    fn client(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &Value,
    ) -> Result<(u16, Value), Box<dyn std::error::Error>> {
        let auth = token.map(|t| format!("Bearer {t}"));
        let mut headers = vec![("Content-Type", "application/json")];
        if let Some(a) = &auth {
            headers.push(("Authorization", a));
        }
        let (status, _, body) = self.call(method, path, &headers, body.to_string().as_bytes())?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    /// A node call speaking protocol 1; returns (status, JSON).
    fn node(
        &self,
        method: &str,
        path: &str,
        token: &str,
        installation: &str,
        body: &Value,
    ) -> Result<(u16, Value), Box<dyn std::error::Error>> {
        self.node_v("1", method, path, token, installation, body)
    }

    /// A node call speaking the given protocol version; returns (status, JSON).
    fn node_v(
        &self,
        version: &str,
        method: &str,
        path: &str,
        token: &str,
        installation: &str,
        body: &Value,
    ) -> Result<(u16, Value), Box<dyn std::error::Error>> {
        let auth = format!("Bearer {token}");
        let headers = [
            ("Authorization", auth.as_str()),
            ("ContextStack-Protocol", version),
            ("ContextStack-Installation", installation),
            ("Content-Type", "application/json"),
        ];
        let (status, _, body) = self.call(
            method,
            &format!("/api/v1/nodes/me/{path}"),
            &headers,
            body.to_string().as_bytes(),
        )?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }
}

impl Drop for Director {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mode(p: &Path) -> Result<u32, std::io::Error> {
    Ok(std::fs::metadata(p)?.permissions().mode() & 0o777)
}

fn sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[test]
fn local_credential_and_every_request_checked() -> Res {
    let d = Director::start()?;
    assert_eq!(mode(&d.data)?, 0o700);
    assert_eq!(mode(&d.data.join("local-credential"))?, 0o600);
    let token = d.local_token()?;
    let (status, me) = d.client("GET", "/api/v1/client/me", Some(&token), &Value::Null)?;
    assert_eq!((status, &me["owner"]), (200, &json!(true)));
    for path in ["/api/v1/client/me", "/api/v1/client/nodes", "/nowhere", "/"] {
        let (status, body) = d.client("GET", path, None, &Value::Null)?;
        assert_eq!(
            (status, body["code"].as_str()),
            (401, Some("unauthenticated")),
            "{path} from localhost"
        );
    }
    let (status, _) = d.client("GET", "/api/v1/client/nowhere", Some(&token), &Value::Null)?;
    assert_eq!(status, 404, "a credential first, then the route");
    let auth = format!("Bearer {token}");
    for origin in ["https://evil.example", "null", "http://127.0.0.1:9999"] {
        let (status, _, body) = d.call(
            "GET",
            "/api/v1/client/me",
            &[("Authorization", &auth), ("Origin", origin)],
            b"",
        )?;
        assert_eq!(status, 403, "{origin}: {}", String::from_utf8_lossy(&body));
    }
    let (status, _, _) = d.call(
        "GET",
        "/api/v1/client/me",
        &[("Authorization", &auth), ("Sec-Fetch-Site", "cross-site")],
        b"",
    )?;
    assert_eq!(status, 403, "a cross-site fetch without Origin");
    let (status, _, _) = d.call(
        "GET",
        "/api/v1/client/me",
        &[
            ("Authorization", &auth),
            ("Origin", "http://127.0.0.1:8740"),
        ],
        b"",
    )?;
    assert_eq!(status, 200, "an allowed origin");
    Ok(())
}

#[test]
fn pairing_a_device() -> Res {
    let d = Director::start()?;
    let owner = d.local_token()?;
    let count = |d: &Director| -> Result<usize, Box<dyn std::error::Error>> {
        let (_, list) = d.client(
            "GET",
            "/api/v1/client/credentials",
            Some(&owner),
            &Value::Null,
        )?;
        Ok(list["credentials"].as_array().map_or(0, Vec::len))
    };
    assert_eq!(count(&d)?, 1);
    for body in [
        json!({}),
        json!({"code": "AAAAA-AAAAA"}),
        json!({"code": 7}),
    ] {
        let (status, answer) = d.client("POST", "/api/v1/pair", None, &body)?;
        assert_eq!(
            (status, answer["code"].as_str()),
            (403, Some("pairing_refused"))
        );
    }
    assert_eq!(
        count(&d)?,
        1,
        "reaching the pairing endpoint grants nothing"
    );

    let (status, issued) = d.client(
        "POST",
        "/api/v1/client/pairings",
        Some(&owner),
        &json!({"name": "phone"}),
    )?;
    assert_eq!(status, 200, "{issued}");
    let code = issued["code"].as_str().ok_or("code")?;
    let (status, paired) = d.client("POST", "/api/v1/pair", None, &json!({"code": code}))?;
    assert_eq!(status, 200, "{paired}");
    let device = paired["token"].as_str().ok_or("token")?.to_string();
    let (status, _) = d.client("POST", "/api/v1/pair", None, &json!({"code": code}))?;
    assert_eq!(status, 403, "a code works once");

    let (status, me) = d.client("GET", "/api/v1/client/me", Some(&device), &Value::Null)?;
    assert_eq!((status, &me["owner"]), (200, &json!(false)));
    assert_eq!(
        d.client("GET", "/api/v1/client/nodes", Some(&device), &Value::Null)?
            .0,
        200
    );
    for (method, path) in [
        ("POST", "/api/v1/client/pairings"),
        ("GET", "/api/v1/client/credentials"),
    ] {
        let (status, body) = d.client(method, path, Some(&device), &json!({"name": "x"}))?;
        assert_eq!(
            (status, body["code"].as_str()),
            (403, Some("owner_only")),
            "{method} {path}"
        );
    }
    let id = paired["credential_id"].as_str().ok_or("id")?;
    let (status, _) = d.client(
        "DELETE",
        &format!("/api/v1/client/credentials/{id}"),
        Some(&owner),
        &Value::Null,
    )?;
    assert_eq!(status, 200);
    assert_eq!(
        d.client("GET", "/api/v1/client/me", Some(&device), &Value::Null)?
            .0,
        401,
        "revoked"
    );

    // A pairing code issued on the trusted machine itself, by the CLI.
    let issued = d.admin(&["pair", "laptop"])?;
    let (status, _) = d.client(
        "POST",
        "/api/v1/pair",
        None,
        &json!({"code": issued["code"]}),
    )?;
    assert_eq!(status, 200);
    Ok(())
}

#[test]
fn a_node_pulls_a_hand_made_job_and_reports_back() -> Res {
    let d = Director::start()?;
    let owner = d.local_token()?;
    let reg = d.admin(&["node", "register", "node-a"])?;
    let (token, inst) = (
        reg["token"].as_str().ok_or("token")?,
        reg["installation"].as_str().ok_or("inst")?,
    );
    let printed = Command::new(bin_path())
        .arg("installation")
        .arg("--data-dir")
        .arg(&d.data)
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&printed.stdout).trim(),
        inst,
        "registration names the installation"
    );
    d.admin(&["node", "register", "never-seen"])?;

    let bundle = d.tmp.path().join("bundle");
    std::fs::create_dir_all(bundle.join("tasks/hello"))?;
    std::fs::write(bundle.join("spec.yaml"), "id: hand-made\n")?;
    std::fs::write(bundle.join("tasks/hello/goal.md"), "Say done.\n")?;
    let made = d.admin(&[
        "job",
        "create",
        "--node",
        "node-a",
        "--bundle",
        bundle.to_str().ok_or("path")?,
        "--meta",
        "why=step3",
    ])?;
    let job_id = made["job_id"].as_str().ok_or("job")?;

    let (status, jobs) = d.node("GET", "jobs", token, inst, &Value::Null)?;
    assert_eq!(status, 200);
    assert_eq!(jobs["jobs"][0]["job_id"], job_id);
    assert_eq!(jobs["jobs"][0]["meta"], json!({"why": "step3"}));
    let digest = jobs["jobs"][0]["bundle_sha256"].clone();
    let (status, _) = d.node(
        "POST",
        &format!("jobs/{job_id}/accept"),
        token,
        inst,
        &json!({"bundle_sha256": digest}),
    )?;
    assert_eq!(status, 200);

    // The node finishes a run and the job, as spool-send would.
    let run_id = "hand-made-r1";
    let usage = report_run(&d, token, inst, run_id)?;
    let term_row = json!({"state": "done", "director_job": job_id, "run_id": run_id}).to_string();
    let term = json!({"envelope_version": 1, "event_id": format!("job_terminal:{job_id}"), "job_id": job_id, "files": [],
                      "envelope_sha256": sha(b""), "row_sha256": sha(term_row.as_bytes())});
    let (_, answer) = d.node(
        "POST",
        "envelopes",
        token,
        inst,
        &json!({"envelope": term, "row_line": term_row}),
    )?;
    assert_eq!(answer["status"], "acked", "{answer}");

    client_sees(&d, &owner, job_id, run_id, &usage)?;
    let (_, after) = d.node("GET", "jobs", token, inst, &Value::Null)?;
    assert_eq!(
        after,
        json!({"jobs": []}),
        "the finished job left the node's list"
    );

    d.admin(&["node", "revoke", "node-a"])?;
    assert_eq!(
        d.node("GET", "jobs", token, inst, &Value::Null)?.0,
        401,
        "a revoked node token"
    );
    Ok(())
}

/// A node reports a finished run as spool-send would: objects first, then the envelope. Returns its usage.
fn report_run(
    d: &Director,
    token: &str,
    inst: &str,
    run_id: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let files = [
        ("check.json", b"{\"pass\": true}".to_vec()),
        (
            "terminal.json",
            json!({"run_id": run_id}).to_string().into_bytes(),
        ),
        ("transcript.md", b"# done\n".to_vec()),
    ];
    let auth = format!("Bearer {token}");
    for (_, data) in &files {
        let path = format!("/api/v1/nodes/me/objects/{}", sha(data));
        let (status, _, _) = d.call(
            "PUT",
            &path,
            &[
                ("Authorization", &auth),
                ("ContextStack-Protocol", "1"),
                ("ContextStack-Installation", inst),
            ],
            data,
        )?;
        assert_eq!(status, 200);
    }
    let listed: Vec<Value> = files
        .iter()
        .map(|(p, b)| json!({"path": p, "sha256": sha(b), "bytes": b.len()}))
        .collect();
    let lines = files.iter().fold(String::new(), |mut out, (p, b)| {
        let _ = writeln!(out, "{p}:{}", sha(b));
        out
    });
    let envelope_sha = sha(lines.as_bytes());
    let row = json!({"transcript": format!("ledger/runs/{run_id}"), "verdict": "pass"}).to_string();
    let usage = json!({"input_tokens": 10, "output_tokens": 2, "est_usd": 0.001, "est_basis": "rate", "charged_usd": 0,
                       "source": {"class": "free_local", "model": "local-coder"}});
    let env = json!({"envelope_version": 1, "event_id": format!("run_finalized:{run_id}"), "run_id": run_id, "files": listed,
                     "envelope_sha256": envelope_sha, "row_sha256": sha(row.as_bytes()), "usage": usage});
    let (_, answer) = d.node(
        "POST",
        "envelopes",
        token,
        inst,
        &json!({"envelope": env, "row_line": row}),
    )?;
    assert_eq!(answer["status"], "acked", "{answer}");
    Ok(usage)
}

/// What a client sees after the node reported: last-seen and stale, the job done with its run, the run's usage.
fn client_sees(d: &Director, owner: &str, job_id: &str, run_id: &str, usage: &Value) -> Res {
    // What a client sees.
    let (_, nodes) = d.client("GET", "/api/v1/client/nodes", Some(owner), &Value::Null)?;
    let by_name = |n: &str| {
        nodes["nodes"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == n))
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(by_name("node-a")["stale"], false);
    assert!(by_name("node-a")["last_seen_at"].is_string());
    assert_eq!(
        by_name("never-seen")["stale"],
        true,
        "not seen is stale, never stopped"
    );
    let (_, job) = d.client(
        "GET",
        &format!("/api/v1/client/jobs/{job_id}"),
        Some(owner),
        &Value::Null,
    )?;
    assert_eq!(
        (&job["state"], &job["run_id"], &job["node"]),
        (&json!("done"), &json!(run_id), &json!("node-a"))
    );
    let (_, jobs) = d.client(
        "GET",
        "/api/v1/client/jobs?node=node-a",
        Some(owner),
        &Value::Null,
    )?;
    assert_eq!(jobs["jobs"].as_array().map(Vec::len), Some(1));
    let (_, run) = d.client(
        "GET",
        &format!("/api/v1/client/runs/{run_id}"),
        Some(owner),
        &Value::Null,
    )?;
    assert_eq!(
        (&run["usage"], &run["job_id"], &run["verdict"]),
        (usage, &json!(job_id), &json!("pass"))
    );
    assert_eq!(
        d.client("GET", "/api/v1/client/runs/nope", Some(owner), &Value::Null)?
            .0,
        404
    );
    Ok(())
}

#[test]
fn tls_when_configured() -> Res {
    let tools = ["openssl", "curl"]
        .iter()
        .all(|t| Command::new(t).arg("--version").output().is_ok());
    if !tools {
        eprintln!("skipped: openssl and curl are needed to make and check a certificate");
        return Ok(());
    }
    let d = Director::start_with(|dir| {
        let made = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:prime256v1",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=director-test",
                "-addext",
                "subjectAltName=IP:127.0.0.1",
                "-keyout",
            ])
            .arg(dir.join("key.pem"))
            .arg("-out")
            .arg(dir.join("cert.pem"))
            .output()?;
        if !made.status.success() {
            return Err(String::from_utf8_lossy(&made.stderr).into_owned().into());
        }
        Ok(vec![
            format!("--tls-cert={}", dir.join("cert.pem").display()),
            format!("--tls-key={}", dir.join("key.pem").display()),
        ])
    })?;
    let token = d.local_token()?;
    let url = format!("https://127.0.0.1:{}/api/v1/client/me", d.port);
    let out = Command::new("curl")
        .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", "--cacert"])
        .arg(d.tmp.path().join("cert.pem"))
        .args(["-H", &format!("Authorization: Bearer {token}"), &url])
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "200",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        d.call("GET", "/api/v1/client/me", &[], b"").is_err(),
        "plain HTTP gets no answer from a TLS listener"
    );
    Ok(())
}

#[test]
fn protocol_2_a_held_request_carries_a_cancel_and_the_node_settles_it() -> Res {
    let d = Director::start()?;
    let owner = d.local_token()?;
    let reg = d.admin(&["node", "register", "node-b"])?;
    let (token, inst) = (
        reg["token"].as_str().ok_or("token")?,
        reg["installation"].as_str().ok_or("inst")?,
    );
    let bundle = d.tmp.path().join("bundle");
    std::fs::create_dir_all(bundle.join("tasks/hello"))?;
    std::fs::write(bundle.join("spec.yaml"), "id: p2\n")?;
    std::fs::write(bundle.join("tasks/hello/goal.md"), "Say done.\n")?;
    let made = d.admin(&[
        "job",
        "create",
        "--node",
        "node-b",
        "--bundle",
        bundle.to_str().ok_or("path")?,
    ])?;
    let job_id = made["job_id"].as_str().ok_or("job")?.to_string();

    let (status, _) = d.node("GET", "control", token, inst, &Value::Null)?;
    assert_eq!(
        status, 404,
        "protocol 2's calls do not exist under protocol 1"
    );
    let (status, first) = d.node_v("2", "GET", "work", token, inst, &Value::Null)?;
    assert_eq!((status, &first["changed"]), (200, &Value::Bool(true)));
    let cursor = first["cursor"].as_str().ok_or("cursor")?.to_string();

    let held = std::thread::scope(|s| -> Result<Value, String> {
        let waiting = s.spawn(|| {
            d.node_v(
                "2",
                "GET",
                &format!("work?after={cursor}"),
                token,
                inst,
                &Value::Null,
            )
            .map_err(|e| e.to_string())
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let auth = format!("Bearer {owner}");
        let (status, _, _) = d
            .call(
                "POST",
                &format!("/api/v1/client/jobs/{job_id}/cancel"),
                &[("Authorization", &auth), ("Idempotency-Key", "p2-cancel")],
                b"{}",
            )
            .map_err(|e| e.to_string())?;
        assert_eq!(status, 202);
        let (status, answer) = waiting.join().map_err(|_| "the held request panicked")??;
        assert_eq!(status, 200);
        Ok(answer)
    })?;
    assert_eq!(held["changed"], true, "the cancel woke the held request");
    let message = &held["control"][0];
    assert_eq!(
        (&message["kind"], &message["body"]["job_id"]),
        (&json!("cancel"), &json!(job_id))
    );
    let control_id = message["control_id"].as_str().ok_or("control_id")?;
    let report = json!({"state": "applied", "outcome": "stopped"});
    let (status, _) = d.node_v(
        "2",
        "POST",
        &format!("control/{control_id}"),
        token,
        inst,
        &report,
    )?;
    assert_eq!(status, 200);
    let (_, cancels) = d.client(
        "GET",
        &format!("/api/v1/client/jobs/{job_id}"),
        Some(&owner),
        &Value::Null,
    )?;
    assert_eq!(cancels["cancel"]["outcome"], "stopped");
    let (_, node) = d.client(
        "GET",
        "/api/v1/client/nodes/node-b",
        Some(&owner),
        &Value::Null,
    )?;
    assert_eq!(node["protocol"], 2);
    Ok(())
}
