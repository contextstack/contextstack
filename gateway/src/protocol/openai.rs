//! OpenAI-compatible chat/completions (Ollama, vLLM, `OpenAI` and the other keyed clouds), unary and
//! SSE. Image blocks are refused fail-closed (WIRE clause 3); Hermes/Qwen XML tool calls become
//! structured calls (clause 2). A stream ends in the same reply the unary path builds.

use super::history::validate_tool_history;
use super::sse::Lines;
use super::{
    Call, Event, Http, Reply, Stop, error_body, request_id_header, trace, transport, until,
};
use crate::errors::CallError;
use crate::wire::{Message, ReplyToolCall, nullable};
use serde::Deserialize;
use serde_json::{Map, Value, json, value::RawValue};
use tokio::time::Instant;

/// One OpenAI-compatible endpoint.
pub struct OpenAi<'a> {
    /// Endpoint id (error text names it).
    pub name: &'a str,
    /// Base URL including `/v1`.
    pub base: &'a str,
    /// Model when the call names none.
    pub default_model: &'a str,
}

#[derive(Deserialize, Default)]
struct Out {
    #[serde(default, deserialize_with = "nullable")]
    choices: Vec<Choice>,
    #[serde(default, deserialize_with = "nullable")]
    usage: OutUsage,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct PromptDetails {
    #[serde(default, deserialize_with = "nullable")]
    cached_tokens: i64,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct OutUsage {
    #[serde(default, deserialize_with = "nullable")]
    prompt_tokens: i64,
    #[serde(default, deserialize_with = "nullable")]
    completion_tokens: i64,
    #[serde(default, deserialize_with = "nullable")]
    prompt_tokens_details: PromptDetails,
    #[serde(default)]
    cost: Option<f64>,
}

impl OutUsage {
    fn token_bill(&self) -> (i64, i64, i64) {
        let cached = self.prompt_tokens_details.cached_tokens.max(0);
        let fresh = self.prompt_tokens.saturating_sub(cached);
        (fresh, cached, self.completion_tokens)
    }
}

#[derive(Deserialize, Default)]
struct Choice {
    #[serde(default, deserialize_with = "nullable")]
    finish_reason: String,
    #[serde(default, deserialize_with = "nullable")]
    message: OutMessage,
}

#[derive(Deserialize, Default)]
struct OutMessage {
    #[serde(default, deserialize_with = "nullable")]
    content: String,
    #[serde(default, deserialize_with = "nullable")]
    tool_calls: Vec<OutToolCall>,
}

#[derive(Deserialize, Default)]
struct OutToolCall {
    /// Stream only: the delta index fragments are keyed by.
    #[serde(skip)]
    index: i64,
    /// Stream only: the argument text so far (`None` until a fragment arrives).
    #[serde(skip)]
    fragments: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    function: OutFunction,
}

#[derive(Deserialize, Default)]
struct OutFunction {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
    /// Present-but-null stays `null` (only an absent field means `{}`).
    #[serde(default, deserialize_with = "present")]
    arguments: Option<Box<RawValue>>,
}

impl OpenAi<'_> {
    /// The upstream request body.
    pub fn body(&self, call: &Call) -> Result<Value, CallError> {
        let model = call.model_or(self.default_model);
        let max_tokens = call.max_tokens_or_default();
        let mut body = json!({"model": model, "max_tokens": max_tokens, "messages": encode(call)?});
        if !call.tools.is_empty() {
            let tools: Vec<Value> = call
                .tools
                .iter()
                .map(|t| {
                    let mut f = Map::new();
                    f.insert("name".into(), json!(t.name));
                    if !t.description.is_empty() {
                        f.insert("description".into(), json!(t.description));
                    }
                    let params = t.schema_value();
                    f.insert("parameters".into(), params);
                    json!({"type": "function", "function": f})
                })
                .collect();
            body["tools"] = Value::Array(tools);
        }
        Ok(body)
    }

    /// The request as sent, and its URL.
    fn request(&self, call: &Call, stream: bool) -> Result<(String, Vec<u8>), CallError> {
        let base = self.base.trim_end_matches('/');
        if base.is_empty() {
            return Err(CallError::transport_local(format!(
                "{}: empty base url",
                self.name
            )));
        }
        let mut body = self.body(call)?;
        if stream {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
        }
        let raw =
            serde_json::to_vec(&body).map_err(|e| CallError::transport_local(e.to_string()))?;
        Ok((format!("{base}/chat/completions"), raw))
    }

    /// POST and settle the status: a non-2xx is the classified error with the upstream body.
    async fn send(
        &self,
        http: &Http,
        key: Option<&str>,
        call: &Call,
        url: &str,
        raw: Vec<u8>,
        fields: &mut Value,
    ) -> Result<reqwest::Response, CallError> {
        let mut req = http
            .client()
            .post(url)
            .header("Content-Type", "application/json")
            .body(raw);
        if let Some(k) = key.filter(|k| !k.is_empty()) {
            req = req.header("Authorization", format!("Bearer {k}"));
        }
        if let Some(id) = request_id_header(&call.request_id) {
            req = req.header("X-Request-Id", id);
        }
        let resp = req.send().await.map_err(|e| transport("Post", url, &e))?;
        let status = resp.status().as_u16();
        fields["http_status"] = json!(status);
        if status >= 300 {
            let retry_after_ms = crate::errors::parse_retry_after(resp.headers());
            let text = error_body(resp, 4096).await;
            fields["upstream_error_bytes"] = json!(text.len());
            fields["upstream_error_sha256"] = json!(crate::log::sha256_hex(text.as_bytes()));
            return Err(CallError::upstream_http(
                status,
                self.name,
                retry_after_ms,
                &text,
            ));
        }
        Ok(resp)
    }

    /// One unary call.
    pub async fn invoke(
        &self,
        http: &Http,
        key: Option<&str>,
        call: &Call,
        deadline: Instant,
    ) -> Result<Reply, CallError> {
        let (url, raw) = self.request(call, false)?;
        let started = chrono::Utc::now();
        let start = std::time::Instant::now();
        let mut fields = json!({});
        let result = until(deadline, "Post", &url, async {
            let resp = self
                .send(http, key, call, &url, raw.clone(), &mut fields)
                .await?;
            let latency = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
            let bytes = super::success_body(resp, "Post", &url).await?;
            let out: Out = first_value(&bytes).map_err(CallError::transport_local)?;
            reply(self.name, out, latency, &mut fields)
        })
        .await;
        fields["failed"] = json!(result.is_err());
        trace(&call.request_id, &raw, started, start.elapsed(), &fields);
        result
    }

    /// Open a stream (`stream: true`, usage requested): the request and its status are settled here;
    /// events follow from [`Stream::next`].
    pub async fn stream(
        &self,
        http: &Http,
        key: Option<&str>,
        call: &Call,
        deadline: Instant,
    ) -> Result<Stream, CallError> {
        let (url, raw) = self.request(call, true)?;
        let started = chrono::Utc::now();
        let start = std::time::Instant::now();
        let mut fields = json!({"stream": true});
        let result = until(
            deadline,
            "Post",
            &url,
            self.send(http, key, call, &url, raw.clone(), &mut fields),
        )
        .await;
        fields["failed"] = json!(result.is_err());
        trace(&call.request_id, &raw, started, start.elapsed(), &fields);
        let resp = result?;
        let sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
        let mut stream = Stream {
            lines: None,
            acc: Accum {
                name: self.name.to_string(),
                ..Accum::default()
            },
            queued: Vec::new(),
            done: false,
        };
        if sse {
            stream.lines = Some(Lines::new(resp, deadline, "openai_compat stream"));
            return Ok(stream);
        }
        // Many OpenAI-compatible servers ignore `stream: true` and answer with the unary body. Fold it
        // through the unary rules into one delta and the stop, so the caller still gets a valid stream.
        let bytes = until(deadline, "Post", &url, super::success_body(resp, "Post", &url)).await?;
        let out: Out = first_value(&bytes).map_err(CallError::transport_local)?;
        let r = reply(self.name, out, 0, &mut json!({}))?;
        stream.queued.push(Event::Stop(Stop {
            reason: r.stop_reason,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            charged: r.charged,
            provider: r.provider,
            tool_calls: r.tool_calls,
        }));
        if !r.text.is_empty() {
            stream.queued.push(Event::Delta(r.text));
        }
        Ok(stream)
    }
}

/// The unary reply from a decoded body; a stream accumulates the same body and ends here too, so both
/// paths share one set of rules (stop reasons, native and Hermes tool calls, argument parsing, charges).
fn reply(name: &str, out: Out, latency_ms: i64, fields: &mut Value) -> Result<Reply, CallError> {
    if out.usage.cost.is_some_and(|c| c < 0.0) {
        return Err(CallError::transport_local(format!(
            "{name}: negative provider charge"
        )));
    }
    fields["response_decoded"] = json!(true);
    fields["input_tokens"] = json!(out.usage.prompt_tokens);
    fields["output_tokens"] = json!(out.usage.completion_tokens);
    let mut text = String::new();
    let mut stop = "end_turn".to_string();
    let mut tool_calls = Vec::new();
    if let Some(c) = out.choices.into_iter().next() {
        fields["finish_reason"] = json!(c.finish_reason);
        fields["content_bytes"] = json!(c.message.content.len());
        fields["native_tool_calls"] = json!(c.message.tool_calls.len());
        text = c.message.content;
        if c.finish_reason == "tool_calls" {
            stop = "tool_use".into();
        } else if !c.finish_reason.is_empty() {
            stop = c.finish_reason;
        }
        for tc in c.message.tool_calls {
            let args = native_args(tc.function.arguments).map_err(|e| {
                CallError::ProviderFailed(format!("{name}: tool call {:?}: {e}", tc.function.name))
            })?;
            tool_calls.push(ReplyToolCall {
                id: tc.id,
                name: tc.function.name,
                args,
            });
        }
    }
    if tool_calls.is_empty() {
        let xml = hermes_tool_calls(&text);
        if !xml.is_empty() {
            tool_calls = xml;
            stop = "tool_use".into();
        }
    }
    let (input, cache_read, output) = out.usage.token_bill();
    Ok(Reply {
        text,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
        charged: out.usage.cost,
        provider: name.to_string(),
        latency_ms,
        stop_reason: stop,
        tool_calls,
    })
}

/// An open `chat.completions` SSE stream. Content deltas pass through as they arrive; everything else
/// (finish reason, tool-call fragments by index, usage) accumulates into the unary body's shape, which
/// [`reply`] turns into the stop at `data: [DONE]`. EOF before `[DONE]` is transport death (WIRE clause 4).
pub struct Stream {
    /// The SSE body; `None` when the server answered with a unary body (see `queued`).
    lines: Option<Lines>,
    acc: Accum,
    /// Events already decoded from a unary answer, in reverse order (popped from the end).
    queued: Vec<Event>,
    done: bool,
}

/// What a stream has said so far, in the unary body's shape.
#[derive(Default)]
struct Accum {
    name: String,
    /// The first choice as a unary body would carry it (tool-call arguments as concatenated text).
    choice: Choice,
    usage: OutUsage,
}

#[derive(Deserialize, Default)]
struct StreamChunk {
    #[serde(default, deserialize_with = "nullable")]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<OutUsage>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize, Default)]
struct StreamChoice {
    #[serde(default, deserialize_with = "nullable")]
    index: i64,
    #[serde(default, deserialize_with = "nullable")]
    finish_reason: String,
    #[serde(default, deserialize_with = "nullable")]
    delta: StreamDelta,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default, deserialize_with = "nullable")]
    content: String,
    #[serde(default, deserialize_with = "nullable")]
    tool_calls: Vec<DeltaCall>,
}

