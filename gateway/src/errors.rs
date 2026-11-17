//! The error hierarchy and its single mapping to the stable wire error classes (WIRE clause 5).
//!
//! Inside the gateway a failed call is a [`CallError`]; the handler maps it once, here, to a
//! [`WireError`] (`class`, `code`, `recoverable`). Request-shape refusals that happen before any
//! dispatch are built as `WireError`s directly by the handler.

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Why a provider-facing step failed, independent of how it is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Upstream said try again (429, 5xx, other non-auth status).
    ProviderTransient,
    /// Upstream or the credential store refused authority (401, 403, missing or refused key).
    ProviderAuth,
    /// Admission refused (queue full, queue timeout, unbound deployment) or nothing serves the name.
    CapacityExhausted,
    /// Inventory names a provider id the running catalog did not register.
    ProviderNotConfigured,
    /// The snapshot's policy denied the call (egress or capability).
    PolicyDenied,
    /// A stream asked of an endpoint whose protocol or declaration cannot stream (unary still serves).
    StreamingUnsupported,
    /// Upstream rejected the request shape or context (4xx that will not heal on retry).
    ProviderRequestRejected,
    /// Upstream quota or billing exhausted (not rate-limited).
    ProviderQuotaExhausted,
}

/// A transport failure before a complete provider reply is classified.
#[derive(Debug, Clone)]
pub struct TransportDetail {
    /// Human text (never a credential).
    pub message: String,
    /// Connection-level failures that may succeed on retry.
    pub network: bool,
}

impl TransportDetail {
    /// Decode, deadline, or other non-network transport faults.
    pub fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            network: false,
        }
    }

    /// Connection reset, refused, timeout at the socket, and similar.
    pub fn network(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            network: true,
        }
    }
}

impl std::fmt::Display for TransportDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A failed model call or stream, before it becomes wire text.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CallError {
    /// A classified failure reported by a provider, admission, policy or the credential store.
    #[error("{message}")]
    Classified {
        /// The failure's class.
        class: FailureClass,
        /// Human text (never a credential).
        message: String,
        /// From an upstream `Retry-After` header when present.
        retry_after_ms: Option<i64>,
    },
    /// The router could not dispatch (no provider registered for the name).
    #[error("{message}")]
    Routing {
        /// The failure's class.
        class: FailureClass,
        /// Human text.
        message: String,
    },
    /// Anything unclassified: transport death, undecodable reply, refused request shape, deadline.
    #[error("{0}")]
    Transport(TransportDetail),
    /// The provider broke its own protocol (a reply that cannot become a valid envelope).
    #[error("{0}")]
    ProviderFailed(String),
}

impl CallError {
    /// A classified failure.
    pub fn classified(class: FailureClass, message: impl Into<String>) -> Self {
        Self::Classified {
            class,
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// Upstream HTTP status to a class (legacy callers without headers).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn status(status: u16, message: String) -> Self {
        Self::upstream(status, message, None, "")
    }

    /// Upstream non-2xx with optional `Retry-After` and body text for quota/context signals.
    pub fn upstream(status: u16, message: String, retry_after_ms: Option<i64>, body: &str) -> Self {
        let class = classify_upstream(status, body);
        Self::Classified {
            class,
            message,
            retry_after_ms,
        }
    }

    /// Upstream non-2xx: wire text is short; `body` is for classification and logging only.
    pub fn upstream_http(
        status: u16,
        provider: &str,
        retry_after_ms: Option<i64>,
        body: &str,
    ) -> Self {
        let class = classify_upstream(status, body);
        let message = upstream_wire_message(status, class, provider, body);
        Self::Classified {
            class,
            message,
            retry_after_ms,
        }
    }

    /// True when the gateway may attempt another upstream call before returning.
    pub fn gateway_retryable(&self) -> bool {
        matches!(
            self,
            Self::Classified {
                class: FailureClass::ProviderTransient,
                ..
            } | Self::Transport(TransportDetail { network: true, .. })
        )
    }

    /// Local transport fault (decode, deadline, malformed stream).
    pub fn transport_local(message: impl Into<String>) -> Self {
        Self::Transport(TransportDetail::local(message))
    }

    /// Retry hint carried from the upstream response.
    pub fn retry_after_ms(&self) -> Option<i64> {
        match self {
            Self::Classified { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        }
    }

    /// The single mapping to the wire.
    pub fn to_wire(&self) -> WireError {
        use FailureClass::{
            CapacityExhausted, PolicyDenied, ProviderAuth, ProviderNotConfigured, ProviderQuotaExhausted,
            ProviderRequestRejected, ProviderTransient, StreamingUnsupported,
        };
        let (class, message, routed, retry_after_ms) = match self {
            Self::ProviderFailed(message) => {
                return WireError::runtime("gateway.provider_failed", message);
            }
            Self::Classified {
                class,
                message,
                retry_after_ms,
            } => (Some(*class), message, false, *retry_after_ms),
            Self::Routing { class, message } => (Some(*class), message, true, None),
            Self::Transport(TransportDetail { message, network }) => {
                return if *network {
                    WireError::retryable("gateway.provider_transient", message)
                } else {
                    WireError::runtime("gateway.transport_failed", message)
                };
            }
        };
        let mut wire = match class {
            Some(ProviderTransient) if !routed => {
                WireError::retryable("gateway.provider_transient", message)
            }
            Some(ProviderRequestRejected) => {
                WireError::fatal("gateway.provider_request_rejected", message)
            }
            Some(ProviderQuotaExhausted) => {
                WireError::fatal("gateway.provider_quota_exhausted", message)
            }
            Some(ProviderAuth) => WireError::fatal("gateway.provider_auth", message),
            Some(ProviderNotConfigured) => {
                WireError::fatal("gateway.provider_not_configured", message)
            }
            Some(CapacityExhausted) => WireError::retryable("gateway.capacity_exhausted", message),
            Some(PolicyDenied) => WireError::fatal("gateway.policy_denied", message),
            Some(StreamingUnsupported) => {
                WireError::fatal("gateway.streaming_unsupported", message)
            }
            Some(ProviderTransient) | None => {
                WireError::runtime("gateway.transport_failed", message)
            }
        };
        wire.retry_after_ms = retry_after_ms;
        wire
    }
}

/// Parse `Retry-After` as milliseconds (seconds or HTTP date).
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())?;
    let trimmed = raw.trim();
    if let Ok(secs) = trimmed.parse::<i64>() {
        return Some(secs.saturating_mul(1000));
    }
    let Ok(parsed) = DateTime::parse_from_rfc2822(trimmed) else {
        return None;
    };
    let ms = parsed.with_timezone(&Utc).signed_duration_since(Utc::now());
    Some(ms.num_milliseconds().max(0))
}

