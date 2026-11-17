//! Temporary in-process estates for runner coverage (mirrors harness/tests/quality.py).

#![allow(dead_code)]

use cs_harness::{
    gateway::Gateway,
    process::Control,
    runner,
    spec::{self, Spec},
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

const REAL_OPS: &[&str] = &[
    "setup-workspace",
    "capture-stimulus",
    "finalize-run",
    "publish-evidence",
];
const OP_HELPERS: &[&str] = &[
    "_estate_io.py",
    "_lifecycle_evidence.py",
    "_harvest_git.py",
    "_setup_workspace.py",
    "_corpus_contract.py",
    "_workspace_setup.py",
    "_env_policy.py",
    "_roots.py",
    "_sandbox_policy.py",
    "_runner.py",
    "_errors.py",
];

fn lab() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../cs")
}

fn fixture_dir() -> PathBuf {
    lab().join("tasks/regressions/runner-characterisation-v2/fixture")
}

fn copy_exec(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::copy(from, to).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !to.extension().is_some_and(|e| e == "json" || e == "py") {
            fs::set_permissions(to, fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// A minimal runnable estate under a temporary directory.
pub struct Lab {
    pub _guard: TempDir,
    pub root: PathBuf,
}

impl Lab {
    pub fn build() -> Result<Self, String> {
        let guard = TempDir::new().map_err(|e| e.to_string())?;
        let root = guard.path().to_path_buf();
        let src = lab();
        let here = fixture_dir();
        for sub in [
            "ledger",
            "prompts",
            "tools/bin",
            "hooks/bin",
            "ops/bin",
            "ops/real-bin",
            "specs",
            "tasks",
            "fixture",
            "home",
            "tmp",
        ] {
            fs::create_dir_all(root.join(sub)).map_err(|e| e.to_string())?;
        }
        fs::write(root.join("INVARIANTS.md"), "# fixture estate\n").map_err(|e| e.to_string())?;
        fs::write(root.join("ESTATE"), "dev\n").map_err(|e| e.to_string())?;
        copy_exec(
            &src.join("prompts/coder-min.md"),
            &root.join("prompts/coder-min.md"),
        )?;
        copy_exec(&here.join("fx_tool"), &root.join("tools/bin/fx_tool"))?;
        copy_exec(
            &here.join("fx_tool.json"),
            &root.join("tools/bin/fx_tool.json"),
        )?;
        for name in REAL_OPS {
            let real = src.join("ops/bin").join(name);
            if real.is_file() {
                copy_exec(&real, &root.join("ops/real-bin").join(name))?;
                copy_exec(&here.join("op_shim.py"), &root.join("ops/bin").join(name))?;
            }
        }
        for name in OP_HELPERS {
            let helper = src.join("ops/bin").join(name);
            if helper.is_file() {
                for dest in ["ops/real-bin", "ops/bin"] {
                    copy_exec(&helper, &root.join(dest).join(name))?;
                }
            }
        }
        let task = root.join("tasks/fx");
        fs::create_dir_all(&task).map_err(|e| e.to_string())?;
        fs::write(task.join("goal.md"), "Say done.\n").map_err(|e| e.to_string())?;
        fs::write(task.join("check"), "#!/bin/sh\nexit 0\n").map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(task.join("check"), fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            _guard: guard,
            root,
        })
    }

    pub fn write_spec(&self, yaml: &str) -> Result<Spec, String> {
        let path = self.root.join("specs/fx.yaml");
        fs::write(&path, yaml).map_err(|e| e.to_string())?;
        spec::load(path.to_str().ok_or("non-UTF8 path")?)
    }

    pub fn spec(&self) -> Result<Spec, String> {
        self.write_spec(
            "id: fx-estate\nmodel: fixture\nsystem: prompts/coder-min.md\ntools: [fx_tool]\npost_turn: []\npre_tool: []\nbudget: {turns: 3, tool_calls: 5, usd: 0, seconds: 30}\ntasks: [tasks/fx]\ntrials: 1\nhypothesis: 'fixture: in-process fixture'\n",
        )
    }

    pub fn hook(&self, name: &str, body: &str) -> Result<(), String> {
        let path = self.root.join("hooks/bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn tool_script(&self, body: &str) -> Result<(), String> {
        fs::write(
            self.root.join("tools/bin/fx_tool"),
            format!("#!/bin/sh\n{body}\n"),
        )
        .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                self.root.join("tools/bin/fx_tool"),
                fs::Permissions::from_mode(0o755),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn terminal(&self) -> Result<Value, String> {
        let runs = self.root.join("ledger/runs");
        let mut dirs = fs::read_dir(&runs).map_err(|e| e.to_string())?;
        let entry = dirs
            .next()
            .ok_or("no run directory")?
            .map_err(|e| e.to_string())?;
        let path = entry.path().join("terminal.json");
        let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
        serde_json::from_str(&raw).map_err(|e| e.to_string())
    }
}

pub const OK: &str = r#"{"status":"ok","model":"fixture","message":{"content":"done"},"tool_calls":[],"cost":{"class":"free_local","est_usd":0,"charged_usd":0}}"#;

pub fn tool_reply(text: &str, args: &str) -> String {
    format!(
        r#"{{"status":"ok","model":"fixture","message":{{"content":{text:?}}},"tool_calls":[{{"id":"1","name":"fx_tool","args":{args}}}],"cost":{{"class":"free_local","est_usd":0,"charged_usd":0}}}}"#
    )
}

async fn mock_gateway(
    bodies: Vec<String>,
) -> Result<(String, tokio::task::JoinHandle<()>), String> {
    mock_gateway_observed(bodies, None).await
}

pub async fn mock_gateway_observed(
    bodies: Vec<String>,
    received: Option<Arc<tokio::sync::Notify>>,
) -> Result<(String, tokio::task::JoinHandle<()>), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let queue = Arc::new(Mutex::new(VecDeque::from(bodies)));
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let Ok(n) = socket.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_lowercase();
                if text.split_once("\r\n\r\n").is_some_and(|(head, rest)| {
                    head.starts_with("get ")
                        || head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|n| n.trim().parse().ok())
                            .is_some_and(|len| rest.len() >= len)
                }) {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buf);
            let (body, content_type) = if request.starts_with("GET ") {
                (
                    serde_json::json!({"models": []}).to_string(),
                    "application/json",
                )
            } else {
                let body = queue.lock().ok().and_then(|mut q| q.pop_front()).unwrap_or_else(|| {
                    r#"{"status":"error","error":{"class":"fatal","code":"unexpected","message":"no scripted reply"}}"#
                        .into()
                });
                (body, "application/json")
            };
            if body == "__hang__" {
                if let Some(received) = &received {
                    received.notify_one();
                }
                while let Ok(n) = socket.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                }
                continue;
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    Ok((format!("http://{address}/v1/agent/model.call"), task))
}

pub async fn mock_gateway_for_test(
    bodies: Vec<String>,
) -> Result<(String, tokio::task::JoinHandle<()>), String> {
    mock_gateway(bodies).await
}

pub async fn drive(
    lab: &Lab,
    spec: &Spec,
    bodies: Vec<String>,
) -> Result<runner::ResultRow, String> {
    drive_observed(lab, spec, bodies, None).await
}

pub async fn drive_observed(
    lab: &Lab,
    spec: &Spec,
    bodies: Vec<String>,
    received: Option<Arc<tokio::sync::Notify>>,
) -> Result<runner::ResultRow, String> {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let harness = std::env::var("CARGO_BIN_EXE_cs-harness").unwrap_or_default();
    let mut env = vec![
        ("PATH".to_string(), path),
        (
            "HOME".to_string(),
            lab.root.join("home").to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".to_string(),
            lab.root.join("tmp").to_string_lossy().into_owned(),
        ),
        ("LANG".to_string(), "en_US.UTF-8".to_string()),
        (
            "LAB_STATE_ROOT".to_string(),
            lab.root.join("ledger").to_string_lossy().into_owned(),
        ),
        (
            "LAB_CODE_ROOT".to_string(),
            lab.root.to_string_lossy().into_owned(),
        ),
        (
            "LAB_CONFIG_ROOT".to_string(),
            lab.root.to_string_lossy().into_owned(),
        ),
    ];
    if !harness.is_empty() {
        env.push(("CS_RUNNER_PATH".to_string(), harness.clone()));
        env.push(("CS_RUNNER_BIN".to_string(), harness));
    }
    let _env = cs_harness::process::ScopedEnv::set(env);
    let (endpoint, server) = mock_gateway_observed(bodies, received).await?;
    let gateway = Gateway::new(endpoint, spec.model.clone(), spec.id.clone())?;
    let row = runner::run(spec, "tasks/fx", 1, &lab.root, &gateway, Control::default()).await;
    server.abort();
    Ok(row)
}

const BASE: &str =
    "id: fx-estate\nmodel: fixture\nsystem: prompts/coder-min.md\ntools: [fx_tool]\n";
const TAIL: &str = "tasks: [tasks/fx]\ntrials: 1\nhypothesis: fixture\n";

pub fn yaml(budget: &str, middle: &str) -> String {
    let hooks = if middle.is_empty() {
        "post_turn: []\npre_tool: []\n"
    } else {
        middle
    };
    format!("{BASE}{hooks}budget: {budget}\n{TAIL}")
}

pub async fn finish(
    yaml: &str,
    bodies: Vec<String>,
) -> Result<(runner::ResultRow, Value, Option<String>), String> {
    let lab = Lab::build()?;
    let spec = lab.write_spec(yaml)?;
    let row = drive(&lab, &spec, bodies).await?;
    let terminal = lab.terminal()?;
    let runs = lab.root.join("ledger/runs");
    let entry = fs::read_dir(&runs)
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no run directory")?
        .map_err(|e| e.to_string())?;
    let messages = fs::read_to_string(entry.path().join("messages.jsonl")).ok();
    Ok((row, terminal, messages))
}
