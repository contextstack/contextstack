//! The closed set of wire protocols compiled into the gateway: the `Protocol` enum is the one
//! seam). Endpoints, credentials, model names and prices arrive as data; each catalog endpoint names one
//! of these protocols, and a call is a `match` on it.

pub mod anthropic;
pub mod history;
pub mod mock;
pub mod openai;
pub mod sse;
pub mod typesafe;

use crate::errors::{CallError, TransportDetail};
use crate::wire::{Message, ReplyToolCall};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::time::Duration;
use tokio::time::Instant;

/// A capability declaration: a protocol constant, an endpoint declaration, or an intersection.
/// A field left out is false: unknown capability is not permission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caps {
    /// Image blocks.
    #[serde(default)]
    pub images: bool,
    /// Tool declarations.
    #[serde(default)]
    pub tools: bool,
    /// Streaming.
    #[serde(default)]
    pub streaming: bool,
}

/// The wire protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Anthropic Messages API (unary and SSE).
    AnthropicMessages,
    /// OpenAI-compatible chat/completions (unary and SSE; Hermes XML tool calls).
    OpenAiChatCompletions,
    /// The compiled-in mock: a fixed reply, no connection (unary and stream).
    Mock,
    /// `TypeSafe`'s typed-question API (`/v1/ai/typed-question` only; never a chat model).
    TypesafeSystemOne,
}

impl Protocol {
    /// From a catalog `protocol` value.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "anthropic_messages" => Some(Self::AnthropicMessages),
            "openai_chat_completions" => Some(Self::OpenAiChatCompletions),
            "mock" => Some(Self::Mock),
            "typesafe_systemone" => Some(Self::TypesafeSystemOne),
            _ => None,
        }
    }

    /// The protocol constant: the upper bound endpoint declarations narrow.
    pub const fn caps(self) -> Caps {
        match self {
            Self::AnthropicMessages => Caps {
                images: true,
                tools: true,
                streaming: true,
            },
            Self::OpenAiChatCompletions | Self::Mock => Caps {
                images: false,
                tools: true,
                streaming: true,
            },
            Self::TypesafeSystemOne => Caps {
                images: false,
                tools: false,
                streaming: false,
            },
        }
    }
}

/// A tool declaration; the schema passes through verbatim (the caller owns JSON Schema).
#[derive(Debug, Clone)]
pub struct Tool {
    /// Caller's name.
    pub name: String,
    /// Description (omitted when empty).
    pub description: String,
    /// JSON Schema (`{"type":"object"}` when absent).
    pub schema: Option<Box<RawValue>>,
}

/// The narrow request every adapter takes.
#[derive(Debug, Clone)]
pub struct Call {
    /// Resolved model name.
    pub model: String,
    /// Hoisted system text.
    pub system: String,
    /// Non-system messages.
    pub messages: Vec<Message>,
    /// Tools.
    pub tools: Vec<Tool>,
    /// Optional OpenAI-compatible tool selection.
    pub tool_choice: Option<serde_json::Value>,
    /// Sampling limit (0: 4096).
    pub max_tokens: i64,
    /// For X-Request-Id and trace rows.
    pub request_id: String,
}

impl Tool {
    /// The declared JSON Schema, or `{"type":"object"}` when absent.
    pub fn schema_value(&self) -> serde_json::Value {
        self.schema
            .as_ref()
            .and_then(|s| serde_json::from_str(s.get()).ok())
            .unwrap_or_else(|| serde_json::json!({"type": "object"}))
    }
}

impl Call {
    /// The model to send: the call's, else the endpoint's default.
    pub fn model_or<'a>(&'a self, default: &'a str) -> &'a str {
        if self.model.is_empty() {
            default
        } else {
            &self.model
        }
    }

    /// The sampling limit to send (4096 when unset).
    pub fn max_tokens_or_default(&self) -> i64 {
        if self.max_tokens <= 0 {
            4096
        } else {
            self.max_tokens
        }
    }
}