#[derive(Deserialize, Default)]
struct DeltaCall {
    #[serde(default, deserialize_with = "nullable")]
    index: i64,
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    function: DeltaFunction,
}

#[derive(Deserialize, Default)]
struct DeltaFunction {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
    #[serde(default)]
    arguments: Option<Box<RawValue>>,
}

impl Stream {
    /// Tokens and charge accumulated from usage chunks so far.
    pub fn metered_usage(&self) -> super::MeteredUsage {
        let u = &self.acc.usage;
        super::MeteredUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            charged: u.cost,
        }
    }

    /// The next event; `Ok(None)` only after the terminal event has been returned.
    pub async fn next(&mut self) -> Result<Option<Event>, CallError> {
        if self.done {
            return Ok(None);
        }
        let out = self.pull().await;
        if !matches!(out, Ok(Some(Event::Delta(_)))) {
            self.done = true;
        }
        out
    }

    async fn pull(&mut self) -> Result<Option<Event>, CallError> {
        if let Some(ev) = self.queued.pop() {
            return Ok(Some(ev));
        }
        let Some(lines) = self.lines.as_mut() else {
            return Ok(None);
        };
        while let Some(payload) = lines.data().await? {
            if let Some(ev) = self.acc.feed(&payload)? {
                return Ok(Some(ev));
            }
        }
        Err(CallError::transport_local(format!(
            "{} stream ended without [DONE]: unexpected EOF",
            self.acc.name
        )))
    }
}

