//! The caller contract (WIRE.md): `agent.gateway.model_call.v2` and its byte-identical predecessor
//! `cs.gateway.model_call.v1`, their responses, SSE chunks and the cost rule.

use crate::errors::WireError;
use crate::pricing::{ResolvedRates, TokenBill, amount_string, settle_usd, wallet_usd};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// The legacy request schema.
pub const REQUEST_V1: &str = "cs.gateway.model_call.v1";
/// The current request schema.
pub const REQUEST_V2: &str = "agent.gateway.model_call.v2";
/// Chunk family for an accepted v1 request.
pub const CHUNK_V1: &str = "cs.gateway.model_call.chunk.v1";
/// Chunk family for an accepted v2 request (and anything unaccepted).
pub const CHUNK_V2: &str = "agent.gateway.model_call.chunk.v2";

/// The accepted request schema echoed, or `None` for an unknown one.
pub fn accepted_schema(v: &str) -> Option<&'static str> {
    match v {
        REQUEST_V1 => Some(REQUEST_V1),
        REQUEST_V2 => Some(REQUEST_V2),
        _ => None,
    }
}

/// The chunk family matching an accepted request schema.
pub fn chunk_family(schema: &str) -> &'static str {
    if schema == REQUEST_V1 {
        CHUNK_V1
    } else {
        CHUNK_V2
    }
}

/// `null` decodes as the default (Go leaves the zero value in place).
pub fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    d: D,
) -> Result<T, D::Error> {
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

/// A model call request. Unknown fields are ignored (tolerant decoder).
#[derive(Debug, Default, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "the field names are the wire contract"
)]
pub struct Request {
    /// The discriminator, checked.
    #[serde(default, deserialize_with = "nullable")]
    pub schema_version: String,
    /// Echoed.
    #[serde(default, deserialize_with = "nullable")]
    pub request_id: String,
    /// Optional run trace identity.
    pub job_id: Option<String>,
    /// Trace ID for the run.
    pub run_id: Option<String>,
    /// Turn number. Legacy callers have also sent a string.
    pub step_id: Option<serde_json::Value>,
    /// Send number within the model call.
    pub send_index: Option<u64>,
    /// Attribution fallback for `wire_request` rows.
    #[serde(default, deserialize_with = "nullable")]
    pub role: Role,
    /// Routing input.
    #[serde(default, deserialize_with = "nullable")]
    pub capability: Capability,
    /// The conversation.
    #[serde(default, deserialize_with = "nullable")]
    pub messages: Vec<Message>,
    /// Tool declarations.
    #[serde(default, deserialize_with = "nullable")]
    pub tool_schema: Vec<Box<RawValue>>,
    /// Optional OpenAI-compatible tool selection.
    pub tool_choice: Option<serde_json::Value>,
    /// Deadline and sampling.
    #[serde(default, deserialize_with = "nullable")]
    pub budget: Budget,
    /// SSE when true.
    #[serde(default, deserialize_with = "nullable")]
    pub stream: bool,
}

/// `role` (only the pack name is read).
#[derive(Debug, Default, Deserialize)]
pub struct Role {
    /// Caller attribution fallback.
    #[serde(default, deserialize_with = "nullable")]
    pub role_pack_name: String,
}

/// `capability`.
#[derive(Debug, Default, Deserialize)]
pub struct Capability {
    /// The model name, or empty for free-local.
    #[serde(default, deserialize_with = "nullable")]
    pub model_tier_hint: String,
}

/// `budget`: `max_input_tokens` is accepted (its type is checked) and not enforced; `max_usd` is an api
/// call's ceiling under its source's budget (`crate::spend`): it only lowers, never raises.
#[derive(Debug, Default, Deserialize)]
#[expect(
    dead_code,
    reason = "max_input_tokens is decoded for shape, never enforced (WIRE Parked)"
)]
pub struct Budget {
    /// HTTP deadline (default 60 s).
    #[serde(default, deserialize_with = "nullable")]
    pub timeout_ms: i64,
    /// Accepted, unused.
    #[serde(default, deserialize_with = "nullable")]
    pub max_input_tokens: i64,
    /// Sampling limit (0: 4096).
    #[serde(default, deserialize_with = "nullable")]
    pub max_output_tokens: i64,
    /// A decimal string: the most an api call may spend (a job's amount).
    #[serde(default, deserialize_with = "nullable")]
    pub max_usd: String,
}

