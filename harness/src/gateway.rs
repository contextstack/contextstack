//! Gateway-only model access, preserving the agent v2 wire and legacy error text.

use crate::process::Control;
use crate::spec::default_on_null;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json, value::RawValue};
use std::cell::Cell;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Preserve argument bytes: tools and transcript observations receive the original JSON.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolCall {
    #[serde(skip_serializing_if = "String::is_empty")]
    #[serde(deserialize_with = "default_on_null")]
    pub id: String,
    #[serde(deserialize_with = "default_on_null")]
    pub name: String,
    pub args: RawArguments,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub invalid_arguments: bool,
}

impl ToolCall {
    fn normalize_history(&mut self) {
        // Native tool history requires an object even when exec received a bare string.
        if self.name == "exec"
            && self.args.get().starts_with('"')
            && let Ok(args) = RawValue::from_string(format!(r#"{{"cmd":{}}}"#, self.args.get()))
        {
            self.args = RawArguments(Some(args));
        }
    }
}

/// Absent arguments reach tools as empty input; explicit null keeps its four bytes.
#[derive(Debug, Default, Serialize)]
#[serde(transparent)]
pub struct RawArguments(Option<Box<RawValue>>);

impl RawArguments {
    pub fn get(&self) -> &str {
        self.0.as_ref().map_or("", |raw| raw.get())
    }
}

impl<'de> Deserialize<'de> for RawArguments {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(deserializer).map(|raw| Self(Some(raw)))
    }
}

/// Serialize history directly so raw tool arguments never pass through Value.
#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        content: Value,
        #[serde(skip_serializing_if = "String::is_empty")]
        tool_call_id: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

impl Message {
    pub(crate) fn assistant(content: String, mut tool_calls: Vec<ToolCall>) -> Self {
        for call in &mut tool_calls {
            call.normalize_history();
        }
        Self::Assistant {
            content,
            tool_calls,
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    messages: &'a [Message],
    #[serde(flatten)]
    fields: Value,
}

#[derive(Default)]
pub(crate) struct ToolOffer<'a> {
    pub schemas: Option<&'a [Value]>,
    pub choice: Option<Value>,
}

/// The gateway owns accounting classification; the runner sums the supplied numbers.
#[derive(Debug)]
pub struct Response {
    pub text: String,
    pub model: String,
    pub cost_class: String,
    pub tool_calls: Vec<ToolCall>,
    pub est_usd: f64,
    pub charged_usd: f64,
    pub provider_reported_usd: Option<f64>,
    /// `usage` input and output tokens (WIRE Response); absent, non-integer or negative is `None`.
    pub usage: [Option<i64>; 2],
}

/// A failed call, including any charge the gateway settled before its terminal stream error.
#[derive(Debug, PartialEq)]
pub struct CallFailure {
    pub message: String,
    pub charged_usd: f64,
    pub provider_reported_usd: Option<f64>,
    pub cost_class: String,
}

impl From<String> for CallFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            charged_usd: 0.0,
            provider_reported_usd: None,
            cost_class: String::new(),
        }
    }
}

impl From<&str> for CallFailure {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}

impl fmt::Display for CallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CallFailure {}

/// What a streamed reply tells the caller as it arrives (WIRE Response, Stream).
pub enum Partial<'a> {
    /// One `kind: "delta"` chunk's text.
    Delta(&'a str),
    /// The stop chunk arrived and the reply is whole: the deltas so far are its text.
    Done,
    /// An error chunk, a broken stream or a cancel: the deltas so far are void, as a failed unary call.
    Discard,
}

/// The stop for a call the gateway refused as over its `budget.max_usd` (WIRE Request budget:
/// `gateway.policy_denied`, `error.budget.reason: ceiling`, $0). `call` prefixes the error with it.
pub const CEILING_STOP: &str = "budget:usd_ceiling";

/// Where `call` reports a streamed reply's partials; an error aborts the call with that error.
pub type Sink<'s> = dyn FnMut(Partial<'_>) -> Result<(), String> + 's;