impl Accum {
    /// One `data:` payload: a delta, the stop at `[DONE]`, the upstream's error, or nothing yet.
    fn feed(&mut self, payload: &[u8]) -> Result<Option<Event>, CallError> {
        if payload.trim_ascii() == b"[DONE]" {
            return self.finish().map(Some);
        }
        let chunk: StreamChunk = serde_json::from_slice(payload).map_err(|e| {
            CallError::transport_local(format!("{}: undecodable stream chunk: {e}", self.name))
        })?;
        if let Some(err) = chunk.error {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| err.to_string(), str::to_string);
            return Ok(Some(Event::UpstreamError(msg)));
        }
        if let Some(u) = chunk.usage {
            self.usage = u;
        }
        Ok(self.absorb(chunk.choices).map(Event::Delta))
    }

    /// Fold the first choice's delta into the accumulated body; its non-empty text is returned.
    fn absorb(&mut self, choices: Vec<StreamChoice>) -> Option<String> {
        let c = choices.into_iter().find(|c| c.index == 0)?;
        if !c.finish_reason.is_empty() {
            self.choice.finish_reason = c.finish_reason;
        }
        for d in c.delta.tool_calls {
            // A fragment continues the latest call at its index; a different id there starts a new call
            // (servers that send whole calls without distinct indexes).
            let calls = &mut self.choice.message.tool_calls;
            let same = calls.iter().rposition(|t| {
                t.index == d.index && (d.id.is_empty() || t.id.is_empty() || t.id == d.id)
            });
            let at = same.unwrap_or_else(|| {
                calls.push(OutToolCall {
                    index: d.index,
                    ..OutToolCall::default()
                });
                calls.len() - 1
            });
            let t = &mut calls[at];
            if !d.id.is_empty() {
                t.id = d.id;
            }
            if t.function.name.is_empty() {
                t.function.name = d.function.name;
            }
            if let Some(a) = d.function.arguments.filter(|a| a.get() != "null") {
                // Fragments are JSON strings; a server that sends a bare value sends it whole.
                let piece =
                    serde_json::from_str::<String>(a.get()).unwrap_or_else(|_| a.get().to_string());
                t.fragments.get_or_insert_with(String::new).push_str(&piece);
            }
        }
        let text = c.delta.content;
        if text.is_empty() {
            return None;
        }
        self.choice.message.content.push_str(&text);
        Some(text)
    }

    fn finish(&mut self) -> Result<Event, CallError> {
        let mut choice = std::mem::take(&mut self.choice);
        for t in &mut choice.message.tool_calls {
            t.function.arguments = t
                .fragments
                .take()
                .and_then(|f| serde_json::to_string(&f).ok())
                .and_then(|s| RawValue::from_string(s).ok());
        }
        let out = Out {
            choices: vec![choice],
            usage: std::mem::take(&mut self.usage),
        };
        let r = reply(&self.name, out, 0, &mut json!({}))?;
        Ok(Event::Stop(Stop {
            reason: r.stop_reason,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            charged: r.charged,
            provider: r.provider,
            tool_calls: r.tool_calls,
        }))
    }
}

fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(d).map(Some)
}

/// Decode the first JSON value of a body; trailing bytes are ignored (as a streaming decoder does).
pub fn first_value<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    serde_json::Deserializer::from_slice(bytes)
        .into_iter::<T>()
        .next()
        .unwrap_or_else(|| Err(serde::de::Error::custom("EOF")))
        .map_err(|e| e.to_string())
}

/// Native arguments arrive as a JSON-encoded string (or, from some servers, a bare value). Absent, `""`
/// or whitespace mean `{}`. Anything that is not then valid JSON is a provider protocol failure: the
/// whole call fails, with no partial tool calls (WIRE clause 5).
fn native_args(raw: Option<Box<RawValue>>) -> Result<Box<RawValue>, String> {
    match raw {
        None => args_text(""),
        Some(r) if r.get().starts_with('"') => args_text(
            &serde_json::from_str::<String>(r.get())
                .map_err(|e| format!("arguments string does not decode: {e}"))?,
        ),
        Some(r) => Ok(r),
    }
}

/// Tool arguments as text, by the one rule every path shares: empty or whitespace is `{}`; anything
/// else must be JSON.
pub fn args_text(text: &str) -> Result<Box<RawValue>, String> {
    if text.trim().is_empty() {
        return RawValue::from_string("{}".into()).map_err(|e| e.to_string());
    }
    serde_json::from_str::<Box<RawValue>>(text)
        .map_err(|_| format!("arguments are not JSON ({} bytes)", text.len()))
}

