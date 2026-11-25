//! CLI work orders (`POST /v1/agent/cli.run`): the gateway owns the permit and the process. Clients
//! never choose a command: the child is always `<python3> <cli_executor> --gateway-owned --execution-only`, with the
//! compact work order on stdin, an allowlisted environment, and its own process group.
//!
//! Ownership: once admitted, the permit, the child and the run's registry handle move into one
//! supervising task. The HTTP handler only holds a guard; if the caller goes away the guard cancels the
//! task, which terminates the whole process group, waits for it (bounded), kills what remains, reaps,
//! and only then drops the permit and then the handle. A cancel request (`clirun`) takes the same path
//! and the reply is the cancelled outcome. Dropping a Tokio `Child` would kill nothing, and dropping a
//! `JoinHandle` would detach the task, so neither is relied on.

use crate::admission::{Manager, Permit, Refusal, Scope};
use crate::clirun::{Handle, Outcome};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Go's `cmd.WaitDelay`: how long output pipes and a signalled child get before the gateway stops waiting.
pub(crate) const WAIT_DELAY: Duration = Duration::from_secs(5);
// Receipt children drain supervised descendants and persist terminal metadata
// on TERM. Their tracking cleanup can take up to 15 seconds.
pub(crate) const RECEIPT_STOP_GRACE: Duration = Duration::from_secs(18);
/// The request body limit.
pub const MAX_BODY: usize = 2 << 20;

/// The gateway's own environment the CLI executor inherits: the basics, plus the node's roots
/// (`CS_*_ROOT`, set by the installed plist) so a coding-agent run lands in the node's state root,
/// never under the release's code root.
const ALLOWED_ENV: [&str; 18] = [
    "CS_STATE_ROOT",
    "CS_CONFIG_ROOT",
    "CS_WORKSPACES_ROOT",
    "CS_CODE_ROOT",
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
const WORK_ORDER_ENV_KEYS: [&str; 6] = [
    "CS_JOB_ID",
    "CS_TRIAL",
    "CS_SCHEDULE_DATE",
    "CS_SCHEDULE_SLOT",
    "CS_SCHEDULE_TIMEZONE",
    "CS_VIDEO_MUTATION",
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
            return Err("CLI executor path must be absolute".into());
        }
        let meta = std::fs::metadata(&script)
            .map_err(|e| format!("stat {}: {}", script.display(), crate::server::os_error(&e)))?;
        if !meta.is_file() {
            return Err("CLI executor must be a regular file".into());
        }
        let interpreter = PathBuf::from(interpreter);
        if !interpreter.is_absolute() {
            return Err("CLI executor interpreter must be an absolute path".into());
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
    receipt: Option<ReceiptRequest>,
    work_order_env: Vec<(String, String)>,
    /// `stream: true`: live output as SSE.
    pub stream: bool,
    /// The caller's `execution_id`, when it chose one.
    pub execution_id: Option<String>,
}

impl Request {
    /// Whether this caller requested the versioned execution receipt.
    pub fn wants_receipt(&self) -> bool {
        self.receipt.is_some()
    }

    /// The work order's `CS_JOB_ID` (from `environment`), when sent.
    pub fn job(&self) -> Option<&str> {
        self.work_order_env
            .iter()
            .find(|(k, v)| k == "CS_JOB_ID" && !v.is_empty())
            .map(|(_, v)| v.as_str())
    }
}

/// Lines of the child's stdout, as they arrive (streamed work orders).
pub type LineSink = Sender<Vec<u8>>;

/// An HTTP reply: status, content type, body.
pub type HttpReply = (u16, &'static str, String);

fn text(status: u16, msg: &str) -> HttpReply {
    (status, "text/plain; charset=utf-8", format!("{msg}\n"))
}

const MAX_RECEIPT: u64 = 64 * 1024;

#[derive(Deserialize)]
struct ReceiptRequest {
    run_id: String,
    trial: i64,
}

impl ReceiptRequest {
    fn read(
        &self,
        file: &std::fs::File,
        execution_id: &str,
        job_id: Option<&str>,
    ) -> Result<Option<Value>, ()> {
        let mut bytes = Vec::new();
        file.take(MAX_RECEIPT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if u64::try_from(bytes.len()).map_err(|_| ())? > MAX_RECEIPT {
            return Err(());
        }
        if bytes.is_empty() {
            return Ok(None);
        }
        let receipt: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
        if receipt["version"].as_u64() != Some(1)
            || receipt["adapter"].as_str() != Some("executor")
            || receipt["run_id"].as_str() != Some(self.run_id.as_str())
            || receipt["execution_id"].as_str() != Some(execution_id)
            || receipt["job_id"].as_str() != job_id
            || receipt["trial"].as_i64() != Some(self.trial)
            || receipt["native"]["stop"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty())
            || !receipt["native"]
                .get("exit")
                .is_some_and(|exit| exit.is_null() || exit.is_i64())
            || !receipt["usage"].is_object()
        {
            return Err(());
        }
        Ok(Some(receipt))
    }
}

impl Request {
    fn with_receipt(
        &self,
        reply: HttpReply,
        run: &Handle,
        channel: Option<&tempfile::NamedTempFile>,
    ) -> HttpReply {
        let Some(binding) = &self.receipt else {
            return reply;
        };
        if reply.0 != 200 {
            return reply;
        }
        let Ok(mut body) = serde_json::from_str::<Value>(&reply.2) else {
            return text(502, "invalid CLI terminal reply");
        };
        let native = channel.map_or(Ok(None), |file| {
            binding.read(file.as_file(), run.id(), self.job())
        });
        let native = match native {
            Ok(Some(native)) => Some(native),
            Ok(None)
                if matches!(body["outcome"].as_str(), Some("cancelled" | "timed_out"))
                    || body["exit_code"].as_i64().is_some_and(|code| code != 0) =>
            {
                None
            }
            _ => return text(502, "invalid or missing CLI execution receipt"),
        };
        body["receipt"] = json!(native);
        (200, "application/json", format!("{body}\n"))
    }
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
        receipt: Option<ReceiptRequest>,
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
    let fields = serde_json::from_str::<Value>(&raw).ok();
    if b.execution_id
        .as_deref()
        .is_some_and(|id| !crate::clirun::valid_id(id))
    {
        return Err(text(400, "invalid execution_id"));
    }
    let env = b.environment.unwrap_or_default();
    if let Some(binding) = &b.receipt {
        let order_trial = fields.as_ref().and_then(|fields| fields["trial"].as_i64());
        if !crate::clirun::valid_id(&binding.run_id)
            || !binding
                .run_id
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_digit)
            || binding.run_id.contains(':')
            || binding.trial <= 0
            || order_trial != Some(binding.trial)
            || env.get("CS_TRIAL").and_then(|t| t.parse().ok()) != Some(binding.trial)
            || !env
                .get("CS_JOB_ID")
                .is_some_and(|j| crate::clirun::valid_id(j))
            || b.execution_id.is_none()
        {
            return Err(text(400, "invalid CLI receipt binding"));
        }
    }
    let work_order_env = WORK_ORDER_ENV_KEYS
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
        order: fields.map_or_else(|| raw.clone(), |_| compact_preserving(&raw)),
        // The raw order keeps the selection for the executor. Admission and streaming
        // share the base template's capacity and dialect across its model choices.
        executor: order.executor.split(':').next().unwrap_or_default().into(),
        timeout,
        receipt: b.receipt,
        work_order_env,
        stream: b.stream,
        execution_id: b.execution_id,
    })
}