pub(crate) struct CallTrace<'a> {
    pub run: &'a str,
    pub job: Option<&'a str>,
    pub turn: i64,
    pub id: String,
    sends: [Cell<Option<&'static str>>; 2],
}

impl<'a> CallTrace<'a> {
    pub fn new(run: &'a str, job: Option<&'a str>, turn: i64) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            run,
            job,
            turn,
            id: format!("{run}-{nanos}-{turn}"),
            sends: Default::default(),
        }
    }

    pub fn sends(&self) -> Vec<Value> {
        self.sends
            .iter()
            .enumerate()
            .filter_map(|(index, outcome)| {
                outcome
                    .get()
                    .map(|outcome| json!({"send_index": index + 1, "outcome": outcome}))
            })
            .collect()
    }
}

/// An endpoint and its fixed model identity, with a reusable HTTP client.
pub struct Gateway {
    pub endpoint: String,
    pub model: String,
    client: reqwest::Client,
    /// Whether calls stream: set once per run by `probe`, cleared by `gateway.streaming_unsupported`.
    stream: Cell<bool>,
}

/// The unary reply; a stream chunk decodes into it too (`kind`, `content` are chunk-only).
#[derive(Default, Deserialize)]
#[serde(default)]
struct WireResponse {
    #[serde(deserialize_with = "default_on_null")]
    status: String,
    #[serde(deserialize_with = "default_on_null")]
    kind: String,
    #[serde(deserialize_with = "default_on_null")]
    content: String,
    #[serde(deserialize_with = "default_on_null")]
    model: String,
    message: Option<WireMessage>,
    #[serde(deserialize_with = "tool_calls")]
    tool_calls: Vec<ToolCall>,
    cost: Option<WireCost>,
    usage: Value,
    error: Option<WireError>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct WireMessage {
    #[serde(deserialize_with = "default_on_null")]
    content: String,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct WireCost {
    #[serde(deserialize_with = "default_on_null")]
    class: String,
    est_usd: Option<f64>,
    charged_usd: Option<f64>,
    charged: Option<f64>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct WireError {
    #[serde(deserialize_with = "default_on_null")]
    class: String,
    #[serde(deserialize_with = "default_on_null")]
    code: String,
    #[serde(deserialize_with = "default_on_null")]
    message: String,
    budget: Option<WireDenial>,
}
/// The additive `error.budget` block on `gateway.policy_denied`; only its reason is read.
#[derive(Default, Deserialize)]
#[serde(default)]
struct WireDenial {
    #[serde(deserialize_with = "default_on_null")]
    reason: String,
}

fn tool_calls<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<ToolCall>, D::Error> {
    let calls = Option::<Vec<Option<ToolCall>>>::deserialize(deserializer)?;
    Ok(calls
        .unwrap_or_default()
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect())
}

impl Gateway {
    /// Reject an absent endpoint; provider credentials never belong in the runner.
    pub fn new(endpoint: String, model: String) -> Result<Self, String> {
        if endpoint.is_empty() {
            return Err("no gateway endpoint: set CSGATEWAY_ENDPOINT".into());
        }
        Ok(Self {
            endpoint,
            model,
            client: cs_http_client::builder()
                .build()
                .map_err(|e| e.to_string())?,
            stream: Cell::new(false),
        })
    }

    /// Ask once per run whether this model streams (`/v1/models` row `streaming`); any failure means unary.
    pub async fn probe(&self, control: &Control) {
        let Ok(mut url) = reqwest::Url::parse(&self.endpoint) else {
            return;
        };
        url.set_path("/v1/models");
        url.set_query(Some("available_only=false"));
        let ask = async {
            let reply = self.client.get(url).timeout(Duration::from_secs(5));
            reply.send().await.ok()?.json::<Value>().await.ok()
        };
        let models = tokio::select! {
            () = control.stopped() => None,
            models = ask => models,
        };
        let row = models.as_ref().and_then(|m| {
            let rows = m["models"].as_array()?;
            rows.iter().find(|row| row["name"] == self.model.as_str())
        });
        self.stream
            .set(row.is_some_and(|row| row["streaming"] == true));
    }

    /// Invoke once, bounded by the remaining run deadline or the legacy 120-second default.
    /// `max_usd` is the most this call may spend, sent as WIRE's `budget.max_usd`; `None` sends none.
    pub(crate) async fn call(
        &self,
        messages: &[Message],
        tools: ToolOffer<'_>,
        max_usd: Option<f64>,
        control: &Control,
        sink: &mut Sink<'_>,
        trace: &CallTrace<'_>,
    ) -> Result<Response, CallFailure> {
        let timeout = control
            .deadline
            .map_or(Duration::from_secs(120), |deadline| {
                deadline.saturating_duration_since(tokio::time::Instant::now())
            });
        let mut body = json!({
            "schema_version": "agent.gateway.model_call.v2",
            "request_id": trace.id,
            "run_id": trace.run,
            "step_id": trace.turn,
            "role": {"role_pack_name": "cs", "role_pack_version": "0", "role_name": "loop"},
            "capability": {"kind": "generic", "model_tier_hint": self.model},
            "tool_schema": tools.schemas,
            "budget": {
                "timeout_ms": timeout.as_millis().max(1),
                "max_output_tokens": 4096
            }
        });
        if let Some(choice) = tools.choice {
            body["tool_choice"] = choice;
        }
        if let Some(job) = trace.job {
            body["job_id"] = json!(job);
        }
        if let Some(usd) = max_usd {
            body["budget"]["max_usd"] = format!("{:.6}", usd.max(0.0)).into();
        }
        let opened = Cell::new(false);
        let exchange = async {
            let mut index = 1;
            if self.stream.get() {
                if let Some(reply) = self
                    .exchange(messages, &body, Some(&mut *sink), &opened, trace, index)
                    .await?
                {
                    return Ok(reply);
                }
                // The gateway refused streaming before contacting the provider.
                self.stream.set(false);
                index += 1;
            }
            let reply = self
                .exchange(messages, &body, None, &opened, trace, index)
                .await?;
            reply.ok_or_else(|| CallFailure::from("gateway refused a unary call as a stream"))
        };
        let result = tokio::select! {
            biased;
            result = exchange => result,
            () = control.stopped() => Err(self.cancel_error(control.reason() == Some("interrupted")).into()),
            () = tokio::time::sleep(timeout) => Err(self.cancel_error(false).into()),
        };
        if opened.get() {
            let reported = sink(if result.is_ok() {
                Partial::Done
            } else {
                Partial::Discard
            });
            if result.is_ok() {
                reported?;
            }
        }
        result
    }

    /// One HTTP exchange. With a sink it asks for a stream; `None` is `gateway.streaming_unsupported`.
    async fn exchange(
        &self,
        messages: &[Message],
        fields: &Value,
        sink: Option<&mut Sink<'_>>,
        opened: &Cell<bool>,
        trace: &CallTrace<'_>,
        index: usize,
    ) -> Result<Option<Response>, CallFailure> {
        let mut fields = fields.clone();
        if sink.is_some() {
            fields["stream"] = json!(true);
        }
        fields["send_index"] = json!(index);
        let outcome = &trace.sends[index - 1];
        outcome.set(Some("unknown"));
        let mut response = self
            .client
            .post(&self.endpoint)
            .json(&Request { messages, fields })
            .send()
            .await
            .map_err(|error| {
                if error.is_connect() {
                    outcome.set(Some("connection_failed"));
                }
                CallFailure::from(self.transport_error(&error))
            })?;
        outcome.set(Some("delivered"));
        let status = response.status().as_u16();
        let sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .is_some_and(|kind| kind.as_bytes().starts_with(b"text/event-stream"));
        let wire = if let Some(sink) = sink.filter(|_| sse) {
            self.fold(&mut response, status, sink, opened).await?
        } else {
            let raw = response
                .bytes()
                .await
                .map_err(|error| self.transport_error(&error))?;
            Some(decode(&raw, status)?)
        };
        wire.map(|reply| finish(reply, status)).transpose()
    }

    /// Fold SSE chunks into the unary reply: delta text, then the stop chunk's fields (WIRE Stream).
    async fn fold(
        &self,
        response: &mut reqwest::Response,
        status: u16,
        sink: &mut Sink<'_>,
        opened: &Cell<bool>,
    ) -> Result<Option<WireResponse>, String> {
        let (mut buffer, mut text) = (Vec::new(), String::new());
        loop {
            while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                let Some(data) = line.strip_prefix(b"data:") else {
                    continue;
                };
                let mut chunk = decode(data, status)?;
                let unsupported = chunk.kind == "error"
                    && chunk.error.as_ref().map(|e| e.code.as_str())
                        == Some("gateway.streaming_unsupported");
                if unsupported && !opened.get() {
                    return Ok(None);
                }
                opened.set(true);
                match chunk.kind.as_str() {
                    "delta" => {
                        sink(Partial::Delta(&chunk.content))?;
                        text.push_str(&chunk.content);
                    }
                    "stop" => {
                        chunk.status = "ok".into();
                        chunk.message = Some(WireMessage { content: text });
                        return Ok(Some(chunk));
                    }
                    _ => return Ok(Some(chunk)),
                }
            }
            let bytes = response
                .chunk()
                .await
                .map_err(|error| self.transport_error(&error))?
                .ok_or_else(|| bad_response(status, "stream ended without a stop chunk"))?;
            buffer.extend_from_slice(&bytes);
        }
    }

    fn cancel_error(&self, interrupted: bool) -> String {
        let reason = if interrupted {
            "interrupt signal received"
        } else {
            "context deadline exceeded"
        };
        format!("gateway unreachable: Post {:?}: {reason}", self.endpoint)
    }

    fn transport_error(&self, error: &reqwest::Error) -> String {
        if error.is_timeout() {
            return self.cancel_error(false);
        }
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(cause) = source {
            if cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
                && let Ok(url) = reqwest::Url::parse(&self.endpoint)
            {
                return format!(
                    "gateway unreachable: Post {:?}: dial tcp {}:{}: connect: connection refused",
                    self.endpoint,
                    url.host_str().unwrap_or(""),
                    url.port_or_known_default().unwrap_or(80)
                );
            }
            source = cause.source();
        }
        format!("gateway unreachable: Post {:?}: {error}", self.endpoint)
    }
}

fn bad_response(status: u16, error: &str) -> String {
    format!("bad gateway response (HTTP {status}): {error}")
}

fn decode(raw: &[u8], status: u16) -> Result<WireResponse, String> {
    serde_json::from_slice::<Option<WireResponse>>(raw)
        .map_err(|error| bad_response(status, &json_error(raw, &error)))
        .map(Option::unwrap_or_default)
}

/// Unary and folded replies alike: an error is the gateway's own, and an ok reply must carry cost.
fn finish(wire: WireResponse, status: u16) -> Result<Response, CallFailure> {
    let cost = wire.cost.unwrap_or_default();
    let provider_reported_usd = cost
        .charged
        .filter(|charge| charge.is_finite() && *charge > 0.0);
    if wire.status != "ok" {
        let message = wire.error.map_or_else(
            || format!("gateway status {:?} (HTTP {status})", wire.status),
            |error| {
                let text = format!(
                    "gateway {} [{}]: {}",
                    error.class, error.code, error.message
                );
                match error.budget {
                    Some(b) if b.reason == "ceiling" && error.code == "gateway.policy_denied" => {
                        format!("{CEILING_STOP}: {text}")
                    }
                    _ => text,
                }
            },
        );
        return Err(CallFailure {
            message,
            charged_usd: cost.charged_usd.unwrap_or(0.0),
            provider_reported_usd,
            cost_class: cost.class,
        });
    }
    let (Some(est_usd), Some(charged_usd)) = (cost.est_usd, cost.charged_usd) else {
        return Err(CallFailure::from(
            "gateway omitted cost numbers (WIRE Response cost)",
        ));
    };
    Ok(Response {
        text: wire.message.unwrap_or_default().content,
        model: wire.model,
        tool_calls: wire.tool_calls,
        cost_class: cost.class,
        est_usd,
        charged_usd,
        provider_reported_usd,
        usage: ["input_tokens", "output_tokens"]
            .map(|key| wire.usage[key].as_i64().filter(|n| *n >= 0)),
    })
}

// serde and Go name JSON syntax failures differently. Preserve the existing public
// diagnostic for malformed leading literals; other failures retain useful parser detail.
fn json_error(raw: &[u8], error: &serde_json::Error) -> String {
    let input = raw.trim_ascii_start();
    for literal in [b"null".as_slice(), b"true".as_slice(), b"false".as_slice()] {
        if input.first() == literal.first()
            && let Some((actual, expected)) = input
                .iter()
                .zip(literal)
                .find(|(actual, expected)| actual != expected)
        {
            return format!(
                "invalid character '{}' in literal {} (expecting '{}')",
                char::from(*actual),
                String::from_utf8_lossy(literal),
                char::from(*expected)
            );
        }
    }
    if let Some(first) = input.first()
        && !b"{[\"-0123456789ntf".contains(first)
    {
        return format!(
            "invalid character '{}' looking for beginning of value",
            char::from(*first)
        );
    }
    if error.is_eof() {
        "unexpected end of JSON input".into()
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_bytes_survive_response_decode() -> Result<(), serde_json::Error> {
        let raw = r#"{"id":"call-1","name":"tool","args":{ "z":2, "a":1 }}"#;
        let call: ToolCall = serde_json::from_str(raw)?;
        assert_eq!(call.args.get(), r#"{ "z":2, "a":1 }"#);
        assert_eq!(call.id, "call-1");
        Ok(())
    }

    #[test]
    fn missing_arguments_differ_from_explicit_null() -> Result<(), serde_json::Error> {
        let missing: ToolCall = serde_json::from_str(r#"{"name":"tool"}"#)?;
        let null: ToolCall = serde_json::from_str(r#"{"name":"tool","args":null}"#)?;
        assert_eq!(missing.args.get(), "");
        assert_eq!(null.args.get(), "null");
        assert_eq!(serde_json::to_value(missing)?, serde_json::to_value(null)?);
        Ok(())
    }

    #[test]
    fn response_null_fields_keep_go_zero_values() -> Result<(), serde_json::Error> {
        let wire: WireResponse = serde_json::from_str(
            r#"{"status":null,"model":null,"message":{"content":null},
            "tool_calls":[null,{"id":null,"name":null}],
            "cost":{"class":null,"est_usd":null,"charged_usd":0},
            "error":{"class":null,"code":null,"message":null}}"#,
        )?;
        assert_eq!(wire.status, "");
        assert_eq!(wire.model, "");
        assert_eq!(wire.message.unwrap_or_default().content, "");
        assert_eq!(wire.tool_calls.len(), 2);
        assert!(
            wire.tool_calls.iter().all(|call| call.id.is_empty()
                && call.name.is_empty()
                && call.args.get().is_empty())
        );
        let cost = wire.cost.unwrap_or_default();
        assert_eq!(cost.class, "");
        assert!(cost.est_usd.is_none());
        assert_eq!(cost.charged_usd, Some(0.0));
        let error = wire.error.unwrap_or_default();
        assert!(error.class.is_empty() && error.code.is_empty() && error.message.is_empty());
        Ok(())
    }

    async fn serve_once(listener: &tokio::net::TcpListener, body: &str) -> std::io::Result<()> {
        serve(listener, "application/json", body).await.map(drop)
    }

    /// Serve one HTTP reply after reading the whole request, so the client never sees a reset.
    async fn serve(
        listener: &tokio::net::TcpListener,
        content_type: &str,
        body: &str,
    ) -> std::io::Result<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await?;
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let read = socket.read(&mut chunk).await?;
            request.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&request).to_lowercase();
            let complete = text.split_once("\r\n\r\n").is_some_and(|(head, rest)| {
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(Some(0), |n| n.trim().parse::<usize>().ok());
                length.is_some_and(|n| rest.len() >= n)
            });
            if read == 0 || complete {
                break;
            }
        }
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).await?;
        Ok(String::from_utf8_lossy(&request).into_owned())
    }

    const SSE: &str = "text/event-stream";
    const OK: &str = r#"{"status":"ok","model":"m","message":{"content":"hi there"},"cost":{"class":"free_local","est_usd":0.5,"charged_usd":0},"usage":{"input_tokens":20,"output_tokens":30}}"#;

    fn frames(chunks: &[&str]) -> String {
        chunks.iter().flat_map(|c| ["data: ", c, "\n\n"]).collect()
    }

    fn delta(text: &str) -> String {
        format!(r#"{{"index":0,"kind":"delta","content":{text:?}}}"#)
    }

    /// A gateway already told the model streams, and the partials it reported, as text.
    async fn streamed(
        replies: &[(&str, String)],
    ) -> Result<(Result<Response, CallFailure>, Vec<String>, Vec<String>), String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let gateway = Gateway::new(format!("http://{address}/v1/agent/model.call"), "m".into())?;
        gateway.stream.set(true);
        let mut partials = Vec::new();
        let call_trace = CallTrace::new("r", Some("job-test"), 7);
        let mut sink = |p: Partial<'_>| {
            partials.push(match p {
                Partial::Delta(d) => format!("+{d}"),
                Partial::Done => "done".into(),
                Partial::Discard => "discard".into(),
            });
            Ok(())
        };
        let control = Control::default();
        let serving = async {
            let mut requests = Vec::new();
            for (kind, body) in replies {
                requests.push(
                    serve(&listener, kind, body)
                        .await
                        .map_err(|e| e.to_string())?,
                );
            }
            Ok::<_, String>(requests)
        };
        let (requests, response) = tokio::join!(
            serving,
            gateway.call(
                &[],
                ToolOffer::default(),
                None,
                &control,
                &mut sink,
                &call_trace
            )
        );
        Ok((response, partials, requests?))
    }

