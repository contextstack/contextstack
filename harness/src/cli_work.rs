use crate::{
    process::{self, Control},
    runner,
    trace::{Clock, Trace},
};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fs::File,
    io::{BufRead, BufReader, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::Duration,
};

const ROUTE: &str = "/v1/agent/cli.run";
const CHUNK: &str = "agent.gateway.cli_run.chunk.v1";
const ENV_KEYS: [&str; 4] = [
    "CS_SCHEDULE_DATE",
    "CS_SCHEDULE_SLOT",
    "CS_SCHEDULE_TIMEZONE",
    "CS_VIDEO_MUTATION",
];

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Binding {
    pub run_id: String,
    pub job_id: String,
    pub trial: i64,
    pub execution_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Receipt {
    version: u64,
    adapter: String,
    #[serde(flatten)]
    binding: Binding,
    pub native: Native,
    pub usage: serde_json::Map<String, Value>,
    #[serde(flatten)]
    metadata: serde_json::Map<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Native {
    pub stop: String,
    #[serde(deserialize_with = "required_nullable")]
    pub exit: Option<i64>,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Terminal {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    #[serde(deserialize_with = "required_nullable")]
    pub receipt: Option<Receipt>,
    #[serde(default)]
    pub outcome: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    ChildExit(Terminal),
    Cancelled(Option<Terminal>),
    TimedOut(Option<Terminal>),
    Refused(String),
    GatewayFailure(String),
    InvalidReply(String),
    TransportLost(String),
}

impl Outcome {
    fn terminal(&self) -> Option<&Terminal> {
        match self {
            Self::ChildExit(terminal) => Some(terminal),
            Self::Cancelled(terminal) | Self::TimedOut(terminal) => terminal.as_ref(),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::ChildExit(_) => "child_exit",
            Self::Cancelled(_) => "cancelled",
            Self::TimedOut(_) => "timed_out",
            Self::Refused(_) => "refused",
            Self::GatewayFailure(_) => "gateway_failure",
            Self::InvalidReply(_) => "invalid_reply",
            Self::TransportLost(_) => "transport_lost",
        }
    }
}

#[derive(Serialize)]
pub struct Execution {
    #[serde(flatten)]
    pub binding: Binding,
    #[serde(skip)]
    pub lock: File,
    #[serde(flatten)]
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_error: Option<String>,
}

struct Gateway {
    client: Client,
    url: Url,
    caller: String,
    token: String,
}

impl Gateway {
    fn new(endpoint: &str, caller: String, token: String) -> Result<Self, String> {
        let mut url = Url::parse(endpoint).map_err(|e| e.to_string())?;
        if url.host_str() == Some("localhost") {
            // Keep even a misconfigured hosts file from redirecting the bearer.
            url.set_host(Some("127.0.0.1")).map_err(|e| e.to_string())?;
        }
        let local = url
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
        if !local
            || url.scheme() != "http"
            || url.path() != ROUTE
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(format!(
                "CLI gateway endpoint must be loopback HTTP at {ROUTE}"
            ));
        }
        if caller.trim().is_empty() || reqwest::header::HeaderValue::from_str(&caller).is_err() {
            return Err("invalid CLI gateway caller".into());
        }
        let client = cs_http_client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            url,
            caller,
            token,
        })
    }

    fn post(&self, url: Url) -> RequestBuilder {
        let req = self
            .client
            .post(url)
            .header("X-ContextStack-Caller", &self.caller);
        if self.token.is_empty() {
            req
        } else {
            req.bearer_auth(&self.token)
        }
    }

    async fn cancel(&self, binding: &Binding, control: &Control, timed_out: &Cell<bool>) {
        control.stopped().await;
        timed_out.set(!control.cancel.is_cancelled());
        let mut url = self.url.clone();
        url.set_path(&format!("{ROUTE}/{}/cancel", binding.execution_id));
        let reason = control.cause.get().copied().or_else(|| control.reason());
        let relay = async {
            // The cancellation request can arrive before registration. Retry the cancel only.
            loop {
                let response = self
                    .post(url.clone())
                    .timeout(Duration::from_secs(21))
                    .json(&json!({"reason": reason}))
                    .send()
                    .await;
                if !matches!(response, Ok(r) if r.status().as_u16() == 404) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(22), relay).await;
        // The original request is still authoritative, including when cancel races child exit.
        tokio::time::sleep(Duration::from_secs(4)).await;
    }

    async fn execute(
        &self,
        order: &Value,
        binding: &Binding,
        control: &Control,
        delta: &mut impl FnMut(&str),
    ) -> Outcome {
        let call = async {
            let mut env =
                json!({"CS_JOB_ID": binding.job_id, "CS_TRIAL": binding.trial.to_string()});
            for key in ENV_KEYS {
                if let Some(value) = process::env(key) {
                    env[key] = json!(value);
                }
            }
            let mut payload = json!({
                "work_order": order, "environment": env, "stream": true,
                "execution_id": binding.execution_id,
                "receipt": {"run_id": binding.run_id, "trial": binding.trial},
            });
            if let Some(outcome) = self.exchange(&payload, binding, delta).await {
                return outcome;
            }
            if let Some(outcome) = stopped(control) {
                return outcome;
            }
            payload["stream"] = json!(false);
            self.exchange(&payload, binding, delta)
                .await
                .unwrap_or_else(|| {
                    Outcome::InvalidReply("gateway refused streaming for a unary request".into())
                })
        };
        if let Some(outcome) = stopped(control) {
            return outcome;
        }
        let timed_out = Cell::new(false);
        let outcome = tokio::select! {
            biased;
            outcome = call => outcome,
            () = self.cancel(binding, control, &timed_out) => Outcome::TransportLost("no terminal reply after forwarding cancellation".into()),
        };
        match outcome {
            Outcome::Cancelled(receipt) if timed_out.get() => Outcome::TimedOut(receipt),
            _ => outcome,
        }
    }

    async fn exchange(
        &self,
        body: &Value,
        binding: &Binding,
        delta: &mut impl FnMut(&str),
    ) -> Option<Outcome> {
        let response = match self
            .post(self.url.clone())
            .timeout(Duration::from_secs(11_000))
            .json(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return Some(Outcome::TransportLost(error.to_string())),
        };
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            let message = format!("gateway HTTP {status}: {detail}");
            return Some(gateway_error(status.as_u16(), message));
        }
        let event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("text/event-stream"));
        if event_stream && body["stream"] == true {
            return stream(response, binding, delta).await;
        }
        Some(match response.bytes().await {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => terminal(value, binding),
                Err(error) if error.is_eof() => Outcome::TransportLost(error.to_string()),
                Err(error) => Outcome::InvalidReply(error.to_string()),
            },
            Err(error) => Outcome::TransportLost(error.to_string()),
        })
    }
}