fn classify_upstream(status: u16, body: &str) -> FailureClass {
    if let Some(class) = body_failure_class(body) {
        return class;
    }
    match status {
        401 | 403 => FailureClass::ProviderAuth,
        408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529 => FailureClass::ProviderTransient,
        402 => FailureClass::ProviderQuotaExhausted,
        400..=499 => FailureClass::ProviderRequestRejected,
        _ => FailureClass::ProviderTransient,
    }
}

const MAX_WIRE_PROVIDER_MSG: usize = 300;

/// Short text for the wire; uses a plain provider `error.message` under 300 characters when present.
pub fn upstream_wire_message(
    status: u16,
    class: FailureClass,
    provider: &str,
    body: &str,
) -> String {
    if let Some(msg) = plain_provider_message(body) {
        return msg;
    }
    match class {
        FailureClass::ProviderAuth => {
            format!("{provider}: upstream refused credentials (HTTP {status})")
        }
        FailureClass::ProviderQuotaExhausted => {
            format!("{provider}: upstream quota or billing exhausted (HTTP {status})")
        }
        FailureClass::ProviderRequestRejected => {
            format!("{provider}: upstream rejected the request (HTTP {status})")
        }
        FailureClass::ProviderTransient => {
            format!("{provider}: upstream error (HTTP {status})")
        }
        _ => format!("{provider}: upstream HTTP {status}"),
    }
}

fn plain_provider_message(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let msg = v
        .pointer("/error/message")
        .or_else(|| v.pointer("/error/error/message"))
        .and_then(|m| m.as_str())?;
    if msg.is_empty() || msg.len() > MAX_WIRE_PROVIDER_MSG || msg.contains('\n') || msg.contains('\r') {
        return None;
    }
    Some(msg.to_string())
}

fn body_failure_class(body: &str) -> Option<FailureClass> {
    let b = body.to_ascii_lowercase();
    if b.contains("insufficient_quota")
        || b.contains("billing_hard_limit")
        || b.contains("exceeded your current quota")
        || b.contains("\"billing\"") && b.contains("exhaust")
    {
        return Some(FailureClass::ProviderQuotaExhausted);
    }
    if b.contains("prompt too long")
        || b.contains("context length")
        || b.contains("context_length_exceeded")
        || b.contains("maximum context")
    {
        return Some(FailureClass::ProviderRequestRejected);
    }
    None
}

/// The wire's error block.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WireError {
    /// `fatal_session` | `retryable_gateway` | `runtime_fault`.
    pub class: &'static str,
    /// Machine-readable code (`gateway.*`).
    pub code: &'static str,
    /// Human text.
    pub message: String,
    /// Retry hint; never set by the agent route today.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<i64>,
    /// Always "gateway".
    pub boundary: &'static str,
    /// Whether the caller may retry.
    pub recoverable: bool,
    /// Why a pay-as-you-go call was refused (`gateway.policy_denied` from the source budgets only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Box<crate::spend::Denial>>,
}

impl WireError {
    fn new(class: &'static str, code: &'static str, message: &str, recoverable: bool) -> Self {
        Self {
            class,
            code,
            message: message.to_string(),
            retry_after_ms: None,
            boundary: "gateway",
            recoverable,
            budget: None,
        }
    }