/// A unary reply.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    /// Text.
    pub text: String,
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
    /// Cache read tokens.
    pub cache_read_tokens: i64,
    /// Cache write / creation tokens.
    pub cache_write_tokens: i64,
    /// Provider-reported charge.
    pub charged: Option<f64>,
    /// Provider id.
    pub provider: String,
    /// Upstream latency.
    pub latency_ms: i64,
    /// Stop reason ("" becomes `end_turn`).
    pub stop_reason: String,
    /// Structured tool calls.
    pub tool_calls: Vec<ReplyToolCall>,
}

/// One stream event, in lifecycle order: deltas, then exactly one terminal (stop or upstream error).
/// Transport failure, EOF without a stop, a reply that breaks its protocol and the deadline surface as
/// `Err` from the stream.
#[derive(Debug, Clone)]
pub enum Event {
    /// Non-empty text.
    Delta(String),
    /// The terminal stop: everything the unary [`Reply`] carries except the text, which the deltas
    /// already delivered.
    Stop(Stop),
    /// The upstream's own error event.
    UpstreamError(String),
}

/// Usage the upstream reported (or accumulated) before a stream ended.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeteredUsage {
    /// Prompt tokens when known.
    pub input_tokens: i64,
    /// Completion tokens when known.
    pub output_tokens: i64,
    /// Cache reads when known.
    pub cache_read_tokens: i64,
    /// Cache writes when known.
    pub cache_write_tokens: i64,
    /// Provider-reported charge when known.
    pub charged: Option<f64>,
}

/// A stream's terminal stop. The deltas' text plus this equal the unary reply for the same input.
#[derive(Debug, Clone, Default)]
pub struct Stop {
    /// Stop reason (`end_turn` when the upstream gave none).
    pub reason: String,
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
    /// Cache read tokens.
    pub cache_read_tokens: i64,
    /// Cache write / creation tokens.
    pub cache_write_tokens: i64,
    /// Provider-reported charge.
    pub charged: Option<f64>,
    /// Provider id, as the unary reply names it.
    pub provider: String,
    /// Structured tool calls, parsed by the unary rules.
    pub tool_calls: Vec<ReplyToolCall>,
}