fn stopped(control: &Control) -> Option<Outcome> {
    control.reason().map(|_| {
        if control.cancel.is_cancelled() {
            Outcome::Cancelled(None)
        } else {
            Outcome::TimedOut(None)
        }
    })
}

fn gateway_error(status: u16, message: String) -> Outcome {
    match status {
        504 => Outcome::TimedOut(None),
        400..=499 | 503 => Outcome::Refused(message),
        _ => Outcome::GatewayFailure(message),
    }
}

fn terminal(value: Value, binding: &Binding) -> Outcome {
    let reply: Terminal = match serde_json::from_value(value) {
        Ok(reply) => reply,
        Err(error) => return Outcome::InvalidReply(error.to_string()),
    };
    let receipt = reply.receipt.as_ref();
    if receipt.is_some_and(|receipt| {
        receipt.version != 1
            || receipt.adapter != "executor"
            || receipt.binding != *binding
            || receipt.native.stop.trim().is_empty()
    }) {
        return Outcome::InvalidReply("invalid CLI receipt or mismatched run identity".into());
    }
    let native_stop = receipt.map(|receipt| receipt.native.stop.as_str());
    match (reply.outcome.as_deref(), native_stop) {
        (Some("cancelled"), _) | (None, Some("cancelled")) => Outcome::Cancelled(Some(reply)),
        (Some("timed_out"), _) | (None, Some("timeout")) => Outcome::TimedOut(Some(reply)),
        (None, _) if reply.exit_code != 0 || receipt.is_some() => Outcome::ChildExit(reply),
        _ => Outcome::InvalidReply("invalid CLI outcome or missing native stop".into()),
    }
}

#[derive(Deserialize)]
struct Chunk {
    schema_version: String,
    execution_id: String,
    index: u64,
    kind: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    error: Value,
}

