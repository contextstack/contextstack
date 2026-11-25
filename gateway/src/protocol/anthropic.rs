//! Anthropic Messages: unary and stream. Tool names are encoded injectively for
//! Anthropic's name rule ('.' → `_d`, '_' → `_u`) and restored on the way back; neutral image blocks
//! become the `source` shape.
//!
//! The stream is a pull state machine over the upstream body: text deltas, then exactly one terminal.
//! `tool_use` blocks and their `input_json_delta` fragments are collected and land on the stop as
//! tool calls, parsed by the unary rules. A body that ends without `message_stop`, a read error and the
//! deadline are errors, never a stop (WIRE clause 4). Dropping the stream drops the upstream connection.

use super::history::{anthropic_images, validate_tool_history};
use super::sse::Lines;
use super::{Call, Event, Http, Reply, Stop, error_body, request_id_header};
use crate::errors::CallError;
use crate::wire::{ReplyToolCall, nullable};
use serde::Deserialize;
use serde_json::{Map, Value, json, value::RawValue};
use std::collections::HashMap;
use tokio::time::Instant;

/// One Anthropic endpoint.
pub struct Anthropic<'a> {
    /// Base URL (no `/v1`).
    pub base: &'a str,
    /// Model when the call names none.
    pub default_model: &'a str,
}

struct Wire {
    raw: Vec<u8>,
    inverse: HashMap<String, String>,
}

/// Encode a tool name for Anthropic; refuses a rune outside `[A-Za-z0-9._-]`.
pub fn encode_name(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("anthropic: empty tool name".into());
    }
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '.' => out.push_str("_d"),
            '_' => out.push_str("_u"),
            c if c.is_ascii_alphanumeric() || c == '-' => out.push(c),
            c => {
                return Err(format!(
                    "anthropic: tool name {name:?} has unsupported rune {c:?}"
                ));
            }
        }
    }
    Ok(out)
}

