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
//! estimate when it reports none (`X-ContextStack-Cost-Estimated: true`); any other status counts
//! nothing. Once the request body was sent, a timeout, transport failure or caller disconnect settles
//! at the estimate, flagged the same way (the provider may have answered and billed); a failure before
//! the body was sent counts nothing. A charge the spend file could not record omits the cost header. Optional headers: `X-ContextStack-Max-Usd` (a ceiling that only
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
use crate::spend::Reservation;
use http_body_util::{BodyExt, Limited};
use hyper::{Request, Response, body::Incoming};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A question's hold. Settled explicitly on an answer; dropped (caller disconnect) or abandoned on an
/// error after the body was sent, it settles at the reservation; before that, it is released at $0.
struct QuestionHold {
    hold: Option<Reservation>,
    sent: Arc<AtomicBool>,
    request_id: String,
}

impl QuestionHold {
    fn settle(&mut self, charged: f64, over: bool) -> Result<f64, String> {
        self.hold
            .take()
            .map_or(Ok(0.0), |hold| hold.settle(charged, over))
    }

    /// The reservation, settled, when the body was sent; else release.
    fn settle_sent(&mut self) -> Option<Result<f64, String>> {
        if !self.sent.load(Ordering::Relaxed) {
            drop(self.hold.take());
            return None;
        }
        let reserved = self.hold.as_ref()?.reservation_usd();
        crate::log::row(&serde_json::json!({
            "event": "typed_question_settled_at_reservation", "source": SOURCE,
            "request_id": self.request_id, "charged_usd": reserved, "basis": "reservation_estimate",
        }));
        Some(self.settle(reserved, false))
    }
}

impl Drop for QuestionHold {
    fn drop(&mut self) {
        let _ = self.settle_sent();
    }
}

fn cost_headers(r: &mut Response<Body>, charged: &Result<f64, String>, estimated: bool) {
    let h = r.headers_mut();
    let mut pairs = vec![("x-contextstack-cost-class", "api".to_string())];
    if let Ok(usd) = charged {
        pairs.push(("x-contextstack-cost-usd", format!("{usd:.9}")));
        if estimated {
            pairs.push(("x-contextstack-cost-estimated", "true".to_string()));
        }
    }
    for (k, v) in pairs {
        if let Ok(v) = hyper::header::HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
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
    let (body, sent) = crate::trace::tracked_body(body);
    let mut hold = QuestionHold {
        hold: Some(hold),
        sent,
        request_id: request_id.clone(),
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let answer = match gw
        .typed_question(&snap, SOURCE, &model, &request_id, body, deadline)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            let w = e.to_wire();
            let mut r = refuse(status_of(&e, &w), &w);
            if let Some(charged) = hold.settle_sent() {
                cost_headers(&mut r, &charged, true);
            }
            return r;
        }
    };
    let ok = (200..300).contains(&answer.status);
    let (charged, estimated) = if ok {
        match answer.input_tokens {
            Some(in_t) => (
                usd_tokens(rates, in_t, answer.output_tokens.unwrap_or(0)),
                false,
            ),
            None => (est, true),
        }
    } else {
        (0.0, false)
    };
    let over = charged > est + 1e-9;
    let recorded = hold.settle(charged, over);
    crate::log::line(&format!(
        "USAGE provider={SOURCE} model={model} in={} out={} cost={charged:.9} class=api req={request_id} rev={} status={}",
        answer.input_tokens.unwrap_or(0),
        answer.output_tokens.unwrap_or(0),
        snap.revision,
        answer.status,
    ));
    let mut r = full(answer.status, &answer.content_type, answer.body);
    cost_headers(&mut r, &recorded, estimated);
    if let Ok(v) = hyper::header::HeaderValue::from_str(&answer.latency_ms.to_string()) {
        r.headers_mut().insert("x-contextstack-latency-ms", v);
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

    fn question(
        sent: bool,
    ) -> Result<
        (tempfile::TempDir, Arc<crate::spend::Guard>, QuestionHold),
        Box<dyn std::error::Error>,
    > {
        let home = tempfile::tempdir()?;
        let guard = Arc::new(crate::spend::Guard::new(Some(home.path())));
        guard
            .replace(br#"{"api_usd_per_day": 1}"#)
            .map_err(|e| format!("{e:?}"))?;
        let hold = QuestionHold {
            hold: Some(guard.admit(SOURCE, "", 0.1)?),
            sent: Arc::new(AtomicBool::new(sent)),
            request_id: "q".into(),
        };
        Ok((home, guard, hold))
    }

    fn spent(guard: &crate::spend::Guard) -> serde_json::Value {
        guard.report([SOURCE.to_owned()])["total"].clone()
    }

    // G5: a caller that disconnects after the question was sent still pays the reservation.
    #[test]
    fn a_disconnect_after_sending_settles_the_reservation() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_home, guard, hold) = question(true)?;
        drop(hold);
        assert_eq!(spent(&guard)["spent_usd"], 0.1);
        assert_eq!(spent(&guard)["held_usd"], 0.0);
        Ok(())
    }

    // G5: a timeout after sending settles the reservation, flagged estimated.
    #[test]
    fn a_timeout_after_sending_settles_the_reservation() -> Result<(), Box<dyn std::error::Error>> {
        let (_home, guard, mut hold) = question(true)?;
        let charged = hold.settle_sent().ok_or("no settlement")?;
        assert_eq!(charged, Ok(0.1));
        let mut r = full(504, "application/json", "{}");
        cost_headers(&mut r, &charged, true);
        assert_eq!(r.headers()["x-contextstack-cost-estimated"], "true");
        assert_eq!(r.headers()["x-contextstack-cost-usd"], "0.100000000");
        drop(hold);
        assert_eq!(spent(&guard)["spent_usd"], 0.1, "settled once");
        Ok(())
    }

    #[test]
    fn a_failure_before_sending_releases() -> Result<(), Box<dyn std::error::Error>> {
        let (_home, guard, mut hold) = question(false)?;
        assert!(hold.settle_sent().is_none());
        drop(hold);
        assert_eq!(spent(&guard)["spent_usd"], 0.0);
        assert_eq!(spent(&guard)["held_usd"], 0.0);
        Ok(())
    }
}