async fn stream(
    mut response: Response,
    binding: &Binding,
    delta: &mut impl FnMut(&str),
) -> Option<Outcome> {
    let mut pending = Vec::new();
    let mut data = Vec::new();
    let mut index = 0;
    loop {
        match response.chunk().await {
            Ok(Some(bytes)) => pending.extend_from_slice(&bytes),
            Ok(None) => {
                return Some(Outcome::TransportLost(
                    "CLI stream ended before its terminal event".into(),
                ));
            }
            Err(error) => return Some(Outcome::TransportLost(error.to_string())),
        }
        let mut consumed = 0;
        for line in pending.split_inclusive(|b| *b == b'\n') {
            let Some(line_without_lf) = line.strip_suffix(b"\n") else {
                break;
            };
            consumed += line.len();
            let line = line_without_lf
                .strip_suffix(b"\r")
                .unwrap_or(line_without_lf);
            if let Some(value) = line.strip_prefix(b"data:") {
                data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
                data.push(b'\n');
            } else if line.is_empty() && !data.is_empty() {
                let chunk: Chunk = match serde_json::from_slice(&data) {
                    Ok(chunk) => chunk,
                    Err(error) => return Some(Outcome::InvalidReply(error.to_string())),
                };
                data.clear();
                if chunk.schema_version != CHUNK
                    || chunk.execution_id != binding.execution_id
                    || chunk.index != index
                {
                    return Some(Outcome::InvalidReply(
                        "mismatched CLI stream identity or sequence".into(),
                    ));
                }
                index += 1;
                match chunk.kind.as_str() {
                    "stop" => return Some(terminal(chunk.result, binding)),
                    "error"
                        if index == 1 && chunk.error["code"] == "gateway.streaming_unsupported" =>
                    {
                        return None;
                    }
                    "error" if chunk.error.is_object() => {
                        let message = chunk.error["message"]
                            .as_str()
                            .unwrap_or("gateway CLI error")
                            .to_owned();
                        return Some(gateway_error(chunk.status.unwrap_or(502), message));
                    }
                    "delta" if let Some(text) = chunk.content => delta(&text),
                    _ => return Some(Outcome::InvalidReply("invalid CLI stream event".into())),
                }
            }
        }
        pending.drain(..consumed);
    }
}