#[derive(Deserialize, Default)]
struct Out {
    #[serde(default, deserialize_with = "nullable")]
    content: Vec<Block>,
    #[serde(default, deserialize_with = "nullable")]
    stop_reason: String,
    #[serde(default, deserialize_with = "nullable")]
    usage: OutUsage,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[allow(
    clippy::struct_field_names,
    reason = "Anthropic usage field names are wire-fixed"
)]
struct OutUsage {
    #[serde(default, deserialize_with = "nullable")]
    input_tokens: i64,
    #[serde(default, deserialize_with = "nullable")]
    output_tokens: i64,
    #[serde(default, deserialize_with = "nullable")]
    cache_read_input_tokens: i64,
    #[serde(default, deserialize_with = "nullable")]
    cache_creation_input_tokens: i64,
}

#[derive(Deserialize, Default)]
struct Block {
    #[serde(default, rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(default, deserialize_with = "nullable")]
    text: String,
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    name: String,
    #[serde(default)]
    input: Option<Box<RawValue>>,
}

impl Anthropic<'_> {
    fn marshal(
        &self,
        call: &Call,
        stream: bool,
        trace: &mut crate::trace::Exchange,
    ) -> Result<Wire, CallError> {
        let model = call.model_or(self.default_model);
        let max_tokens = call.max_tokens_or_default();
        let messages = encode(call).map_err(CallError::transport_local)?;
        let mut inverse = HashMap::new();
        let mut tools = Vec::new();
        for t in &call.tools {
            let encoded = encode_name(&t.name).map_err(CallError::transport_local)?;
            if let Some(prev) = inverse.get(&encoded).filter(|p| **p != t.name) {
                return Err(CallError::transport_local(format!(
                    "anthropic: tool names {prev:?} and {:?} collide after encoding",
                    t.name
                )));
            }
            let mut entry = Map::new();
            entry.insert("name".into(), json!(encoded));
            if !t.description.is_empty() {
                entry.insert("description".into(), json!(t.description));
            }
            let schema = t.schema_value();
            entry.insert("input_schema".into(), schema);
            tools.push(Value::Object(entry));
            inverse.insert(encoded, t.name.clone());
        }
        let mut body = json!({"model": model, "max_tokens": max_tokens, "messages": messages});
        if stream {
            body["stream"] = json!(true);
        }
        if !call.system.is_empty() {
            body["system"] = json!(call.system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(choice) = &call.tool_choice {
            match (choice.as_str(), choice["type"].as_str()) {
                (Some("required"), _) => body["tool_choice"] = json!({"type": "any"}),
                (Some("auto"), _) => body["tool_choice"] = json!({"type": "auto"}),
                (Some("none"), _) => body["tool_choice"] = json!({"type": "none"}),
                (_, Some("function")) if let Some(name) = choice["function"]["name"].as_str() => {
                    let name = encode_name(name).map_err(CallError::transport_local)?;
                    body["tool_choice"] = json!({"type": "tool", "name": name});
                }
                _ => trace.drop_tool_choice(),
            }
        }
        let raw = serde_json::to_vec(&body)
            .map_err(|e| CallError::transport_local(format!("anthropic: marshal request: {e}")))?;
        Ok(Wire { raw, inverse })
    }

    async fn post(
        &self,
        http: &Http,
        key: &str,
        call: &Call,
        raw: Vec<u8>,
        stream: bool,
        trace: &mut crate::trace::Exchange,
    ) -> Result<reqwest::Response, CallError> {
        let url = format!("{}/v1/messages", self.base.trim_end_matches('/'));
        let mut req = http
            .client()
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01");
        if stream {
            req = req.header("Accept", "text/event-stream");
        }
        if let Some(id) = request_id_header(&call.request_id) {
            req = req.header("X-Request-Id", id);
        }
        let resp = http.model_post(req, &url, raw, trace).await?;
        let status = resp.status().as_u16();
        trace.headers(status);
        if status >= 300 {
            let retry_after_ms = crate::errors::parse_retry_after(resp.headers());
            let text = error_body(resp, 4096).await;
            trace.upstream_error_body(&text);
            return Err(CallError::upstream_http(
                status,
                "anthropic",
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
        key: &str,
        call: &Call,
        deadline: Instant,
        trace: &mut crate::trace::Exchange,
    ) -> Result<Reply, CallError> {
        let wire = self.marshal(call, false, trace)?;
        trace.begin_attempt("anthropic", &wire.raw, false);
        let url = format!("{}/v1/messages", self.base.trim_end_matches('/'));
        let start = std::time::Instant::now();
        super::until(deadline, "Post", &url, async {
            let resp = self.post(http, key, call, wire.raw, false, trace).await?;
            let latency_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
            let bytes = super::success_body(resp, "Post", &url).await?;
            let out: Out =
                super::openai::first_value(&bytes).map_err(CallError::transport_local)?;
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            for b in out.content {
                match b.kind.as_str() {
                    "text" => text.push_str(&b.text),
                    "tool_use" => {
                        let args = b.input.or_else(|| RawValue::from_string("{}".into()).ok());
                        let Some(args) = args else { continue };
                        let name = wire.inverse.get(&b.name).cloned().unwrap_or(b.name);
                        tool_calls.push(ReplyToolCall {
                            id: b.id,
                            name,
                            invalid_arguments: false,
                            args,
                        });
                    }
                    _ => {}
                }
            }
            Ok(Reply {
                text,
                input_tokens: out.usage.input_tokens,
                output_tokens: out.usage.output_tokens,
                cache_read_tokens: out.usage.cache_read_input_tokens,
                cache_write_tokens: out.usage.cache_creation_input_tokens,
                charged: None,
                provider: "anthropic".into(),
                latency_ms,
                stop_reason: out.stop_reason,
                tool_calls,
            })
        })
        .await
    }

    /// Open a stream: the request and its status are settled here; events follow from [`Stream::next`].
    pub async fn stream(
        &self,
        http: &Http,
        key: &str,
        call: &Call,
        deadline: Instant,
        trace: &mut crate::trace::Exchange,
    ) -> Result<Stream, CallError> {
        let wire = self.marshal(call, true, trace)?;
        trace.begin_attempt("anthropic", &wire.raw, true);
        let url = format!("{}/v1/messages", self.base.trim_end_matches('/'));
        let resp = super::until(
            deadline,
            "Post",
            &url,
            self.post(http, key, call, wire.raw, true, trace),
        )
        .await?;
        Ok(Stream {
            lines: Lines::new(resp, deadline, "anthropic stream"),
            inverse: wire.inverse,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            usage_known: false,
            stop: String::new(),
            tools: Vec::new(),
            done: false,
        })
    }
}

/// An open Anthropic event stream.
pub struct Stream {
    lines: Lines,
    inverse: HashMap<String, String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    usage_known: bool,
    stop: String,
    /// `tool_use` blocks in arrival order.
    tools: Vec<ToolBlock>,
    done: bool,
}

/// One streamed `tool_use` block: its index, id, encoded name, start input and streamed JSON.
struct ToolBlock {
    index: i64,
    id: String,
    name: String,
    start: Option<Box<RawValue>>,
    json: String,
}

#[derive(Deserialize, Default)]
struct Sse {
    #[serde(default, rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(default, deserialize_with = "nullable")]
    index: i64,
    #[serde(default, deserialize_with = "nullable")]
    delta: SseDelta,
    #[serde(default, deserialize_with = "nullable")]
    content_block: Block,
    usage: Option<OutUsage>,
    #[serde(default, deserialize_with = "nullable")]
    message: SseMessage,
    #[serde(default, deserialize_with = "nullable")]
    error: SseError,
}

#[derive(Deserialize, Default)]
struct SseDelta {
    #[serde(default, rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(default, deserialize_with = "nullable")]
    text: String,
    #[serde(default, deserialize_with = "nullable")]
    partial_json: String,
    #[serde(default, deserialize_with = "nullable")]
    stop_reason: String,
}

#[derive(Deserialize, Default)]
struct SseMessage {
    usage: Option<OutUsage>,
}

#[derive(Deserialize, Default)]
struct SseError {
    #[serde(default, deserialize_with = "nullable")]
    message: String,
}

impl Stream {
    /// Tokens accumulated from `message_start` / `message_delta` so far.
    pub fn metered_usage(&self) -> super::MeteredUsage {
        super::MeteredUsage {
            known: self.usage_known,
            input_tokens: self.input,
            output_tokens: self.output,
            cache_read_tokens: self.cache_read,
            cache_write_tokens: self.cache_write,
            charged: None,
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
        while let Some(payload) = self.lines.data().await? {
            if let Some(ev) = self.event(&payload)? {
                return Ok(Some(ev));
            }
        }
        // EOF before message_stop is transport death, never a stop (WIRE clause 4).
        Err(CallError::transport_local(
            "anthropic stream ended without message_stop: unexpected EOF",
        ))
    }

    fn event(&mut self, payload: &[u8]) -> Result<Option<Event>, CallError> {
        let Ok(evt) = serde_json::from_slice::<Sse>(payload) else {
            return Ok(None);
        };
        match evt.kind.as_str() {
            "message_start" => {
                self.absorb_usage(evt.message.usage.as_ref(), "");
            }
            "content_block_start" if evt.content_block.kind == "tool_use" => {
                let b = evt.content_block;
                self.tools.push(ToolBlock {
                    index: evt.index,
                    id: b.id,
                    name: b.name,
                    start: b.input,
                    json: String::new(),
                });
            }
            "content_block_delta" => match evt.delta.kind.as_str() {
                "text_delta" if !evt.delta.text.is_empty() => {
                    return Ok(Some(Event::Delta(evt.delta.text)));
                }
                "input_json_delta" => {
                    if let Some(t) = self.tools.iter_mut().rev().find(|t| t.index == evt.index) {
                        t.json.push_str(&evt.delta.partial_json);
                    }
                }
                _ => {}
            },
            "message_delta" => self.absorb_usage(evt.usage.as_ref(), &evt.delta.stop_reason),
            "message_stop" => return self.finish().map(Some),
            "error" => return Ok(Some(Event::UpstreamError(evt.error.message))),
            _ => {}
        }
        Ok(None)
    }

    /// The stop: tool blocks become tool calls by the unary rules (names restored; streamed input
    /// empty means the start block's input, else `{}`; input that is not JSON fails the call).
    fn finish(&mut self) -> Result<Event, CallError> {
        let mut tool_calls = Vec::with_capacity(self.tools.len());
        for t in std::mem::take(&mut self.tools) {
            let name = self.inverse.get(&t.name).cloned().unwrap_or(t.name);
            let args = if t.json.trim().is_empty() {
                t.start
                    .filter(|s| s.get() != "null")
                    .map_or_else(|| super::openai::args_text(""), Ok)
            } else {
                super::openai::args_text(&t.json)
            }
            .map_err(|e| {
                CallError::ProviderFailed(format!("anthropic: tool call {name:?}: {e}"))
            })?;
            tool_calls.push(ReplyToolCall {
                id: t.id,
                name,
                invalid_arguments: false,
                args,
            });
        }
        let reason = if self.stop.is_empty() {
            "end_turn".to_string()
        } else {
            self.stop.clone()
        };
        Ok(Event::Stop(Stop {
            reason,
            input_tokens: self.input,
            output_tokens: self.output,
            cache_read_tokens: self.cache_read,
            cache_write_tokens: self.cache_write,
            charged: None,
            provider: "anthropic".into(),
            tool_calls,
        }))
    }

    fn absorb_usage(&mut self, usage: Option<&OutUsage>, stop: &str) {
        if let Some(usage) = usage {
            self.usage_known = true;
            if usage.input_tokens > 0 {
                self.input = usage.input_tokens;
            }
            if usage.output_tokens > 0 {
                self.output = usage.output_tokens;
            }
            if usage.cache_read_input_tokens > 0 {
                self.cache_read = usage.cache_read_input_tokens;
            }
            if usage.cache_creation_input_tokens > 0 {
                self.cache_write = usage.cache_creation_input_tokens;
            }
        }
        if !stop.is_empty() {
            self.stop = stop.to_string();
        }
    }
}

/// Encode messages: tool calls become `tool_use` blocks after any text, tool results become user
/// `tool_result` blocks, legacy observations user text, neutral images the `source` shape.
fn encode(call: &Call) -> Result<Vec<Value>, String> {
    validate_tool_history(&call.messages)?;
    let mut out = Vec::with_capacity(call.messages.len());
    for m in &call.messages {
        let mut role = m.role.clone();
        let mut content = anthropic_images(&m.content);
        if !m.tool_calls.is_empty() {
            let mut blocks: Vec<Value> = Vec::new();
            match &content {
                Value::String(t) if !t.is_empty() => {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                Value::Array(parts) => blocks.extend(parts.iter().cloned()),
                _ => {}
            }
            for c in &m.tool_calls {
                let name = encode_name(&c.name)?;
                let input = c
                    .args
                    .as_ref()
                    .and_then(|a| serde_json::from_str::<Value>(a.get()).ok())
                    .unwrap_or(Value::Null);
                blocks.push(json!({"type": "tool_use", "id": c.id, "name": name, "input": input}));
            }
            content = Value::Array(blocks);
        }
        if role == "tool" {
            role = "user".into();
            if !m.tool_call_id.is_empty() {
                content = json!([{"type": "tool_result", "tool_use_id": m.tool_call_id, "content": content, "is_error": m.is_error}]);
            } else if let Value::String(t) = &content {
                content = Value::String(format!("[tool observation]\n{t}"));
            }
        }
        out.push(json!({"role": role, "content": content}));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stream_uses_the_full_call_deadline_and_meters_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await?;
            let events = concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{",
                "\"input_tokens\":5,\"cache_read_input_tokens\":11,\"cache_creation_input_tokens\":23}}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
            );
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{events}",
                events.len()
            );
            socket.write_all(reply.as_bytes()).await
        });
        let http = Http::new()?;
        let mut trace = crate::trace::Exchange::new(
            None,
            crate::trace::Stamp::now(),
            &crate::wire::Request::default(),
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(180);
        let endpoint = Anthropic {
            base: &base,
            default_model: "test",
        };
        let call = Call {
            model: String::new(),
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 32,
            request_id: String::new(),
        };
        let mut stream = endpoint
            .stream(&http, "fake", &call, deadline, &mut trace)
            .await?;
        server.await??;
        assert_eq!(stream.lines.deadline(), deadline);
        assert!(matches!(stream.next().await?, Some(Event::Delta(_))));
        let meter = stream.metered_usage();
        assert_eq!(
            (
                meter.input_tokens,
                meter.cache_read_tokens,
                meter.cache_write_tokens
            ),
            (5, 11, 23)
        );
        Ok(())
    }

    #[test]
    fn tool_names_are_injective_and_refuse_odd_runes() {
        assert_eq!(encode_name("fs.read").ok().as_deref(), Some("fs_dread"));
        assert_eq!(encode_name("fs_read").ok().as_deref(), Some("fs_uread"));
        assert_eq!(encode_name("okf-x").ok().as_deref(), Some("okf-x"));
        assert!(encode_name("fs/read").is_err());
        assert!(encode_name("a b").is_err());
    }
}