    #[tokio::test]
    async fn streamed_text_folds_into_the_unary_reply() -> Result<(), String> {
        let stop = r#"{"kind":"stop","stop_reason":"end_turn","provider":"p","model":"m","cost":{"class":"free_local","est_usd":0.5,"charged_usd":0},"usage":{"input_tokens":20,"output_tokens":30}}"#;
        let body = frames(&[&delta("hi "), &delta("there"), stop]);
        let (response, partials, requests) = streamed(&[(SSE, body)]).await?;
        let unary = finish(decode(OK.as_bytes(), 200)?, 200).map_err(|e| e.message)?;
        assert_eq!(
            format!("{:?}", response.map_err(|e| e.message)?),
            format!("{unary:?}")
        );
        assert_eq!(partials, ["+hi ", "+there", "done"]);
        assert!(requests[0].contains(r#""stream":true"#), "{}", requests[0]);
        Ok(())
    }

    #[tokio::test]
    async fn streamed_tool_calls_keep_argument_bytes() -> Result<(), String> {
        let stop = r#"{"kind":"stop","model":"m","tool_calls":[{"id":"c1","name":"t","args":{ "b":1 }}],"cost":{"est_usd":0,"charged_usd":0}}"#;
        let (response, partials, _) = streamed(&[(SSE, frames(&[stop]))]).await?;
        let response = response.map_err(|e| e.message)?;
        assert_eq!((response.text.as_str(), response.tool_calls.len()), ("", 1));
        assert_eq!(response.tool_calls[0].args.get(), r#"{ "b":1 }"#);
        assert_eq!(partials, ["done"]);
        Ok(())
    }

    #[tokio::test]
    async fn an_error_chunk_discards_the_partial_text_and_retains_cost() -> Result<(), String> {
        let error = r#"{"kind":"error","cost":{"amount":"0.004200","class":"api","est_usd":0.0042,"charged_usd":0.0042},"error":{"class":"runtime","code":"gateway.upstream_error","message":"boom"}}"#;
        let (response, partials, _) = streamed(&[(SSE, frames(&[&delta("half"), error]))]).await?;
        let Err(failure) = response else {
            return Err("accepted terminal stream error".into());
        };
        assert_eq!(
            failure.message,
            "gateway runtime [gateway.upstream_error]: boom"
        );
        assert_eq!(failure.charged_usd, 0.0042);
        assert_eq!(failure.cost_class, "api");
        assert_eq!(partials, ["+half", "discard"]);
        let (response, partials, _) = streamed(&[(SSE, frames(&[&delta("half")]))]).await?;
        let error = "bad gateway response (HTTP 200): stream ended without a stop chunk";
        let Err(failure) = response else {
            return Err("accepted incomplete stream".into());
        };
        assert_eq!(failure.message, error);
        assert_eq!(partials, ["+half", "discard"]);
        Ok(())
    }

    #[tokio::test]
    async fn streaming_unsupported_falls_back_to_one_unary_call() -> Result<(), String> {
        let refusal = r#"{"kind":"error","error":{"class":"fatal_session","code":"gateway.streaming_unsupported","message":"no"}}"#;
        let replies = [(SSE, frames(&[refusal])), ("application/json", OK.into())];
        let (response, partials, requests) = streamed(&replies).await?;
        assert_eq!(response.map_err(|e| e.message)?.text, "hi there");
        assert!(partials.is_empty(), "{partials:?}");
        assert!(requests[0].contains(r#""stream":true"#) && !requests[1].contains("stream"));
        let bodies = requests
            .iter()
            .map(|raw| {
                let (_, body) = raw.split_once("\r\n\r\n").ok_or("missing request body")?;
                serde_json::from_str::<Value>(body).map_err(|_| "invalid request body")
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(bodies[0]["request_id"], bodies[1]["request_id"]);
        assert!(
            bodies[0]["request_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        for (i, body) in bodies.iter().enumerate() {
            assert_eq!(body["job_id"], "job-test");
            assert_eq!(body["run_id"], "r");
            assert_eq!(body["step_id"], 7);
            assert_eq!(body["send_index"], i + 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn probe_reads_the_models_streaming_flag() -> Result<(), String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let gateway = Gateway::new(format!("http://{address}/v1/agent/model.call"), "m".into())?;
        let control = Control::default();
        for (body, expected) in [
            (
                r#"{"models":[{"name":"x","streaming":true},{"name":"m","streaming":true}]}"#,
                true,
            ),
            (r#"{"models":[{"name":"m","streaming":false}]}"#, false),
            (r#"{"models":[{"name":"x","streaming":true}]}"#, false),
            ("not json", false),
        ] {
            let (request, ()) = tokio::join!(
                serve(&listener, "application/json", body),
                gateway.probe(&control)
            );
            assert!(
                request
                    .map_err(|e| e.to_string())?
                    .starts_with("GET /v1/models?available_only=false ")
            );
            assert_eq!(gateway.stream.get(), expected, "{body}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn usage_tokens_are_read_and_bad_values_are_missing() -> Result<(), String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let gateway = Gateway::new(format!("http://{address}/"), "m".into())?;
        for (usage, expected) in [
            (
                r#","usage":{"input_tokens":12,"output_tokens":3}"#,
                [Some(12), Some(3)],
            ),
            (
                r#","usage":{"input_tokens":1.5,"output_tokens":-1}"#,
                [None, None],
            ),
            (r#","usage":{"input_tokens":null}"#, [None, None]),
            (r#","usage":"bogus""#, [None, None]),
            ("", [None, None]),
        ] {
            let body =
                format!(r#"{{"status":"ok","cost":{{"est_usd":0,"charged_usd":0}}{usage}}}"#);
            let control = Control::default();
            let call_trace = CallTrace::new("r", None, 1);
            let mut sink = |_: Partial<'_>| Ok(());
            let (served, response) = tokio::join!(
                serve_once(&listener, &body),
                gateway.call(
                    &[],
                    ToolOffer::default(),
                    None,
                    &control,
                    &mut sink,
                    &call_trace
                )
            );
            served.map_err(|e| e.to_string())?;
            assert_eq!(response.map_err(|e| e.message)?.usage, expected, "{usage}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn max_usd_is_sent_only_when_set() -> Result<(), String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let gateway = Gateway::new(format!("http://{address}/"), "m".into())?;
        for (max_usd, expected) in [
            (Some(0.0125), Some("0.012500")),
            (Some(-1.0), Some("0.000000")),
            (None, None),
        ] {
            let control = Control::default();
            let call_trace = CallTrace::new("r", None, 1);
            let mut sink = |_: Partial<'_>| Ok(());
            let (request, response) = tokio::join!(
                serve(&listener, "application/json", OK),
                gateway.call(
                    &[],
                    ToolOffer::default(),
                    max_usd,
                    &control,
                    &mut sink,
                    &call_trace
                )
            );
            response.map_err(|e| e.message)?;
            let request = request.map_err(|e| e.to_string())?;
            let body = request.split_once("\r\n\r\n").map_or("", |(_, b)| b);
            let body: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
            assert_eq!(body["budget"]["max_usd"].as_str(), expected, "{max_usd:?}");
            assert_eq!(body["budget"]["max_output_tokens"], 4096);
        }
        Ok(())
    }

    #[test]
    fn only_a_ceiling_denial_carries_the_ceiling_stop() -> Result<(), String> {
        let denied = |reason: &str| {
            let raw = format!(
                r#"{{"status":"error","error":{{"class":"fatal_session","code":"gateway.policy_denied","message":"no","budget":{{"reason":{reason:?}}}}}}}"#
            );
            finish(decode(raw.as_bytes(), 200)?, 200)
                .map(|r| r.text)
                .map_err(|e| e.message)
        };
        let text = "gateway fatal_session [gateway.policy_denied]: no";
        assert_eq!(denied("ceiling"), Err(format!("{CEILING_STOP}: {text}")));
        assert_eq!(denied("exhausted"), Err(text.into()));
        assert_eq!(denied("off"), Err(text.into()));
        let other = r#"{"status":"error","error":{"class":"fatal_session","code":"gateway.other","message":"no","budget":{"reason":"ceiling"}}}"#;
        let error = finish(decode(other.as_bytes(), 200)?, 200)
            .err()
            .map(|e| e.message);
        assert_eq!(
            error.as_deref(),
            Some("gateway fatal_session [gateway.other]: no")
        );
        Ok(())
    }

    #[test]
    fn empty_endpoint_is_rejected_at_construction() {
        if let Err(error) = Gateway::new(String::new(), "m".into()) {
            assert!(error.contains("no gateway endpoint"));
        } else {
            panic!("expected empty endpoint error");
        }
    }

    #[test]
    fn finish_rejects_responses_without_cost_numbers() -> Result<(), String> {
        let raw = r#"{"status":"ok","model":"m","message":{"content":"x"},"cost":{}}"#;
        let error = finish(decode(raw.as_bytes(), 200)?, 200)
            .err()
            .ok_or("accepted")?;
        assert!(error.message.contains("omitted cost numbers"));
        Ok(())
    }

    #[test]
    fn malformed_literal_errors_preserve_public_text() -> Result<(), String> {
        for (raw, expected) in [
            (
                "not json",
                "invalid character 'o' in literal null (expecting 'u')",
            ),
            (
                "<html>",
                "invalid character '<' looking for beginning of value",
            ),
            ("{", "unexpected end of JSON input"),
        ] {
            let error = serde_json::from_str::<Value>(raw)
                .err()
                .ok_or("malformed JSON accepted")?;
            assert_eq!(json_error(raw.as_bytes(), &error), expected);
        }
        Ok(())
    }
}