/// The shared outbound HTTP client: Go's proxy rules (`HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY`, never for
/// loopback) and redirects that stay on the requested host.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    /// Build from the process environment.
    pub fn new() -> Result<Self, String> {
        let pick = |names: [&str; 2]| {
            names
                .iter()
                .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
        };
        let https = pick(["HTTPS_PROXY", "https_proxy"]);
        let http = pick(["HTTP_PROXY", "http_proxy"]);
        let no_proxy = pick(["NO_PROXY", "no_proxy"]).unwrap_or_default();
        let proxy = reqwest::Proxy::custom(move |url| {
            let host = url.host_str()?.trim_matches(['[', ']']).to_lowercase();
            if crate::catalog::loopback(&host) || bypass(&no_proxy, &host) {
                return None;
            }
            let chosen = if url.scheme() == "https" {
                https.as_deref()
            } else {
                http.as_deref()
            }?;
            let with_scheme = if chosen.contains("://") {
                chosen.to_string()
            } else {
                format!("http://{chosen}")
            };
            reqwest::Url::parse(&with_scheme).ok()
        });
        let redirect = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("stopped after 10 redirects");
            }
            let first = attempt.previous().first().and_then(authority);
            let next = authority(attempt.url());
            if first == next {
                attempt.follow()
            } else {
                let msg = format!(
                    "refused cross-host redirect from {} to {}",
                    first.unwrap_or_default(),
                    next.unwrap_or_default()
                );
                attempt.error(msg)
            }
        });
        let client = cs_http_client::builder()
            .no_proxy()
            .proxy(proxy)
            .redirect(redirect)
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client })
    }

    /// Send a model POST, tracking body dispatch while preserving explicit redirects.
    async fn model_post(
        &self,
        req: reqwest::RequestBuilder,
        url: &str,
        raw: Vec<u8>,
        trace: &mut crate::trace::Exchange,
    ) -> Result<reqwest::Response, CallError> {
        let mut req = req.build().map_err(|e| transport("Post", url, &e, false))?;
        let raw = bytes::Bytes::from(raw);
        let mut sent_before_redirect = false;
        for redirects in 0..10 {
            let mut post = req
                .try_clone()
                .ok_or_else(|| CallError::transport_local("could not clone POST headers"))?;
            *post.body_mut() = Some(trace.request_body(raw.clone()));
            let response = self.client.execute(post).await.map_err(|e| {
                if redirects == 0 && e.is_request() && !e.is_connect() && closed_before_headers(&e)
                {
                    return CallError::connection_lost_before_headers(transport_message(
                        "Post", url, &e,
                    ));
                }
                transport(
                    "Post",
                    url,
                    &e,
                    sent_before_redirect || trace.request_body_sent(),
                )
            })?;
            if !matches!(response.status().as_u16(), 307 | 308) || response.url() != req.url() {
                return Ok(response);
            }
            // reqwest cannot clone a tracked body for a 307/308. Follow only an explicit
            // redirect allowed by the same authority rule as reqwest's redirect policy.
            let Some(next) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|location| location.to_str().ok())
                .and_then(|location| response.url().join(location).ok())
            else {
                return Ok(response);
            };
            if !matches!(next.scheme(), "http" | "https")
                || authority(&next) != authority(req.url())
            {
                return Err(CallError::transport_local("refused cross-host redirect"));
            }
            sent_before_redirect |= trace.request_body_sent();
            *req.url_mut() = next;
        }
        Err(CallError::transport_local("stopped after 10 redirects"))
    }

    /// The underlying client.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

fn authority(url: &reqwest::Url) -> Option<String> {
    url.host_str().map(|host| {
        format!(
            "{}:{}",
            host.to_lowercase(),
            url.port_or_known_default().unwrap_or(0)
        )
    })
}

fn bypass(no_proxy: &str, host: &str) -> bool {
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|p| {
            p == "*" || {
                let p = p.trim_start_matches('.').split(':').next().unwrap_or("");
                host == p || host.ends_with(&format!(".{p}"))
            }
        })
}

fn transport_message(method: &str, url: &str, err: &reqwest::Error) -> String {
    let mut cause = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(s) = source {
        cause = s.to_string();
        source = s.source();
    }
    format!("{method} {url:?}: {cause}")
}

/// A close while waiting for headers occurs before any response body is delivered.
fn closed_before_headers(err: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        if cause
            .downcast_ref::<hyper::Error>()
            .is_some_and(hyper::Error::is_incomplete_message)
            || cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                )
            })
        {
            return true;
        }
        source = cause.source();
    }
    false
}

fn transport(method: &str, url: &str, err: &reqwest::Error, body_sent: bool) -> CallError {
    let message = transport_message(method, url, err);
    let is_connect = err.is_connect();
    if is_connect && !body_sent {
        CallError::Transport(TransportDetail::network(message))
    } else if is_connect || err.is_request() || err.is_timeout() {
        CallError::connection_lost(message)
    } else {
        CallError::transport_local(message)
    }
}

/// A transport error after receiving headers.
pub fn transport_after_response(method: &str, url: &str, err: &reqwest::Error) -> CallError {
    CallError::connection_lost(transport_message(method, url, err))
}

/// The deadline error.
pub fn deadline(method: &str, url: &str) -> CallError {
    CallError::deadline(format!("{method} {url:?}: context deadline exceeded"))
}

/// Await `fut` until `deadline`.
pub async fn until<T>(
    deadline: Instant,
    method: &str,
    url: &str,
    fut: impl Future<Output = Result<T, CallError>>,
) -> Result<T, CallError> {
    tokio::time::timeout_at(deadline, fut)
        .await
        .unwrap_or_else(|_| Err(self::deadline(method, url)))
}

