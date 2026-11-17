//! Lab work orders (`POST /v1/ai/lab-execute`): the gateway owns the permit and the process. Clients
//! never choose a command: the child is always `<python3> <lab_executor> --gateway-owned`, with the
//! compact work order on stdin, an allowlisted environment, and its own process group.
//!
//! Ownership: once admitted, the permit, the child and the run's registry handle move into one
//! supervising task. The HTTP handler only holds a guard; if the caller goes away the guard cancels the
//! task, which terminates the whole process group, waits for it (bounded), kills what remains, reaps,
//! and only then drops the permit and then the handle. A cancel request (`labrun`) takes the same path
//! and the reply is the cancelled outcome. Dropping a Tokio `Child` would kill nothing, and dropping a
//! `JoinHandle` would detach the task, so neither is relied on.

use crate::admission::{Manager, Permit, Refusal, Scope};
use crate::labrun::{Handle, Outcome};
use rustix::process::{Pid, Signal, kill_process_group};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Go's `cmd.WaitDelay`: how long output pipes and a signalled child get before the gateway stops waiting.
const WAIT_DELAY: Duration = Duration::from_secs(5);
/// The request body limit.
pub const MAX_BODY: usize = 2 << 20;

/// The gateway's own environment the Lab executor inherits: the basics, plus the node's roots
/// (`LAB_*_ROOT`, set by the installed plist) so a coding-agent run lands in the node's state root,
/// never under the release's code root.
const ALLOWED_ENV: [&str; 18] = [
    "LAB_STATE_ROOT",
    "LAB_CONFIG_ROOT",
    "LAB_WORKSPACES_ROOT",
    "LAB_CODE_ROOT",
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SHELL",
    "HOMEBREW_PREFIX",
    "HOMEBREW_CELLAR",
];
const LAB_KEYS: [&str; 6] = [
    "LAB_JOB_ID",
    "LAB_TRIAL",
    "LAB_SCHEDULE_DATE",
    "LAB_SCHEDULE_SLOT",
    "LAB_SCHEDULE_TIMEZONE",
    "LAB_VIDEO_MUTATION",
];

/// The configured executor.
#[derive(Debug, Clone)]
pub struct Executor {
    interpreter: PathBuf,
    script: PathBuf,
    /// The child's base environment (allowlisted names the gateway has; PATH is the resolver's).
    env: Vec<(String, String)>,
}

impl Executor {
    /// Validate the script (absolute, a regular file) and capture the allowlisted environment.
    pub fn new(
        script: &str,
        interpreter: String,
        settings: &crate::settings::Settings,
        search_path: &str,
    ) -> Result<Self, String> {
        let script = PathBuf::from(script);
        if !script.is_absolute() {
            return Err("Lab executor path must be absolute".into());
        }
        let meta = std::fs::metadata(&script)
            .map_err(|e| format!("stat {}: {}", script.display(), crate::server::os_error(&e)))?;
        if !meta.is_file() {
            return Err("Lab executor must be a regular file".into());
        }
        let interpreter = PathBuf::from(interpreter);
        if !interpreter.is_absolute() {
            return Err("Lab executor interpreter must be an absolute path".into());
        }
        let mut env = Vec::new();
        for key in ALLOWED_ENV {
            let value = if key == "PATH" {
                Some(search_path.to_string())
            } else {
                settings.env.raw(key)
            };
            if let Some(v) = value {
                env.push((key.to_string(), v));
            }
        }
        Ok(Self {
            interpreter,
            script,
            env,
        })
    }
}

/// The work order's fields the gateway reads.
#[derive(serde::Deserialize)]
struct Order {
    #[serde(default)]
    executor: String,
    #[serde(default)]
    timeout_s: i64,
}

/// A parsed request ready to admit.
pub struct Request {
    order: String,
    /// The work order's executor name.
    pub executor: String,
    timeout: Duration,
    lab: Vec<(String, String)>,
    /// `stream: true`: live output as SSE.
    pub stream: bool,
    /// The caller's `execution_id`, when it chose one.
    pub execution_id: Option<String>,
}

impl Request {
    /// The work order's `LAB_JOB_ID` (from `environment`), when sent.
    pub fn job(&self) -> Option<&str> {
        self.lab
            .iter()
            .find(|(k, v)| k == "LAB_JOB_ID" && !v.is_empty())
            .map(|(_, v)| v.as_str())
    }
}

/// Lines of the child's stdout, as they arrive (streamed work orders).
pub type LineSink = UnboundedSender<Vec<u8>>;