/// The caller's JSON with insignificant whitespace removed and key order kept (Go's `json.Compact`).
fn compact_preserving(raw: &str) -> String {
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
        biased;
        () = run.cancel.cancelled() => {
            run.set_outcome(Outcome::Cancelled);
            return req.with_receipt(cancelled(b"", b"", -1), &run, None);
        }
        () = disconnect.cancelled() => return text(504, "context canceled"),
        r = tokio::time::timeout_at(deadline, manager.acquire("cli", &req.executor, Scope::CliWorkOrder, "")) => r,
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
fn signal_group(pgid: Pid, sig: Signal) {
    let _ = kill_process_group(pgid, sig);
}

// Inspect the exit without reaping the group leader, keeping its PID reserved until the last signal.
async fn wait_for_exit(pid: Pid) -> bool {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
    loop {
        match waitid(WaitId::Pid(pid), options) {
            Ok(Some(_)) => return true,
            Err(e) if e == rustix::io::Errno::CHILD => return false,
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const OUTPUT_TAIL: usize = 8 * 1024 * 1024;
const MAX_EVENT_LINE: usize = 1024 * 1024;

#[derive(Default)]
struct Drain {
    tail: Arc<Mutex<VecDeque<u8>>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drain {
    fn start(
        pipe: Option<impl AsyncRead + Unpin + Send + 'static>,
        sink: Option<LineSink>,
    ) -> Self {
        let mut drain = Self::default();
        if let Some(pipe) = pipe {
            drain.task = Some(tokio::spawn(read_pipe(pipe, sink, Arc::clone(&drain.tail))));
        }
        drain
    }

    async fn collect(self, deadline: Instant) -> (Vec<u8>, bool) {
        let mut expired = false;
        if let Some(mut task) = self.task
            && tokio::time::timeout_at(deadline, &mut task).await.is_err()
        {
            expired = true;
            task.abort();
            let _ = task.await;
        }
        let tail = std::mem::take(&mut *self.tail.lock().unwrap_or_else(PoisonError::into_inner));
        (tail.into(), expired)
    }
}

/// Capture bytes as they arrive, even when a writer never finishes a line or closes its pipe.
async fn read_pipe(
    mut pipe: impl AsyncRead + Unpin,
    sink: Option<LineSink>,
    tail: Arc<Mutex<VecDeque<u8>>>,
) {
    let mut buf = [0u8; 8192];
    let mut line = Vec::new();
    let mut oversized = false;
    while let Ok(n @ 1..) = pipe.read(&mut buf).await {
        let bytes = &buf[..n];
        {
            let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
            let excess = (tail.len() + n).saturating_sub(OUTPUT_TAIL);
            tail.drain(..excess);
            tail.extend(bytes);
        }
        if let Some(sink) = &sink {
            for part in bytes.split_inclusive(|b| *b == b'\n') {
                oversized |= line.len() + part.len() > MAX_EVENT_LINE;
                if !oversized {
                    line.extend_from_slice(part);
                }
                if part.ends_with(b"\n") {
                    if !oversized {
                        // A slow SSE reader cannot stall pipe drainage or process cleanup.
                        let _ = sink.try_send(std::mem::take(&mut line));
                    }
                    line.clear();
                    oversized = false;
                }
            }
        }
    }
    if let Some(sink) = sink
        && !oversized
        && !line.is_empty()
    {
        let _ = sink.try_send(line);
    }
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
    if run.cancel.is_cancelled() {
        run.set_outcome(Outcome::Cancelled);
        return req.with_receipt(cancelled(b"", b"", -1), &run, None);
    }
    if disconnect.is_cancelled() {
        return text(504, "context canceled");
    }
    let Ok(channel) = req
        .receipt
        .as_ref()
        .map(|_| tempfile::NamedTempFile::new())
        .transpose()
    else {
        run.set_outcome(Outcome::Failed);
        return text(502, "could not create CLI receipt channel");
    };
    let mut child = match launch(&executor, &req, &run, channel.as_ref()) {
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
    let Some(pgid) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    else {
        run.set_outcome(Outcome::Failed);
        return text(502, "missing CLI process id");
    };
    let writer = feed_order(child.stdin.take(), format!("{}\n", req.order.trim()), pgid);
    let stdout = Drain::start(child.stdout.take(), lines);
    let stderr = Drain::start(child.stderr.take(), None);
    let (stopped, may_signal) = tokio::select! {
        exited = wait_for_exit(pgid) => (None, exited),
        () = disconnect.cancelled() => (Some(Stop::Disconnect), true),
        () = run.cancel.cancelled() => (Some(Stop::Cancel), true),
        () = tokio::time::sleep_until(deadline) => (Some(Stop::Deadline), true),
    };
    if stopped.is_some() {
        signal_group(pgid, Signal::TERM);
    }
    let may_signal = if stopped.is_some() {
        let grace = if req.wants_receipt() {
            RECEIPT_STOP_GRACE
        } else {
            WAIT_DELAY
        };
        tokio::time::timeout(grace, wait_for_exit(pgid))
            .await
            .unwrap_or(true)
    } else {
        may_signal
    };
    // Join the writer before reaping so it cannot signal a reused process group.
    writer.abort();
    let stdin = writer.await.ok();
    if may_signal {
        signal_group(pgid, Signal::KILL);
    }
    let status = child.wait().await.ok();
    drop(stdin);
    let (out, err, wait_delay_expired) = collect_output(stdout, stderr).await;
    drop(permit);
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    let (outcome, reply) = reply(
        stopped,
        wait_delay_expired,
        code,
        &out,
        &err,
        req.wants_receipt(),
    );
    let reply = req.with_receipt(reply, &run, channel.as_ref());
    if let Some(o) = outcome {
        run.set_outcome(if reply.0 == 200 || o == Outcome::Cancelled {
            o
        } else {
            Outcome::Failed
        });
    }
    reply
}

// Keep stdin open while the child runs so gateway death is visible as EOF.
fn feed_order(
    mut stdin: Option<tokio::process::ChildStdin>,
    input: String,
    pgid: Pid,
) -> tokio::task::JoinHandle<Option<tokio::process::ChildStdin>> {
    tokio::spawn(async move {
        if let Some(pipe) = stdin.as_mut()
            && pipe.write_all(input.as_bytes()).await.is_err()
        {
            signal_group(pgid, Signal::TERM);
        }
        stdin
    })
}

fn launch(
    executor: &Executor,
    req: &Request,
    run: &Handle,
    channel: Option<&tempfile::NamedTempFile>,
) -> std::io::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new(&executor.interpreter);
    cs_child_signals::reset_on_exec(cmd.as_std_mut());
    cmd.arg(&executor.script)
        .args(["--gateway-owned", "--execution-only"])
        .env_clear()
        .envs(executor.env.iter().map(|(k, v)| (k, v)))
        .envs(req.work_order_env.iter().map(|(k, v)| (k, v)))
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let (Some(binding), Some(file)) = (&req.receipt, channel) {
        cmd.env("CS_RUN_ID", &binding.run_id)
            .env("CS_EXECUTION_ID", run.id())
            .env("CS_CLI_RECEIPT_PATH", file.path());
    }
    cmd.spawn()
}

// Descendants can hold inherited pipes open after child exit. Both drains share one deadline.
async fn collect_output(stdout: Drain, stderr: Drain) -> (Vec<u8>, Vec<u8>, bool) {
    let deadline = Instant::now() + WAIT_DELAY;
    let ((out, out_expired), (err, err_expired)) =
        tokio::join!(stdout.collect(deadline), stderr.collect(deadline));
    (out, err, out_expired || err_expired)
}

/// The reply and the outcome (`None`: the caller went away) once the child is reaped.
fn reply(
    stopped: Option<Stop>,
    wait_delay_expired: bool,
    code: i32,
    out: &[u8],
    err: &[u8],
    receipt: bool,
) -> (Option<Outcome>, HttpReply) {
    match stopped {
        Some(Stop::Cancel) => return (Some(Outcome::Cancelled), cancelled(out, err, code)),
        Some(Stop::Disconnect) => return (None, text(504, "context canceled")),
        Some(Stop::Deadline) if !receipt => {
            return (
                Some(Outcome::Failed),
                text(504, "context deadline exceeded"),
            );
        }
        Some(Stop::Deadline) | None => {}
    }
    let timed_out = matches!(stopped, Some(Stop::Deadline));
    if !timed_out && wait_delay_expired && code == 0 {
        return (
            Some(Outcome::Failed),
            text(502, "exec: WaitDelay expired before I/O complete"),
        );
    }
    let mut body = json!({
        "stdout": String::from_utf8_lossy(out),
        "stderr": String::from_utf8_lossy(err),
        "exit_code": code,
    });
    let outcome = if timed_out {
        body["outcome"] = json!("timed_out");
        Outcome::Failed
    } else {
        Outcome::Completed
    };
    (
        Some(outcome),
        (200, "application/json", format!("{body}\n")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_cli_model_keeps_order_and_uses_base_executor() {
        let body = br#"{"work_order":{"executor":"codex:gpt-6.2-sol"}}"#;
        let Ok(request) = parse(body) else {
            panic!("valid CLI selection")
        };
        assert_eq!(request.executor, "codex");
        assert_eq!(request.order, r#"{"executor":"codex:gpt-6.2-sol"}"#);
    }

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

#[cfg(test)]
mod drain_tests {
    use super::*;

    #[tokio::test]
    async fn drain_timeout_keeps_bytes_even_without_a_newline() -> std::io::Result<()> {
        let (mut writer, pipe) = tokio::io::duplex(32);
        writer.write_all(b"partial output").await?;
        let (output, _, expired) =
            collect_output(Drain::start(Some(pipe), None), Drain::default()).await;
        assert!(expired);
        assert_eq!(output, b"partial output");
        Ok(())
    }

    #[tokio::test]
    async fn drain_bounds_an_unterminated_line_and_keeps_its_tail() -> std::io::Result<()> {
        let mut data = vec![b'x'; 9 * 1024 * 1024];
        data.extend_from_slice(b"last bytes");
        let (output, expired) = Drain::start(Some(std::io::Cursor::new(data)), None)
            .collect(Instant::now() + WAIT_DELAY)
            .await;
        assert!(!expired);
        assert!(output.len() <= 8 * 1024 * 1024);
        assert!(output.ends_with(b"last bytes"));
        Ok(())
    }

    #[test]
    fn cancel_wait_covers_receipt_grace_and_pipe_drain() {
        assert!(crate::clirun::CANCEL_WAIT > RECEIPT_STOP_GRACE + WAIT_DELAY);
    }
}