/// One message. Content is a string or a block list, kept as JSON.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    /// user | assistant | tool | system.
    #[serde(default, deserialize_with = "nullable")]
    pub role: String,
    /// String, block list, or null when absent.
    #[serde(default)]
    pub content: serde_json::Value,
    /// Assistant tool calls.
    #[serde(default, deserialize_with = "nullable")]
    pub tool_calls: Vec<ToolCall>,
    /// Pairs a tool result with its call.
    #[serde(default, deserialize_with = "nullable")]
    pub tool_call_id: String,
    /// Tool result failed.
    #[serde(default, deserialize_with = "nullable")]
    pub is_error: bool,
}

impl Message {
    /// The content as text when it is a string.
    pub fn text(&self) -> Option<&str> {
        self.content.as_str()
    }
}

/// A tool call in history; `args` is the caller's exact JSON text.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    /// Call id.
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    /// Tool name.
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    /// Arguments, verbatim (absent when missing).
    #[serde(default)]
    pub args: Option<Box<RawValue>>,
}

/// A tool call in a reply.
#[derive(Debug, Clone, Serialize)]
pub struct ReplyToolCall {
    /// Call id (omitted when empty).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments object.
    pub args: Box<RawValue>,
    /// The provider returned malformed native arguments.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub invalid_arguments: bool,
}

/// `usage`.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
}

/// `cost`.
#[derive(Debug, Clone, Serialize)]
pub struct Cost {
    /// Always USD.
    pub currency: &'static str,
    /// Six-place decimal string.
    pub amount: String,
    /// `free_local` | subscription | api (omitted on errors except a settled partial stream).
    #[serde(skip_serializing_if = "str::is_empty")]
    pub class: String,
    /// Provider-reported charge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charged: Option<f64>,
    /// `amount` as a number (ok replies and settled partial streams).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_usd: Option<f64>,
    /// What the caller's wallet is charged (ok replies and settled partial streams).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charged_usd: Option<f64>,
    /// True when settlement exceeded the pre-flight reservation (additive).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub over_reservation: bool,
}

impl Cost {
    /// The $0 block every error without a settled partial stream carries.
    pub fn zero() -> Self {
        Self {
            currency: "USD",
            amount: "0.000000".into(),
            class: String::new(),
            charged: None,
            est_usd: None,
            charged_usd: None,
            over_reservation: false,
        }
    }

    /// A positive, finite charge reported by the provider.
    pub fn reported_charge(&self) -> Option<f64> {
        self.charged
            .filter(|charge| charge.is_finite() && *charge > 0.0)
    }

    /// The cost rule: `est_usd` is amount as a number; `charged_usd` is the provider's reported charge when
    /// known, else amount for class api, else 0.
    pub fn priced(
        amount: String,
        class: &str,
        charged: Option<f64>,
        over_reservation: bool,
    ) -> Self {
        let est: f64 = amount.parse().unwrap_or(0.0);
        let charged_usd = Some(wallet_usd(class, est, charged));
        Self {
            currency: "USD",
            amount,
            class: class.to_string(),
            charged,
            est_usd: Some(est),
            charged_usd,
            over_reservation,
        }
    }
}

/// Build cost from usage, reservation, and optional provider charge.
pub fn price_call(
    rates: ResolvedRates,
    bill: TokenBill,
    class: &str,
    charged: Option<f64>,
    reservation_usd: Option<f64>,
) -> Cost {
    let amount = amount_string(rates, bill);
    let est = settle_usd(rates, bill);
    let settlement = charged
        .filter(|c| c.is_finite() && *c >= 0.0)
        .unwrap_or(est);
    let over = reservation_usd.is_some_and(|r| settlement > r + 1e-9);
    Cost::priced(amount, class, charged, over)
}

/// `message` in a reply.
#[derive(Debug, Clone, Serialize)]
pub struct ReplyMessage {
    /// Always assistant.
    pub role: &'static str,
    /// Text.
    pub content: String,
    /// Stop reason.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stop_reason: String,
}

/// The unary envelope, ok or error.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    /// Echoed accepted schema.
    pub schema_version: &'static str,
    /// Echoed.
    pub request_id: String,
    /// ok | error.
    pub status: &'static str,
    /// Provider id (ok only).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// The resolved model name (ok only).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// The reply message (ok only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<ReplyMessage>,
    /// Structured tool calls (ok, when any).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ReplyToolCall>,
    /// Tokens.
    pub usage: Usage,
    /// Cost.
    pub cost: Cost,
    /// Upstream latency (omitted when 0).
    #[serde(skip_serializing_if = "is_zero")]
    pub latency_ms: i64,
    /// Error block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(v: &i64) -> bool {
    *v == 0
}