/// Encode messages: system hoisted first; legacy tool observations become user turns; any block
/// content is refused fail-closed (no translation is attempted).
fn encode(call: &Call) -> Result<Vec<Value>, CallError> {
    validate_tool_history(&call.messages).map_err(CallError::transport_local)?;
    let mut out = Vec::with_capacity(call.messages.len() + 1);
    if !call.system.is_empty() {
        out.push(json!({"role": "system", "content": call.system}));
    }
    for m in &call.messages {
        out.push(encode_one(m)?);
    }
    Ok(out)
}

fn encode_one(m: &Message) -> Result<Value, CallError> {
    let mut role = m.role.as_str();
    let mut content = m.content.clone();
    if role == "tool" && m.tool_call_id.is_empty() {
        role = "user";
        if let Some(t) = m.text() {
            content = Value::String(format!("[tool observation]\n{t}"));
        }
    } else if role != "assistant" && role != "tool" {
        role = "user";
    }
    if !m.content.is_string() && (!m.content.is_null() || m.tool_calls.is_empty()) {
        return Err(CallError::transport_local(format!(
            "openai_compat: block content unsupported (role {})",
            m.role
        )));
    }
    let mut wire = json!({"role": role, "content": content});
    if !m.tool_call_id.is_empty() {
        wire["tool_call_id"] = json!(m.tool_call_id);
    }
    if !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|c| {
                let args = c.args.as_ref().map(|a| a.get().to_string()).unwrap_or_default();
                json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": args}})
            })
            .collect();
        wire["tool_calls"] = Value::Array(calls);
    }
    Ok(wire)
}