/// An HTTP reply: status, content type, body.
pub type HttpReply = (u16, &'static str, String);

fn text(status: u16, msg: &str) -> HttpReply {
    (status, "text/plain; charset=utf-8", format!("{msg}\n"))
}

/// Parse and validate the body (400s), before any admission or child.
pub fn parse(body: &[u8]) -> Result<Request, HttpReply> {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(default)]
        work_order: Option<Box<serde_json::value::RawValue>>,
        #[serde(default)]
        environment: Option<std::collections::HashMap<String, String>>,
        #[serde(default)]
        stream: bool,
        #[serde(default)]
        execution_id: Option<String>,
    }
    if body.len() > MAX_BODY {
        return Err(text(400, "invalid work order"));
    }
    let b: Body =
        crate::protocol::openai::first_value(body).map_err(|_| text(400, "invalid work order"))?;
    let raw = b
        .work_order
        .map_or_else(|| "null".into(), |r| r.get().to_string());
    let order = serde_json::from_str::<Order>(&raw)
        .ok()
        .filter(|o| !o.executor.is_empty() && (0..=3600).contains(&o.timeout_s))
        .ok_or_else(|| text(400, "invalid executor or timeout"))?;
    let compact = serde_json::from_str::<serde_json::Value>(&raw)
        .map(|v| compact_preserving(&raw, &v))
        .unwrap_or(raw);
    if b.execution_id
        .as_deref()
        .is_some_and(|id| !crate::labrun::valid_id(id))
    {
        return Err(text(400, "invalid execution_id"));
    }
    let env = b.environment.unwrap_or_default();
    let lab = LAB_KEYS
        .iter()
        .map(|k| ((*k).to_string(), env.get(*k).cloned().unwrap_or_default()))
        .collect();
    let timeout_s = if order.timeout_s == 0 {
        300
    } else {
        order.timeout_s
    };
    let timeout = Duration::from_secs(u64::try_from(3 * timeout_s + 120).unwrap_or(1020));
    Ok(Request {
        order: compact,
        executor: order.executor,
        timeout,
        lab,
        stream: b.stream,
        execution_id: b.execution_id,
    })
}

/// The caller's JSON with insignificant whitespace removed and key order kept (Go's `json.Compact`).
fn compact_preserving(raw: &str, _parsed: &serde_json::Value) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// The reply for a cancelled run: what the child wrote before it was stopped. Not a fault: HTTP 200,
/// `outcome: "cancelled"`; `exit_code` -1 when it never exited on its own (or never started).
fn cancelled(out: &[u8], err: &[u8], code: i32) -> HttpReply {
    let body = serde_json::json!({
        "stdout": String::from_utf8_lossy(out),
        "stderr": String::from_utf8_lossy(err),
        "exit_code": code,
        "outcome": Outcome::Cancelled.as_str(),
    });
    (200, "application/json", format!("{body}\n"))
}

/// Admit, run under supervision, and reply. `disconnect` is the caller going away (for a unary call the
/// route's drop guard; for a stream, the SSE body's); `run.cancel` is a cancel request. Either one stops
/// the process tree before the permit is released. `lines` receives stdout lines as they arrive.
pub async fn execute(
    manager: &Arc<Manager>,
    executor: &Executor,
    req: Request,
    mut run: Handle,
    disconnect: CancellationToken,
    lines: Option<LineSink>,
) -> HttpReply {
    let deadline = Instant::now() + req.timeout;
    let admitted = tokio::select! {
        r = tokio::time::timeout_at(deadline, manager.acquire("cli", &req.executor, Scope::LabWorkOrder, "")) => r,
        () = run.cancel.cancelled() => {
            run.set_outcome(Outcome::Cancelled);
            return cancelled(b"", b"", -1);
        }
        () = disconnect.cancelled() => return text(504, "context canceled"),
    };
    let permit = match admitted {
        Err(_) => {
            run.set_outcome(Outcome::Failed);
            return text(504, "context deadline exceeded");
        }
        Ok(Err(Refusal(msg))) => {
            run.set_outcome(Outcome::Failed);
            return text(503, &msg);
        }
        Ok(Ok(p)) => p,
    };
    let executor = executor.clone();
    let task = tokio::spawn(supervise(
        executor, req, deadline, disconnect, permit, run, lines,
    ));
    task.await.unwrap_or_else(|e| text(502, &e.to_string()))
}

/// Signal the child's process group (the child leads it).
fn signal_group(pgid: Option<u32>, sig: Signal) {
    if let Some(pid) = pgid
        .and_then(|p| i32::try_from(p).ok())
        .and_then(Pid::from_raw)
    {
        let _ = kill_process_group(pid, sig);
    }
}

/// Read a pipe to its end, passing each line to `sink` as it arrives.
async fn drain(pipe: Option<impl AsyncRead + Unpin>, sink: Option<LineSink>) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(p) = pipe else { return out };
    let mut r = BufReader::new(p);
    loop {
        let start = out.len();
        match r.read_until(b'\n', &mut out).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if let Some(s) = &sink {
                    let _ = s.send(out[start..].to_vec());
                }
            }
        }
    }
    out
}

/// Why the supervisor stopped the child.
#[derive(Clone, Copy)]
enum Stop {
    Disconnect,
    Deadline,
    Cancel,
}