fn slug(value: &str) -> String {
    value
        .chars()
        .take(32)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// The local gateway accepts work orders over HTTP. Only a trusted harness
// reservation can enable the job's approval bridge inside its executor.
fn approval_binding(path: &Path, job: &str) -> std::io::Result<String> {
    let mut random = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let token = hex::encode(Sha256::digest(random));
    let file = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer(file, &json!({"job": job, "token": token}))?;
    Ok(token)
}

pub async fn run(
    order: Value,
    root: &Path,
    control: &Control,
    token: Option<String>,
    delta: impl FnMut(&str),
) -> Result<Execution, String> {
    run_with_binding_file(order, root, control, token, "action-mcp-binding", delta).await
}

async fn run_with_binding_file(
    mut order: Value,
    root: &Path,
    control: &Control,
    token: Option<String>,
    binding_file: &str,
    mut delta: impl FnMut(&str),
) -> Result<Execution, String> {
    let job = process::env("CS_JOB_ID")
        .filter(|s| {
            (1..=128).contains(&s.len())
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        })
        .ok_or("valid CS_JOB_ID required for CLI work orders")?;
    let trial = match order.get("trial") {
        None => process::env("CS_TRIAL").map_or(Ok(1), |s| s.parse()).ok(),
        Some(value) => value.as_i64(),
    }
    .filter(|n| *n > 0)
    .ok_or("invalid CLI trial")?;
    if process::env("CS_TRIAL").is_some_and(|s| s.parse::<i64>() != Ok(trial)) {
        return Err("CLI work order trial differs from CS_TRIAL".into());
    }
    let executor = order["executor"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("CLI executor required")?;
    let task = order["task"]
        .as_str()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .ok_or("CLI task required")?;
    let gateway = Gateway::new(
        &process::env("CSGATEWAY_CLI_ENDPOINT")
            .unwrap_or_else(|| format!("http://127.0.0.1:9200{ROUTE}")),
        process::env("CS_GATEWAY_CALLER").unwrap_or_else(|| format!("harness:{job}")),
        token
            .or_else(|| process::env("CSGATEWAY_AUTH_TOKEN"))
            .unwrap_or_default(),
    )?;
    let clock = Clock::start();
    let dir = runner::reserve_run(
        &runner::state_roots(root).0.join("runs"),
        &slug(executor),
        &slug(task),
        trial,
        clock.0,
    )?;
    let lock = File::create_new(dir.join("execution.lock")).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    order["trial"] = json!(trial);
    let mut trace = Trace::new(&dir);
    let binding = Binding {
        execution_id: format!("cli-{}", trace.run),
        run_id: trace.run.clone(),
        job_id: job,
        trial,
    };
    let approval_path = dir.join(binding_file);
    let approval_token = approval_binding(&approval_path, &binding.job_id)
        .map_err(|e| format!("create approval binding: {e}"))?;
    order["_cs_action_binding"] = json!(approval_token);
    let outcome = gateway.execute(&order, &binding, control, &mut delta).await;
    let _ = std::fs::remove_file(approval_path);
    let observed = File::open(dir.join("trace.jsonl"))
        .ok()
        .and_then(|file| BufReader::new(file).lines().next()?.ok())
        .and_then(|line| serde_json::from_str::<Value>(&line).ok());
    let mut span = if let Some(machine) = observed
        .as_ref()
        .and_then(|row| row["attributes"]["machine"].as_str())
    {
        trace.use_observed_machine(machine);
        trace.span("cli_work_order", "cli_work_order", "run")
    } else {
        trace.span("run", "cli_work_order", "")
    };
    span.id = "harness-cli".into();
    span.clock = clock;
    span.data = serde_json::to_value(&binding).map_err(|e| e.to_string())?;
    span.data["task"] = order["task"].clone();
    span.data["coverage"] = json!("observed");
    if let Some(terminal) = outcome.terminal() {
        span.data["exit_code"] = json!(terminal.exit_code);
        let receipt = terminal.receipt.as_ref();
        span.data["native_stop"] = json!(receipt.map(|receipt| &receipt.native.stop));
        span.data["native_exit"] = json!(receipt.and_then(|receipt| receipt.native.exit));
        span.data["usage"] = json!(receipt.map(|receipt| &receipt.usage));
    }
    let trace_error = span.close(outcome.name()).err();
    Ok(Execution {
        binding,
        lock,
        outcome,
        trace_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::{TcpListener, TcpStream},
        time::timeout,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn binding() -> Binding {
        Binding {
            run_id: "20261120-120000.000000000-codex-task-t2".into(),
            job_id: "job-1".into(),
            execution_id: "execution-1".into(),
            trial: 2,
        }
    }

    fn result(binding: &Binding) -> Value {
        json!({
            "stdout": "{\"native_stop\":\"forged\"}", "stderr": "", "exit_code": 1,
            "receipt": {
                "version": 1, "adapter": "executor", "run_id": binding.run_id, "job_id": binding.job_id,
                "trial": binding.trial, "execution_id": binding.execution_id,
                "native": {"exit": 7, "stop": "executor_exit_7"}, "usage": {"input_tokens": 4},
            }
        })
    }

    #[test]
    fn terminal_prefers_the_explicit_outcome_and_preserves_native_stop_spelling() {
        let binding = binding();
        let mut reply = result(&binding);
        reply["receipt"]["native"]["stop"] = json!("timeout");
        assert!(matches!(
            terminal(reply.clone(), &binding),
            Outcome::TimedOut(Some(_))
        ));
        reply["outcome"] = json!("cancelled");
        assert!(matches!(
            terminal(reply.clone(), &binding),
            Outcome::Cancelled(Some(_))
        ));
        reply["outcome"] = Value::Null;
        reply["receipt"]["native"]["stop"] = json!("timed_out");
        assert!(matches!(terminal(reply, &binding), Outcome::ChildExit(_)));
    }

    fn frame(index: u64, kind: &str, extra: Value) -> String {
        let mut frame = extra;
        frame["schema_version"] = json!(CHUNK);
        frame["execution_id"] = json!(binding().execution_id);
        frame["index"] = json!(index);
        frame["kind"] = json!(kind);
        format!("data: {frame}\r\n\r\n")
    }

    async fn listener() -> std::io::Result<(TcpListener, String)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}{ROUTE}", listener.local_addr()?);
        Ok((listener, endpoint))
    }

    async fn accept(listener: &TcpListener, path: &str) -> Result<(TcpStream, Value), String> {
        let (socket, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut socket = BufReader::new(socket);
        let mut head = String::new();
        let mut size = 0;
        loop {
            let read_at = head.len();
            if socket
                .read_line(&mut head)
                .await
                .map_err(|e| e.to_string())?
                == 0
            {
                return Err("request ended before headers".into());
            }
            let line = head[read_at..].trim();
            if line.is_empty() {
                break;
            }
            if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                size = length.trim().parse::<usize>().map_err(|e| e.to_string())?;
            }
        }
        assert!(head.starts_with(&format!("POST {path} HTTP/1.1\r\n")));
        let head = head.to_ascii_lowercase();
        assert!(head.contains("authorization: bearer test-token\r\n"));
        assert!(head.contains("x-contextstack-caller: test-caller\r\n"));
        let mut body = vec![0; size];
        socket
            .read_exact(&mut body)
            .await
            .map_err(|e| e.to_string())?;
        Ok((
            socket.into_inner(),
            serde_json::from_slice(&body).map_err(|e| e.to_string())?,
        ))
    }

    async fn reply(
        socket: &mut TcpStream,
        status: u16,
        kind: &str,
        body: &str,
    ) -> std::io::Result<()> {
        socket.write_all(format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: {kind}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ).as_bytes()).await
    }

    #[tokio::test]
    async fn cli_run_reserves_bound_run_and_traces_live_output() -> TestResult {
        run_with_trace(false).await?;
        run_with_trace(true).await
    }

    async fn run_with_trace(observed: bool) -> TestResult {
        let (listener, endpoint) = listener().await?;
        let scratch = tempfile::tempdir()?;
        let _env = process::ScopedEnv::set([
            (
                "CS_STATE_ROOT".into(),
                scratch.path().to_string_lossy().into_owned(),
            ),
            ("CS_JOB_ID".into(), "job-1".into()),
            ("CS_TRIAL".into(), "2".into()),
            ("CS_VIDEO_MUTATION".into(), "allowed".into()),
            ("CS_CONFIG_ROOT".into(), "/private-config".into()),
            ("CSGATEWAY_CLI_ENDPOINT".into(), endpoint),
            ("CSGATEWAY_AUTH_TOKEN".into(), "test-token".into()),
            ("CS_GATEWAY_CALLER".into(), "test-caller".into()),
        ]);
        // Seatbelt reserves the production basename for the trusted host harness.
        let binding_file = "fixture-binding.json";
        let live = tokio::sync::Notify::new();
        let serve = async {
            let (mut socket, request) = accept(&listener, ROUTE).await?;
            let (run_id, id) = assert_bound_request(scratch.path(), binding_file, &request)?;
            let frame = |index, kind, extra| frame(index, kind, extra).replace("execution-1", id);
            let first = frame(0, "delta", json!({"content": "live"}));
            let mut terminal = result(&binding());
            terminal["receipt"]["run_id"] = json!(run_id);
            terminal["receipt"]["execution_id"] = json!(id);
            let last = frame(1, "stop", json!({"result": terminal}));
            socket.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                first.len() + last.len()
            ).as_bytes()).await.map_err(|e| e.to_string())?;
            let (part, rest) = first.as_bytes().split_at(9);
            socket.write_all(part).await.map_err(|e| e.to_string())?;
            tokio::task::yield_now().await;
            socket.write_all(rest).await.map_err(|e| e.to_string())?;
            live.notified().await;
            if observed {
                let root =
                    json!({"span_id": "run", "kind": "run", "attributes": {"machine": "estate"}});
                std::fs::write(
                    scratch.path().join("runs").join(run_id).join("trace.jsonl"),
                    format!("{root}\n"),
                )
                .map_err(|e| e.to_string())?;
            }
            socket
                .write_all(last.as_bytes())
                .await
                .map_err(|e| e.to_string())
        };
        let control = Control::default();
        let run = run_with_binding_file(
            json!({"executor": "codex", "task": "tasks/task"}),
            scratch.path(),
            &control,
            None,
            binding_file,
            |text| {
                assert_eq!(text, "live");
                live.notify_one();
            },
        );
        let execution = timeout(Duration::from_secs(5), async {
            tokio::try_join!(serve, async {
                let execution = run.await?;
                if !matches!(execution.outcome, Outcome::ChildExit(_)) {
                    return Err(format!("unexpected outcome {:?}", execution.outcome));
                }
                Ok(execution)
            })
            .map(|((), execution)| execution)
        })
        .await
        .map_err(|_| "CLI gateway fixture did not finish streaming within five seconds")??;
        let Outcome::ChildExit(terminal) = execution.outcome else {
            unreachable!();
        };
        assert_eq!(terminal.exit_code, 1);
        assert_eq!(
            terminal
                .receipt
                .as_ref()
                .and_then(|receipt| receipt.native.exit),
            Some(7)
        );
        assert_eq!(execution.trace_error, None);
        assert!(
            !scratch
                .path()
                .join("runs")
                .join(&execution.binding.run_id)
                .join(binding_file)
                .exists()
        );
        assert_trace(scratch.path(), &execution.binding, observed)
    }

    fn assert_bound_request<'a>(
        scratch: &Path,
        binding_file: &str,
        request: &'a Value,
    ) -> Result<(&'a str, &'a str), String> {
        assert_eq!(request["environment"]["CS_JOB_ID"], "job-1");
        assert_eq!(request["environment"]["CS_TRIAL"], "2");
        assert_eq!(request["environment"]["CS_VIDEO_MUTATION"], "allowed");
        assert_eq!(request["environment"].get("CS_CONFIG_ROOT"), None);
        assert_eq!(request["environment"].get("CSGATEWAY_AUTH_TOKEN"), None);
        assert_eq!(request["work_order"]["trial"], 2);
        let run_id = request["receipt"]["run_id"]
            .as_str()
            .ok_or("missing run ID")?;
        let id = request["execution_id"]
            .as_str()
            .ok_or("missing execution ID")?;
        let dir = scratch.join("runs").join(run_id);
        let binding_record: Value = serde_json::from_slice(
            &std::fs::read(dir.join(binding_file)).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        assert_eq!(binding_record["job"], "job-1");
        assert_eq!(
            binding_record["token"],
            request["work_order"]["_cs_action_binding"]
        );
        assert_eq!(id, format!("cli-{run_id}"));
        Ok((run_id, id))
    }

    fn assert_trace(scratch: &Path, binding: &Binding, observed: bool) -> TestResult {
        let records = std::fs::read_to_string(
            scratch
                .join("runs")
                .join(&binding.run_id)
                .join("trace.jsonl"),
        )?;
        let trace: Value =
            serde_json::from_str(records.lines().last().ok_or("missing CLI trace")?)?;
        assert_eq!(records.lines().count(), if observed { 2 } else { 1 });
        if observed {
            assert_eq!(trace["kind"], "cli_work_order");
            assert_eq!(trace["parent_id"], "run");
            assert_eq!(trace["attributes"]["machine"], "estate");
        } else {
            assert_eq!(trace["kind"], "run");
            assert_eq!(trace["parent_id"], Value::Null);
        }
        assert_eq!(trace["trace_id"], binding.run_id);
        assert_eq!(trace["attributes"]["job_id"], "job-1");
        assert_eq!(trace["attributes"]["execution_id"], binding.execution_id);
        assert_eq!(trace["outcome"], "child_exit");
        Ok(())
    }

    #[tokio::test]
    async fn only_explicit_unsupported_stream_falls_back() -> TestResult {
        let (listener, endpoint) = listener().await?;
        let gateway = Gateway::new(&endpoint, "test-caller".into(), "test-token".into())?;
        let binding = binding();
        let serve = async {
            let (mut socket, mut streaming) = accept(&listener, ROUTE).await?;
            assert_eq!(streaming["stream"], true);
            reply(
                &mut socket,
                200,
                "text/event-stream",
                &frame(
                    0,
                    "error",
                    json!({"error": {"code": "gateway.streaming_unsupported"}}),
                ),
            )
            .await
            .map_err(|e| e.to_string())?;
            let (mut socket, unary) = accept(&listener, ROUTE).await?;
            streaming["stream"] = json!(false);
            assert_eq!(streaming, unary);
            reply(
                &mut socket,
                200,
                "application/json",
                &result(&binding).to_string(),
            )
            .await
            .map_err(|e| e.to_string())
        };
        let control = Control::default();
        let order = json!({"executor":"fake-cli"});
        let mut no_delta = |_: &str| panic!("unexpected delta");
        let call = gateway.execute(&order, &binding, &control, &mut no_delta);
        let (served, outcome) =
            timeout(Duration::from_secs(5), async { tokio::join!(serve, call) }).await?;
        served?;
        assert!(matches!(outcome, Outcome::ChildExit(_)));
        Ok(())
    }

    #[tokio::test]
    async fn failures_do_not_resubmit() -> TestResult {
        let binding = binding();
        let mut malformed = result(&binding);
        malformed["receipt"]["trial"] = json!(3);
        let mut timed_out = result(&binding);
        timed_out["outcome"] = json!("timed_out");
        let cases = [
            (502, "text/plain", "failed".to_owned(), "gateway_failure"),
            (
                200,
                "text/event-stream",
                frame(0, "stop", json!({"result": timed_out})),
                "timed_out",
            ),
            (403, "application/json", "{}".to_owned(), "refused"),
            (
                504,
                "text/plain",
                "context deadline exceeded".to_owned(),
                "timed_out",
            ),
            (
                200,
                "text/event-stream",
                frame(
                    0,
                    "error",
                    json!({"status": 503, "error": {"message":"capacity"}}),
                ),
                "refused",
            ),
            (
                200,
                "text/event-stream",
                frame(0, "delta", json!({"content":"partial"})),
                "transport_lost",
            ),
            (200, "application/json", "{".to_owned(), "transport_lost"),
            (
                200,
                "text/event-stream",
                frame(0, "stop", json!({"result": malformed})),
                "invalid_reply",
            ),
        ];
        for (status, kind, body, expected) in cases {
            let (listener, endpoint) = listener().await?;
            let gateway = Gateway::new(&endpoint, "test-caller".into(), "test-token".into())?;
            let serve = async {
                let (mut socket, _) = accept(&listener, ROUTE).await?;
                reply(&mut socket, status, kind, &body)
                    .await
                    .map_err(|e| e.to_string())
            };
            let order = json!({"executor":"codex"});
            let control = Control::default();
            let mut delta = |_: &str| {};
            let call = gateway.execute(&order, &binding, &control, &mut delta);
            let (served, outcome) =
                timeout(Duration::from_secs(5), async { tokio::join!(serve, call) }).await?;
            served?;
            assert_eq!(outcome.name(), expected);
            assert!(
                timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_retries_registration_race_and_drains_terminal() -> TestResult {
        let (listener, endpoint) = listener().await?;
        let gateway = Gateway::new(&endpoint, "test-caller".into(), "test-token".into())?;
        let binding = binding();
        let control = Control::default();
        let serve = async {
            let (mut run, _) = accept(&listener, ROUTE).await?;
            control.cancel_with("SIGTERM");
            let path = format!("{ROUTE}/{}/cancel", binding.execution_id);
            for status in [404, 202] {
                let (mut cancel, request) = accept(&listener, &path).await?;
                assert_eq!(request["reason"], "SIGTERM");
                reply(
                    &mut cancel,
                    status,
                    "application/json",
                    r#"{"result":"cancelling"}"#,
                )
                .await
                .map_err(|e| e.to_string())?;
            }
            let mut terminal = result(&binding);
            terminal["outcome"] = json!("cancelled");
            reply(
                &mut run,
                200,
                "text/event-stream",
                &frame(0, "stop", json!({"result": terminal})),
            )
            .await
            .map_err(|e| e.to_string())
        };
        let order = json!({"executor":"codex"});
        let mut delta = |_: &str| {};
        let (served, outcome) = timeout(Duration::from_secs(5), async {
            tokio::join!(
                serve,
                gateway.execute(&order, &binding, &control, &mut delta)
            )
        })
        .await?;
        served?;
        assert!(matches!(outcome, Outcome::Cancelled(Some(_))));
        assert!(matches!(
            gateway
                .execute(&order, &binding, &control, &mut delta)
                .await,
            Outcome::Cancelled(None)
        ));
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn refuses_remote_and_credential_bearing_endpoints() {
        for endpoint in [
            "http://192.0.2.1/v1/agent/cli.run",
            "http://name@127.0.0.1/v1/agent/cli.run",
            "http://127.0.0.1/v1/agent/model.call",
        ] {
            assert!(Gateway::new(endpoint, "test-caller".into(), "test-token".into()).is_err());
        }
    }
}