    /// `fatal_session`, not recoverable.
    pub fn fatal(code: &'static str, message: &str) -> Self {
        Self::new("fatal_session", code, message, false)
    }

    /// `retryable_gateway`, recoverable.
    pub fn retryable(code: &'static str, message: &str) -> Self {
        Self::new("retryable_gateway", code, message, true)
    }

    /// `runtime_fault`, not recoverable.
    pub fn runtime(code: &'static str, message: &str) -> Self {
        Self::new("runtime_fault", code, message, false)
    }

    /// No HTTP connection slot within the accept wait (`gateway.busy`).
    pub fn busy(retry_after_ms: i64) -> Self {
        let mut e = Self::retryable(
            "gateway.busy",
            "gateway at HTTP connection capacity",
        );
        e.retry_after_ms = Some(retry_after_ms);
        e
    }

    /// A source-budget refusal: `gateway.policy_denied` carrying the denial.
    pub fn budget(d: Box<crate::spend::Denial>) -> Self {
        let wire_msg = if d.reason == "invalid" {
            crate::logf!("budget denied (invalid): {}", d.message);
            "pay-as-you-go budget configuration is invalid or unreadable"
        } else {
            d.message.as_str()
        };
        let mut e = Self::fatal("gateway.policy_denied", wire_msg);
        e.budget = Some(d);
        e
    }

    /// The same block with `recoverable` false (stream error chunks always say false).
    pub fn unrecoverable(mut self) -> Self {
        self.recoverable = false;
        self.retry_after_ms = None;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn mapping_follows_the_wire_table() {
        let w = CallError::status(503, "x".into()).to_wire();
        assert_eq!(
            (w.class, w.code, w.recoverable),
            ("retryable_gateway", "gateway.provider_transient", true)
        );
        let w = CallError::status(401, "x".into()).to_wire();
        assert_eq!(
            (w.class, w.code),
            ("fatal_session", "gateway.provider_auth")
        );
        let w = CallError::upstream(400, "bad".into(), None, "").to_wire();
        assert_eq!(
            (w.class, w.code, w.recoverable),
            ("fatal_session", "gateway.provider_request_rejected", false)
        );
        let w = CallError::upstream(
            429,
            "quota".into(),
            None,
            r#"{"error":{"type":"insufficient_quota"}}"#,
        )
        .to_wire();
        assert_eq!(w.code, "gateway.provider_quota_exhausted");
        let w = CallError::Routing {
            class: FailureClass::ProviderTransient,
            message: "x".into(),
        }
        .to_wire();
        assert_eq!(w.code, "gateway.transport_failed");
        let w = CallError::Routing {
            class: FailureClass::CapacityExhausted,
            message: "x".into(),
        }
        .to_wire();
        assert_eq!(w.code, "gateway.capacity_exhausted");
        assert_eq!(
            CallError::Transport(TransportDetail::local("x"))
                .to_wire()
                .class,
            "runtime_fault"
        );
        assert_eq!(
            CallError::Transport(TransportDetail::network("x"))
                .to_wire()
                .code,
            "gateway.provider_transient"
        );
        assert_eq!(
            CallError::ProviderFailed("x".into()).to_wire().code,
            "gateway.provider_failed"
        );
        let w = CallError::classified(FailureClass::StreamingUnsupported, "x").to_wire();
        assert_eq!(
            (w.class, w.code, w.recoverable),
            ("fatal_session", "gateway.streaming_unsupported", false)
        );
        let w = CallError::classified(FailureClass::PolicyDenied, "x").to_wire();
        assert_eq!(
            (w.class, w.code),
            ("fatal_session", "gateway.policy_denied")
        );
        let w = CallError::Routing {
            class: FailureClass::ProviderNotConfigured,
            message: "model \"gpt\" provider \"openai\" not registered".into(),
        }
        .to_wire();
        assert_eq!(
            (w.class, w.code, w.recoverable),
            ("fatal_session", "gateway.provider_not_configured", false)
        );
    }

    #[test]
    fn upstream_wire_uses_short_provider_message_or_fixed_text() {
        let short = r#"{"error":{"message":"upstream says 400"}}"#;
        let msg = upstream_wire_message(400, FailureClass::ProviderRequestRejected, "vllm", short);
        assert_eq!(msg, "upstream says 400");
        let long = format!(
            r#"{{"error":{{"message":"{}"}}}}"#,
            "x".repeat(400)
        );
        let msg = upstream_wire_message(500, FailureClass::ProviderTransient, "vllm", &long);
        assert_eq!(msg, "vllm: upstream error (HTTP 500)");
        assert!(!msg.contains('x'));
    }

    #[test]
    fn retry_after_seconds_and_preserves_on_wire() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(parse_retry_after(&h), Some(2000));
        let w = CallError::upstream(503, "x".into(), parse_retry_after(&h), "").to_wire();
        assert_eq!(w.retry_after_ms, Some(2000));
    }
}