async fn supervise(
    executor: Executor,
    req: Request,
    deadline: Instant,
    disconnect: CancellationToken,
    permit: Option<Permit>,
    mut run: Handle,
    lines: Option<LineSink>,
) -> HttpReply {
    let mut cmd = tokio::process::Command::new(&executor.interpreter);
    cmd.arg(&executor.script)
        .arg("--gateway-owned")
        .env_clear()
        .envs(executor.env.iter().map(|(k, v)| (k, v)))
        .envs(req.lab.iter().map(|(k, v)| (k, v)))
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            run.set_outcome(Outcome::Failed);
            return text(
                502,
                &format!(
                    "fork/exec {}: {}",
                    executor.interpreter.display(),
                    crate::server::os_error(&e)
                ),
            );
        }
    };
    let pgid = child.id();
    // Stdin stays open while the child runs: gateway death is visible to it as EOF.
    let mut stdin = child.stdin.take();
    if let Some(pipe) = stdin.as_mut()
        && pipe
            .write_all(format!("{}\n", req.order.trim()).as_bytes())
            .await
            .is_err()
    {
        signal_group(pgid, Signal::TERM);
    }
    let stdout = tokio::spawn(drain(child.stdout.take(), lines));
    let stderr = tokio::spawn(drain(child.stderr.take(), None));
    let mut stopped: Option<Stop> = None;
    let status = tokio::select! {
        s = child.wait() => s.ok(),
        () = disconnect.cancelled() => { stopped = Some(Stop::Disconnect); None }
        () = run.cancel.cancelled() => { stopped = Some(Stop::Cancel); None }
        () = tokio::time::sleep_until(deadline) => { stopped = Some(Stop::Deadline); None }
    };
    let status = if let Some(s) = status {
        Some(s)
    } else {
        signal_group(pgid, Signal::TERM);
        if let Ok(s) = tokio::time::timeout(WAIT_DELAY, child.wait()).await {
            s.ok()
        } else {
            signal_group(pgid, Signal::KILL);
            child.wait().await.ok()
        }
    };
    // Output is complete when every holder of the pipes has closed them; a surviving descendant may hold
    // them open, so wait at most WAIT_DELAY after the exit.
    let io_deadline = Instant::now() + WAIT_DELAY;
    let mut wait_delay_expired = false;
    let mut collect =
        async |h: tokio::task::JoinHandle<Vec<u8>>| match tokio::time::timeout_at(io_deadline, h)
            .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => Vec::new(),
            Err(_) => {
                wait_delay_expired = true;
                Vec::new()
            }
        };
    let (out_handle, err_handle) = (stdout, stderr);
    let out_abort = out_handle.abort_handle();
    let err_abort = err_handle.abort_handle();
    let out = collect(out_handle).await;
    let err = collect(err_handle).await;
    out_abort.abort();
    err_abort.abort();
    // Reap children holding inherited pipes even after a failed parent.
    signal_group(pgid, Signal::KILL);
    drop(stdin);
    drop(permit);
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    let (outcome, reply) = reply(stopped, wait_delay_expired, code, &out, &err);
    if let Some(o) = outcome {
        run.set_outcome(o);
    }
    reply
}

/// The reply and the outcome (`None`: the caller went away) once the child is reaped.
fn reply(
    stopped: Option<Stop>,
    wait_delay_expired: bool,
    code: i32,
    out: &[u8],
    err: &[u8],
) -> (Option<Outcome>, HttpReply) {
    match stopped {
        Some(Stop::Cancel) => return (Some(Outcome::Cancelled), cancelled(out, err, code)),
        Some(Stop::Disconnect) => return (None, text(504, "context canceled")),
        Some(Stop::Deadline) => {
            return (
                Some(Outcome::Failed),
                text(504, "context deadline exceeded"),
            );
        }
        None => {}
    }
    if wait_delay_expired && code == 0 {
        return (
            Some(Outcome::Failed),
            text(502, "exec: WaitDelay expired before I/O complete"),
        );
    }
    let body = serde_json::json!({
        "stdout": String::from_utf8_lossy(out),
        "stderr": String::from_utf8_lossy(err),
        "exit_code": code,
    });
    (
        Some(Outcome::Completed),
        (200, "application/json", format!("{body}\n")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_refusals_and_compaction() {
        let bad = |b: &[u8]| parse(b).err().map(|r| (r.0, r.2));
        assert_eq!(bad(b"{"), Some((400, "invalid work order\n".into())));
        assert_eq!(
            bad(b"{}"),
            Some((400, "invalid executor or timeout\n".into()))
        );
        assert_eq!(
            bad(br#"{"work_order":{"executor":"x","timeout_s":3601}}"#),
            Some((400, "invalid executor or timeout\n".into()))
        );
        assert_eq!(
            bad(br#"{"work_order":[1]}"#),
            Some((400, "invalid executor or timeout\n".into()))
        );
        let ok = parse(br#"{"work_order": {"executor": "x", "note": "a b", "timeout_s": 5}}"#);
        assert_eq!(
            ok.ok().map(|r| r.order),
            Some(r#"{"executor":"x","note":"a b","timeout_s":5}"#.into())
        );
    }
}