/// Read at most `limit` bytes of an error body.
pub async fn error_body(mut resp: reqwest::Response, limit: usize) -> String {
    let mut out = Vec::new();
    while out.len() < limit {
        match resp.chunk().await {
            Ok(Some(c)) => out.extend_from_slice(&c),
            _ => break,
        }
    }
    out.truncate(limit);
    String::from_utf8_lossy(&out).into_owned()
}

/// The most of a successful upstream body the gateway reads (OpenAI-compat and Anthropic adapters).
pub const MAX_SUCCESS_BODY: usize = 64 * 1024 * 1024;

/// Read a 2xx body up to [`MAX_SUCCESS_BODY`]; over the cap is a local transport fault.
pub async fn success_body(
    resp: reqwest::Response,
    method: &str,
    url: &str,
) -> Result<bytes::Bytes, CallError> {
    bounded_body(resp, method, url, MAX_SUCCESS_BODY).await
}

/// Read a response with a hard cap on buffered bytes.
async fn bounded_body(
    mut resp: reqwest::Response,
    method: &str,
    url: &str,
    limit: usize,
) -> Result<bytes::Bytes, CallError> {
    let mut out = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                if out.len().saturating_add(c.len()) > limit {
                    return Err(CallError::transport_local(format!(
                        "{method} {url:?}: upstream response body exceeds {limit} bytes"
                    )));
                }
                out.extend_from_slice(&c);
            }
            Ok(None) => break,
            Err(e) => return Err(transport_after_response(method, url, &e)),
        }
    }
    Ok(bytes::Bytes::from(out))
}

/// Whether X-Request-Id may carry this id.
pub fn request_id_header(id: &str) -> Option<&str> {
    (!id.is_empty() && id.len() <= 256 && !id.contains(['\r', '\n'])).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_matching() {
        assert!(bypass("example.com,.internal", "api.example.com"));
        assert!(bypass("example.com", "example.com"));
        assert!(!bypass("example.com", "notexample.com"));
        assert!(bypass("*", "anything"));
        assert!(!bypass("", "anything"));
    }

    #[test]
    fn protocol_constants() {
        assert_eq!(Protocol::from_name("mock"), Some(Protocol::Mock));
        assert!(Protocol::AnthropicMessages.caps().streaming);
        assert!(Protocol::OpenAiChatCompletions.caps().streaming);
        assert!(Protocol::Mock.caps().streaming);
        assert!(!Protocol::OpenAiChatCompletions.caps().images);
    }

    #[tokio::test]
    async fn connect_error_retries_only_before_body_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let url = "http://127.0.0.1:1";
        let client = cs_http_client::builder().no_proxy().build()?;
        let error = client
            .post(url)
            .body("{}")
            .send()
            .await
            .err()
            .ok_or("closed port accepted connection")?;
        assert!(matches!(
            transport("Post", url, &error, false),
            CallError::Transport(TransportDetail { network: true, .. })
        ));
        assert!(transport("Post", url, &error, true).is_connection_lost());
        Ok(())
    }

    #[tokio::test]
    async fn success_body_rejects_oversized_stream() -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let url = format!("http://{addr}/v1/chat/completions");
        let chunk = vec![b'x'; 1024 * 1024];
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let so_far = String::from_utf8_lossy(&buf[..n]);
                if so_far.contains("\r\n\r\n") {
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                ?;
            for _ in 0..70 {
                socket
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await?;
                socket.write_all(&chunk).await?;
                socket.write_all(b"\r\n").await?;
            }
            socket.write_all(b"0\r\n\r\n").await
        });
        let client = cs_http_client::builder().build()?;
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await?;
        let err = success_body(resp, "Post", &url)
            .await
            .err()
            .ok_or("oversized response was accepted")?;
        assert!(matches!(err, CallError::Transport(_)));
        assert!(err.to_string().contains("exceeds"), "unexpected: {err}");
        server.abort();
        Ok(())
    }
}