fn name_at(s: &str) -> Option<(&str, &str)> {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    s[end..].strip_prefix('>').map(|rest| (&s[..end], rest))
}

/// Lift `<function=name><parameter=k>v</parameter></function>` text into tool calls (WIRE.md compatibility):
/// ids `xml-1..`, values parsed as JSON when they are JSON, else kept as strings.
pub fn hermes_tool_calls(text: &str) -> Vec<ReplyToolCall> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(off) = text[at..].find("<function=") {
        let start = at + off;
        let after = &text[start + "<function=".len()..];
        let Some((name, rest)) = name_at(after) else {
            at = start + 1;
            continue;
        };
        let Some(close) = rest.find("</function>") else {
            at = start + 1;
            continue;
        };
        let inner = &rest[..close];
        let mut args = Map::new();
        let mut p = 0;
        while let Some(poff) = inner[p..].find("<parameter=") {
            let pstart = p + poff;
            let pafter = &inner[pstart + "<parameter=".len()..];
            let Some((key, prest)) = name_at(pafter) else {
                p = pstart + 1;
                continue;
            };
            let Some(pclose) = prest.find("</parameter>") else {
                p = pstart + 1;
                continue;
            };
            let raw = prest[..pclose].trim();
            let value = serde_json::from_str::<Value>(raw)
                .unwrap_or_else(|_| Value::String(raw.to_string()));
            args.insert(key.to_string(), value);
            p = inner.len() - prest.len() + pclose + "</parameter>".len();
        }
        let body = serde_json::to_string(&Value::Object(args)).unwrap_or_else(|_| "{}".into());
        if let Ok(args) = RawValue::from_string(body) {
            out.push(ReplyToolCall {
                id: format!("xml-{}", out.len() + 1),
                name: name.to_string(),
                args,
            });
        }
        at = text.len() - rest.len() + close + "</function>".len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_dialect() {
        let text = "thinking\n<function=fs_read>\n<parameter=path>\nsrc/a.txt\n</parameter>\n<parameter=limit>\n20\n</parameter>\n</function>\n<function=noop></function>";
        let calls = hermes_tool_calls(text);
        let got: Vec<(String, String, String)> = calls
            .iter()
            .map(|c| (c.id.clone(), c.name.clone(), c.args.get().to_string()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "xml-1".into(),
                    "fs_read".into(),
                    r#"{"limit":20,"path":"src/a.txt"}"#.into()
                ),
                ("xml-2".into(), "noop".into(), "{}".into())
            ]
        );
        assert!(hermes_tool_calls("<function=bad-name></function>").is_empty());
    }

    /// Feed SSE payloads; the text the deltas carried and the terminal event.
    fn feed(payloads: &[&str]) -> (String, Result<Event, CallError>) {
        let mut acc = Accum {
            name: "vllm".into(),
            ..Accum::default()
        };
        let mut text = String::new();
        for p in payloads {
            match acc.feed(p.as_bytes()) {
                Ok(Some(Event::Delta(d))) => text.push_str(&d),
                Ok(Some(ev)) => return (text, Ok(ev)),
                Ok(None) => {}
                Err(e) => return (text, Err(e)),
            }
        }
        (text, Err(CallError::transport_local("no terminal")))
    }

    fn calls(c: &[ReplyToolCall]) -> Vec<(String, String, String)> {
        c.iter()
            .map(|t| (t.id.clone(), t.name.clone(), t.args.get().to_string()))
            .collect()
    }

    #[test]
    fn stream_equals_unary_for_native_tool_calls() {
        let unary: Out = serde_json::from_str(
            r#"{"choices":[{"finish_reason":"tool_calls","message":{"content":"look",
            "tool_calls":[{"id":"c1","function":{"name":"fs.read","arguments":"{\"path\": \"a\"}"}},
                          {"id":"c2","function":{"name":"noop","arguments":""}}]}}],
            "usage":{"prompt_tokens":5,"completion_tokens":4,"cost":0.25}}"#,
        )
        .unwrap_or_default();
        let want = reply("vllm", unary, 0, &mut json!({}))
            .ok()
            .unwrap_or_default();
        let (text, stop) = feed(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"lo"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"ok"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"fs.read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\": \"a\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"noop"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":null}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":4,"cost":0.25}}"#,
            "[DONE]",
        ]);
        assert_eq!(text, want.text);
        let Ok(Event::Stop(stop)) = stop else {
            panic!("expected a stop, got {stop:?}");
        };
        assert_eq!(calls(&stop.tool_calls), calls(&want.tool_calls));
        assert_eq!(
            (
                stop.reason.as_str(),
                stop.input_tokens,
                stop.output_tokens,
                stop.charged
            ),
            ("tool_use", 5, 4, Some(0.25))
        );
        assert_eq!(want.stop_reason, "tool_use");
    }

    #[test]
    fn stream_hermes_and_failures() {
        let (text, stop) = feed(&[
            r#"{"choices":[{"index":0,"delta":{"content":"<function=fs_read><parameter=path>a</parameter>"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"</function>"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        assert!(text.ends_with("</function>"));
        let Ok(Event::Stop(stop)) = stop else {
            panic!("expected a stop");
        };
        assert_eq!(
            calls(&stop.tool_calls),
            vec![("xml-1".into(), "fs_read".into(), r#"{"path":"a"}"#.into())]
        );
        assert_eq!(stop.reason, "tool_use");
        let (_, bad) = feed(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"n","arguments":"{nope"}}]}}]}"#,
            "[DONE]",
        ]);
        assert!(matches!(bad, Err(CallError::ProviderFailed(_))));
        let (_, whole) = feed(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"a","function":{"name":"x","arguments":"{}"}},{"id":"b","function":{"name":"y","arguments":{"k":1}}}]},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        let Ok(Event::Stop(whole)) = whole else {
            panic!("expected a stop");
        };
        assert_eq!(
            calls(&whole.tool_calls),
            vec![
                ("a".into(), "x".into(), "{}".into()),
                ("b".into(), "y".into(), r#"{"k":1}"#.into())
            ]
        );
        let (_, err) = feed(&[r#"{"error":{"message":"overloaded"}}"#]);
        assert!(matches!(err, Ok(Event::UpstreamError(m)) if m == "overloaded"));
        let (_, garbage) = feed(&["{not json"]);
        assert!(matches!(garbage, Err(CallError::Transport(_))));
    }

    #[test]
    fn native_argument_forms() {
        let raw = |s: &str| RawValue::from_string(s.into()).ok();
        let got = |r| native_args(r).map(|r| r.get().to_string()).ok();
        assert_eq!(got(None).as_deref(), Some("{}"));
        assert_eq!(got(raw(r#""  ""#)).as_deref(), Some("{}"));
        assert_eq!(
            got(raw(r#""{\"path\": \"a\"}""#)).as_deref(),
            Some(r#"{"path": "a"}"#)
        );
        assert_eq!(got(raw(r#"{"k":1}"#)).as_deref(), Some(r#"{"k":1}"#));
        assert_eq!(got(raw("null")).as_deref(), Some("null"));
        let f: OutFunction =
            serde_json::from_str(r#"{"name":"n","arguments":null}"#).unwrap_or_default();
        assert_eq!(
            f.arguments.map(|r| r.get().to_string()).as_deref(),
            Some("null")
        );
        assert!(
            native_args(raw(r#""{\"path\": ""#))
                .is_err_and(|e| e.starts_with("arguments are not JSON"))
        );
    }
}
