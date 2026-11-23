//! `POST /v1/ai/typed-question`: `TypeSafe` (System One "decision models" such as Jev) as a keyed,
//! pay-as-you-go source. It answers typed questions with calibrated probabilities and is not a chat
//! model, so it is not routed through `model.call`: the body is `TypeSafe`'s own request
//! (`{"model", "state", "questions", ...}`, docs.typesafe.ai/api), forwarded unchanged to the catalog
//! endpoint `typesafe` (`{endpoint}/systemone`), and the reply is `TypeSafe`'s status and body unchanged.
//!
//! The gateway adds what every keyed source gets: the stored key (`csgateway-rs credentials set
//! typesafe`; callers never hold it), egress policy, admission (models bound by the `typesafe`
//! deployment), and the `typesafe` source budget in limits.json. Admission is on an estimate (body
//! bytes / 4 input tokens plus the default output ceiling at the catalog rate) before any connection;
//! a 2xx answer settles at (input + output) tokens × rate (same rate per 1k for both), or at the
//! estimate when it reports none; any
//! other status counts nothing. Optional headers: `X-ContextStack-Max-Usd` (a ceiling that only
//! lowers), `X-ContextStack-Request-Id`, `X-ContextStack-Timeout-Ms` (default 60 s). The answer carries
//! `X-ContextStack-Cost-Usd` and `X-ContextStack-Cost-Class: api`.
//!
//! A refusal before `TypeSafe` answers is `{"error": <WIRE error>}`: 400 not a `TypeSafe` request, 413
//! over 32 MiB, 402 budget (`gateway.policy_denied` with `error.budget`), 403 egress policy, 429
//! admission, 503 no credential or no endpoint, 502 transport, 504 deadline.

use super::{Body, full, json};
use crate::errors::{CallError, WireError};
use crate::gateway::Gateway;
use crate::pricing::{ResolvedRates, TokenBill, settle_usd};
use http_body_util::{BodyExt, Limited};
use hyper::{Request, Response, body::Incoming};
use std::sync::Arc;
use std::time::Duration;

/// The catalog endpoint and budget source.
pub const SOURCE: &str = "typesafe";
/// `TypeSafe`'s own request limit.
const MAX_BODY: usize = 32 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

fn refuse(status: u16, e: &WireError) -> Response<Body> {
    json(status, &serde_json::json!({ "error": e }))
}

fn header<'a>(req: &'a Request<Incoming>, name: &str) -> &'a str {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
}

fn typesafe_rates(blend_per_1k: f64) -> ResolvedRates {
    ResolvedRates {
        input_per_1k: blend_per_1k,
        output_per_1k: blend_per_1k,
        cache_read_per_1k: blend_per_1k,
        cache_write_per_1k: blend_per_1k,
    }
}

/// List-price USD for input at the input rate and output at the output rate.
fn usd_tokens(rates: ResolvedRates, input: i64, output: i64) -> f64 {
    settle_usd(
        rates,
        TokenBill {
            input_tokens: input,
            output_tokens: output,
            ..TokenBill::default()
        },
    )
}

/// The status a call error answers with.
fn status_of(e: &CallError, w: &WireError) -> u16 {
    match w.code {
        "gateway.policy_denied" => 403,
        "gateway.capacity_exhausted" => 429,
        "gateway.provider_auth" => 503,
        _ if matches!(
            e,
            CallError::Transport(m) if m.message.ends_with("context deadline exceeded")
        ) =>
        {
            504
        }
        _ => 502,
    }
}

/// POST /v1/ai/typed-question.
pub async fn handle(gw: &Arc<Gateway>, req: Request<Incoming>) -> Response<Body> {
    let request_id = header(&req, "x-contextstack-request-id").to_string();
    let max_usd = header(&req, "x-contextstack-max-usd").to_string();
    let timeout = header(&req, "x-contextstack-timeout-ms")
        .parse::<u64>()
        .ok()
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
    let Ok(body) = Limited::new(req.into_body(), MAX_BODY).collect().await else {
        let e = WireError::fatal(
            "gateway.bad_request",
            "typed question over 32 MiB or unreadable",
        );
        return refuse(413, &e);
    };
    let body = body.to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let model = parsed["model"].as_str().unwrap_or("").to_string();
    if model.is_empty() || !parsed["questions"].is_object() {
        let e = WireError::fatal(
            "gateway.bad_request",
            "want a TypeSafe request: {\"model\", \"state\", \"questions\": {...}}",
        );
        return refuse(400, &e);
    }
    let snap = gw.bind(&request_id);
    let Some(entry) = snap.candidate.entry(SOURCE) else {
        let e = WireError::fatal(
            "gateway.provider_unsupported",
            "no typesafe endpoint in the catalog",
        );
        return refuse(503, &e);
    };
    if entry.requires_key && !gw.settings.store_backed(SOURCE) {
        let e = WireError::fatal(
            "gateway.no_provider_credential",
            "provider credential not configured on the gateway",
        );
        return refuse(503, &e);
    }
    let rates = typesafe_rates(entry.default_cost_per_1k);
    let est = crate::spend::estimate(rates, body.len(), 0);
    let hold = match gw.spend.admit_async(SOURCE, &max_usd, est).await {
        Ok(h) => h,
        Err(d) => return refuse(402, &WireError::budget(d)),
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let answer = match gw
        .typed_question(&snap, SOURCE, &model, &request_id, body, deadline)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            let w = e.to_wire();
            return refuse(status_of(&e, &w), &w);
        }
    };
    let ok = (200..300).contains(&answer.status);
    let charged = if ok {
        match answer.input_tokens {
            Some(in_t) => usd_tokens(rates, in_t, answer.output_tokens.unwrap_or(0)),
            None => est,
        }
    } else {
        0.0
    };
    let over = charged > hold.reservation_usd() + 1e-9;
    let _ = tokio::task::spawn_blocking(move || hold.settle(charged, over)).await;
    crate::log::line(&format!(
        "USAGE provider={SOURCE} model={model} in={} out={} cost={charged:.9} class=api req={request_id} rev={} status={}",
        answer.input_tokens.unwrap_or(0),
        answer.output_tokens.unwrap_or(0),
        snap.revision,
        answer.status,
    ));
    let mut r = full(answer.status, &answer.content_type, answer.body);
    let h = r.headers_mut();
    for (k, v) in [
        ("x-contextstack-cost-usd", format!("{charged:.9}")),
        ("x-contextstack-cost-class", "api".to_string()),
        ("x-contextstack-latency-ms", answer.latency_ms.to_string()),
    ] {
        if let Ok(v) = hyper::header::HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priced_per_category_token() {
        let rates = typesafe_rates(0.000_042);
        assert!((usd_tokens(rates, 1_000_000, 0) - 0.042).abs() < 1e-12);
        assert!((usd_tokens(rates, 120, 3) - 0.000_005_166).abs() < 1e-12);
        assert!(usd_tokens(rates, 0, 0).abs() < f64::EPSILON);
    }
}