impl Response {
    /// An envelope error: $0, zero usage, no partial result.
    pub fn error(schema: &'static str, request_id: &str, error: WireError) -> Self {
        Self {
            schema_version: schema,
            request_id: request_id.to_string(),
            status: "error",
            provider: String::new(),
            model: String::new(),
            message: None,
            tool_calls: Vec::new(),
            usage: Usage::default(),
            cost: Cost::zero(),
            latency_ms: 0,
            error: Some(error),
        }
    }
}

/// One SSE chunk.
#[derive(Debug, Clone, Serialize)]
pub struct Chunk {
    /// Chunk family.
    pub schema_version: &'static str,
    /// Echoed.
    pub request_id: String,
    /// Position among delta and stop chunks (an error chunk says 0).
    pub index: i64,
    /// delta | stop | error.
    pub kind: &'static str,
    /// Delta text.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// Stop reason.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stop_reason: String,
    /// Stop: the provider id, as the unary envelope names it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Stop: the resolved model name.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Stop: structured tool calls, as the unary envelope carries them (when any).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ReplyToolCall>,
    /// Stop usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Stop or error cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Error block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl Chunk {
    /// A delta chunk.
    pub fn delta(family: &'static str, request_id: &str, index: i64, content: String) -> Self {
        Self {
            schema_version: family,
            request_id: request_id.to_string(),
            index,
            kind: "delta",
            content,
            stop_reason: String::new(),
            provider: String::new(),
            model: String::new(),
            tool_calls: Vec::new(),
            usage: None,
            cost: None,
            error: None,
        }
    }

    /// The terminal error chunk: index 0, $0 unless replaced after partial settlement, never recoverable.
    pub fn error(family: &'static str, request_id: &str, error: WireError) -> Self {
        Self {
            schema_version: family,
            request_id: request_id.to_string(),
            index: 0,
            kind: "error",
            content: String::new(),
            stop_reason: String::new(),
            provider: String::new(),
            model: String::new(),
            tool_calls: Vec::new(),
            usage: None,
            cost: Some(Cost::zero()),
            error: Some(error.unrecoverable()),
        }
    }

    /// The SSE frame.
    pub fn frame(&self) -> String {
        format!(
            "data: {}\n\n",
            serde_json::to_string(self).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn retired_product_schema_is_rejected() {
        let retired = ["u", "ah.gateway.model_call.v1"].concat();
        assert_eq!(super::accepted_schema(&retired), None);
        assert_eq!(
            super::accepted_schema("cs.gateway.model_call.v1"),
            Some(super::REQUEST_V1)
        );
    }

    use super::*;

    #[test]
    fn amounts_and_cost_rule() {
        let rates = ResolvedRates {
            input_per_1k: 0.01,
            output_per_1k: 0.01,
            cache_read_per_1k: 0.01,
            cache_write_per_1k: 0.01,
        };
        let bill = TokenBill {
            input_tokens: 1200,
            output_tokens: 34,
            ..TokenBill::default()
        };
        assert_eq!(amount_string(rates, bill), "0.012340");
        assert_eq!(
            amount_string(
                rates,
                TokenBill {
                    input_tokens: 1,
                    ..TokenBill::default()
                }
            ),
            "0.000010"
        );
        assert_eq!(
            amount_string(
                ResolvedRates {
                    input_per_1k: 0.0,
                    output_per_1k: 0.0,
                    cache_read_per_1k: 0.0,
                    cache_write_per_1k: 0.0,
                },
                TokenBill {
                    input_tokens: 1000,
                    output_tokens: 1000,
                    ..TokenBill::default()
                }
            ),
            "0.000000"
        );
        let c = Cost::priced("0.020000".into(), "subscription", None, false);
        assert_eq!((c.est_usd, c.charged_usd), (Some(0.02), Some(0.0)));
        let c = Cost::priced("0.002000".into(), "api", Some(0.5), false);
        assert_eq!(c.charged_usd, Some(0.5));
    }

    #[test]
    fn claude_spec_blend_settlement_over_reservation() {
        // anthropic-unary-response: claude-spec blended 0.015/1k, usage 100/50, admission held 64 out.
        let rates = ResolvedRates {
            input_per_1k: 0.015,
            output_per_1k: 0.015,
            cache_read_per_1k: 0.015,
            cache_write_per_1k: 0.015,
        };
        let bill = TokenBill {
            input_tokens: 100,
            output_tokens: 50,
            ..TokenBill::default()
        };
        let cost = price_call(rates, bill, "api", None, Some(0.00207));
        assert_eq!(cost.amount, "0.002250");
        assert_eq!(cost.est_usd, Some(0.00225));
        assert_eq!(cost.charged_usd, Some(0.00225));
        assert!(cost.over_reservation);
    }
}
