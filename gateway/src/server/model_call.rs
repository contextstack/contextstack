//! `POST /v1/agent/model.call` (and its `/v1/cs/model.call` alias). Contract-shape failures are
//! envelope errors with HTTP 200, never 4xx. A stream request answers SSE once its schema is accepted;
//! every stream ends in exactly one terminal chunk, stop or error.

use super::{Body, full, json};
use crate::errors::{CallError, WireError};
use crate::gateway::{Gateway, OpenStream};
use crate::inventory::{FREE_LOCAL, Inventory};
use crate::log;
use crate::pricing::{ResolvedRates, TokenBill, settle_usd};
use crate::protocol::{Call, Event, MeteredUsage, Tool, history};
use crate::spend::Reservation;
use crate::wire::{self, Chunk, Cost, ReplyMessage, Request, Response as Envelope, Usage};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::Response;
use hyper::body::Frame;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_UPSTREAM_RETRIES: u32 = 2;
const RETRY_AFTER_CAP_MS: i64 = 20_000;

/// The resolved target of a call.
#[derive(Clone)]
struct Target {
    name: String,
    provider: String,
    rates: ResolvedRates,
    class: String,
}

fn token_bill(input: i64, output: i64, cache_read: i64, cache_write: i64) -> TokenBill {
    TokenBill {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

/// A body that never decoded: `bad_request` under the v2 family.
pub fn undecodable(err: &str) -> Response<Body> {
    json(
        200,
        &Envelope::error(
            wire::REQUEST_V2,
            "",
            WireError::fatal(
                "gateway.bad_request",
                &format!("request body decode failed: {err}"),
            ),
        ),
    )
}

fn sse(frame: String) -> Response<Body> {
    let mut r = full(200, "text/event-stream", frame);
    sse_headers(&mut r);
    r
}

fn sse_headers(r: &mut Response<Body>) {
    let h = r.headers_mut();
    h.insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-cache"),
    );
    h.insert(
        hyper::header::CONNECTION,
        hyper::header::HeaderValue::from_static("keep-alive"),
    );
}

/// Refuse in the request's shape: an SSE error chunk for a stream, else an envelope.
fn refuse(stream: bool, schema: &'static str, request_id: &str, e: WireError) -> Response<Body> {
    if stream {
        return sse(Chunk::error(wire::chunk_family(schema), request_id, e).frame());
    }
    json(200, &Envelope::error(schema, request_id, e))
}

/// Resolve `capability.model_tier_hint` through the inventory. Unknown names refuse (never fall
/// through); CLI entries refuse; an empty hint is `local-coder`, else the lowest-tier `free_local` model.
fn resolve(inv: &Inventory, hint: &str) -> Result<Target, WireError> {
    let target = |e: &crate::inventory::ModelEntry| Target {
        name: e.name.clone(),
        provider: e.provider.clone(),
        rates: e.resolved_rates(),
        class: e.cost_class.clone(),
    };
    if hint.is_empty() {
        if let Some(e) = inv
            .by_name("local-coder")
            .filter(|e| e.available && !e.is_cli())
        {
            return Ok(target(e));
        }
        for tier in 1..=3 {
            if let Some(e) = inv
                .for_tier(tier)
                .into_iter()
                .find(|e| e.cost_class == FREE_LOCAL)
            {
                return Ok(target(e));
            }
        }
        return Err(WireError::fatal(
            "gateway.api_explicit_only",
            "unnamed model_tier_hint has no free_local model; API requires an explicit name",
        ));
    }
    match inv.by_name(hint) {
        None => Err(WireError::fatal(
            "gateway.unknown_model",
            &format!(
                "unknown model_tier_hint {hint} — not in inventory; refuse, never fall through"
            ),
        )),
        Some(e) if e.is_cli() => Err(WireError::fatal(
            "gateway.provider_unsupported",
            "cs model.call has no cli executor path; inventory entry is cli",
        )),
        Some(e) => Ok(target(e)),
    }
}

/// A `tool_schema[]` entry.
#[derive(serde::Deserialize)]
struct Decl {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    schema: Option<Box<serde_json::value::RawValue>>,
}

/// The adapter request: system messages hoisted and joined, images validated, tools read.
fn invocation(gw: &Gateway, req: &Request, t: &Target) -> Result<Call, WireError> {
    let mut system = Vec::new();
    let mut messages = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        if m.role == "system" {
            if let Some(s) = m.text().filter(|s| !s.is_empty()) {
                system.push(s.to_string());
            }
            continue;
        }
        messages.push(m.clone());
    }
    history::validate_images(&messages)
        .map_err(|e| WireError::fatal("gateway.invalid_image_block", &e))?;
    let tools = req
        .tool_schema
        .iter()
        .filter_map(|raw| serde_json::from_str::<Decl>(raw.get()).ok())
        .filter(|d| !d.name.is_empty())
        .map(|d| Tool {
            name: d.name,
            description: d.description,
            schema: d.schema.filter(|s| s.get() != "null"),
        })
        .collect();
    let needs_key = crate::catalog::embedded_entry(&t.provider).is_some_and(|e| e.requires_key);
    if !t.provider.is_empty() && needs_key && !gw.settings.store_backed(&t.provider) {
        return Err(WireError::fatal(
            "gateway.no_provider_credential",
            "provider credential not configured on the gateway",
        ));
    }
    Ok(Call {
        model: t.name.clone(),
        system: system.join("\n\n"),
        messages,
        tools,
        max_tokens: req.budget.max_output_tokens,
        request_id: req.request_id.clone(),
    })
}

/// Accept the schema and count the request: the accepted schema, or the envelope refusal.
fn accept(
    gw: &Gateway,
    path: &str,
    caller: &str,
    req: &Request,
) -> Result<&'static str, WireError> {
    let Some(schema) = wire::accepted_schema(&req.schema_version) else {
        let msg = format!(
            "expected schema_version {:?} or {:?}, got {:?}",
            wire::REQUEST_V1,
            wire::REQUEST_V2,
            req.schema_version
        );
        return Err(WireError::fatal("gateway.schema_mismatch", &msg));
    };
    // Counted once, after acceptance and before any other validation (WIRE Migration).
    let caller = match (caller, req.role.role_pack_name.as_str()) {
        ("", "") => "unknown",
        ("", pack) => pack,
        (header, _) => header,
    };
    if let Ok(mut w) = gw.wire.lock() {
        w.record(schema, caller);
    }
    crate::log::row(&serde_json::json!({
        "event": "wire_request", "schema_version": schema, "caller": caller, "path": path, "request_id": req.request_id,
    }));
    Ok(schema)
}

/// Debug logging for agent calls: counts, roles and a payload digest only.
#[allow(clippy::redundant_closure_for_method_calls)]
fn agent_debug_row(messages: &serde_json::Value, body: &[u8]) -> serde_json::Value {
    let roles: Vec<&str> = messages
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let count = messages.as_array().map_or(0, |m| m.len());
    serde_json::json!({
        "event": "agent_debug_messages",
        "message_count": count,
        "roles": roles,
        "payload_sha256": crate::log::sha256_hex(body),
        "payload_bytes": body.len(),
    })
}

/// Handle one call.
pub async fn handle(gw: &Arc<Gateway>, path: &str, caller: &str, body: &Bytes) -> Response<Body> {
    let req: Request = match crate::protocol::openai::first_value(body) {
        Ok(r) => r,
        Err(e) => return undecodable(&e),
    };
    let schema = match accept(gw, path, caller, &req) {
        Ok(s) => s,
        Err(e) => return json(200, &Envelope::error(wire::REQUEST_V2, &req.request_id, e)),
    };
    if req.messages.is_empty() {
        let e = WireError::fatal(
            "gateway.empty_messages",
            "messages array must contain at least one entry",
        );
        return refuse(req.stream, schema, &req.request_id, e);
    }
    let snap = gw.bind(&req.request_id);
    let target = match resolve(&snap.inventory(), &req.capability.model_tier_hint) {
        Ok(t) => t,
        Err(e) => return refuse(req.stream, schema, &req.request_id, e),
    };
    if gw.settings.env.get("CSGATEWAY_AGENT_DEBUG", "") == "1" {
        let raw: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        crate::log::row(&agent_debug_row(&raw["messages"], body));
    }
    let call = match invocation(gw, &req, &target) {
        Ok(c) => c,
        Err(e) => return refuse(req.stream, schema, &req.request_id, e),
    };
    // The source's budget decides before any connection (crate::spend); other classes are never counted.
    let hold = if target.class == "api" {
        let est = crate::spend::estimate(target.rates, body.len(), req.budget.max_output_tokens);
        match gw.spend.admit(&target.provider, &req.budget.max_usd, est) {
            Ok(h) => Some(h),
            Err(d) => return refuse(req.stream, schema, &req.request_id, WireError::budget(d)),
        }
    } else {
        None
    };
    let timeout = u64::try_from(req.budget.timeout_ms)
        .ok()
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
    let deadline = tokio::time::Instant::now() + timeout;
    if req.stream {
        let family = wire::chunk_family(schema);
        let est_input = i64::try_from(body.len().div_ceil(4)).unwrap_or(i64::MAX);
        return match stream_with_retry(gw, &snap, &target.provider, &call, deadline).await {
            Ok(s) => stream_response(
                s,
                family,
                req.request_id,
                target,
                snap.revision.clone(),
                hold,
                est_input,
            ),
            Err(e) => sse(Chunk::error(family, &req.request_id, e.to_wire()).frame()),
        };
    }
    match invoke_with_retry(
        gw,
        &snap,
        &target.provider,
        &call,
        deadline,
        &req.request_id,
    )
    .await
    {
        Ok(reply) => ok(schema, req.request_id, &target, &snap.revision, reply, hold),
        Err(e) => json(200, &Envelope::error(schema, &req.request_id, e.to_wire())),
    }
}

fn retry_backoff_ms(attempt: u32, retry_after_ms: Option<i64>) -> u64 {
    if let Some(ms) = retry_after_ms {
        return u64::try_from(ms.clamp(0, RETRY_AFTER_CAP_MS)).unwrap_or(0);
    }
    let base = 250u64.saturating_mul(1 << attempt.min(4));
    let jitter = u64::from(attempt).wrapping_mul(97).wrapping_add(13) % 200;
    base + jitter
}

async fn sleep_for_retry(deadline: Instant, wait_ms: u64) {
    if wait_ms == 0 {
        return;
    }
    let left = deadline.saturating_duration_since(Instant::now());
    let wait = Duration::from_millis(wait_ms).min(left);
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

async fn invoke_with_retry(
    gw: &Arc<Gateway>,
    snap: &crate::gateway::Snapshot,
    provider: &str,
    call: &Call,
    deadline: Instant,
    request_id: &str,
) -> Result<crate::protocol::Reply, CallError> {
    let mut attempt = 0u32;
    loop {
        match gw.invoke(snap, provider, call, deadline).await {
            Ok(reply) => return Ok(reply),
            Err(e) if e.gateway_retryable() && attempt < MAX_UPSTREAM_RETRIES => {
                let wait_ms = retry_backoff_ms(attempt, e.retry_after_ms());
                crate::log::row(&serde_json::json!({
                    "event": "gateway_upstream_retry",
                    "request_id": request_id,
                    "provider": provider,
                    "attempt": attempt + 1,
                    "wait_ms": wait_ms,
                    "code": e.to_wire().code,
                }));
                attempt += 1;
                sleep_for_retry(deadline, wait_ms).await;
                if Instant::now() >= deadline {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

async fn stream_with_retry(
    gw: &Arc<Gateway>,
    snap: &crate::gateway::Snapshot,
    provider: &str,
    call: &Call,
    deadline: Instant,
) -> Result<OpenStream, CallError> {
    let mut attempt = 0u32;
    loop {
        match gw.stream(snap, provider, call, deadline).await {
            Ok(stream) => return Ok(stream),
            Err(e) if e.gateway_retryable() && attempt < MAX_UPSTREAM_RETRIES => {
                let wait_ms = retry_backoff_ms(attempt, e.retry_after_ms());
                crate::log::row(&serde_json::json!({
                    "event": "gateway_upstream_retry",
                    "request_id": call.request_id,
                    "provider": provider,
                    "attempt": attempt + 1,
                    "wait_ms": wait_ms,
                    "stream": true,
                    "code": e.to_wire().code,
                }));
                attempt += 1;
                sleep_for_retry(deadline, wait_ms).await;
                if Instant::now() >= deadline {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// The spend line; `rev` ties it to the catalog revision that priced the call.
fn usage_line(
    (provider, input, output): (&str, i64, i64),
    target: &Target,
    cost: &str,
    request_id: &str,
    revision: &str,
    partial: bool,
) -> String {
    let flag = if partial { " partial=1" } else { "" };
    format!(
        "USAGE provider={provider} model={} in={input} out={output} cost={cost} class={} req={request_id} rev={revision}{flag}",
        target.name, target.class
    )
}

/// The ok envelope, priced by the resolved entry's rate and class.
fn ok(
    schema: &'static str,
    request_id: String,
    target: &Target,
    revision: &str,
    reply: crate::protocol::Reply,
    hold: Option<Reservation>,
) -> Response<Body> {
    let bill = token_bill(
        reply.input_tokens,
        reply.output_tokens,
        reply.cache_read_tokens,
        reply.cache_write_tokens,
    );
    let cost = wire::price_call(
        target.rates,
        bill,
        &target.class,
        reply.charged,
        hold.as_ref().map(Reservation::reservation_usd),
    );
    log::line(&usage_line(
        (&reply.provider, reply.input_tokens, reply.output_tokens),
        target,
        &cost.amount,
        &request_id,
        revision,
        false,
    ));
    settle(hold, &cost);
    let stop = if reply.stop_reason.is_empty() {
        "end_turn".to_string()
    } else {
        reply.stop_reason
    };
    json(
        200,
        &Envelope {
            schema_version: schema,
            request_id,
            status: "ok",
            provider: reply.provider,
            model: target.name.clone(),
            message: Some(ReplyMessage {
                role: "assistant",
                content: reply.text,
                stop_reason: stop,
            }),
            tool_calls: reply.tool_calls,
            usage: Usage {
                input_tokens: reply.input_tokens,
                output_tokens: reply.output_tokens,
            },
            cost,
            latency_ms: reply.latency_ms,
            error: None,
        },
    )
}

/// Count an ok api call's `charged_usd` into today's spend; an unsettled hold is released on drop.
fn settle(hold: Option<Reservation>, cost: &Cost) {
    if let (Some(h), Some(charged)) = (hold, cost.charged_usd) {
        h.settle(charged, cost.over_reservation);
    }
}

struct PartialStreamBill {
    hold: Reservation,
    target: Target,
    meter: MeteredUsage,
    streamed_bytes: usize,
    est_input_tokens: i64,
    provider: String,
    request_id: String,
    revision: String,
}

/// After streamed output, a faulted stream still owes priced tokens (list-price settlement capped by the
/// admission estimate; a provider-reported charge above the hold is recorded in full with `over_reservation`).
fn settle_partial_stream(bill: PartialStreamBill) {
    let reservation = bill.hold.held_estimate();
    let mut input = bill.meter.input_tokens;
    let mut output = bill.meter.output_tokens;
    if output == 0 && bill.streamed_bytes > 0 {
        output = i64::try_from(bill.streamed_bytes.div_ceil(4))
            .unwrap_or(i64::MAX)
            .max(1);
    }
    if input == 0 && bill.est_input_tokens > 0 {
        input = bill.est_input_tokens;
    }
    let token_bill = token_bill(input, output, 0, 0);
    let provider_charge = bill.meter.charged.filter(|c| c.is_finite() && *c >= 0.0);
    let mut cost = wire::price_call(
        bill.target.rates,
        token_bill,
        &bill.target.class,
        provider_charge,
        Some(reservation),
    );
    let list = settle_usd(bill.target.rates, token_bill);
    let wallet = match provider_charge {
        Some(c) if c > 0.0 => c,
        _ => {
            let mut w = if list > 0.0 {
                list
            } else if output > 0 {
                f64::EPSILON
            } else {
                0.0
            };
            w = w.min(reservation);
            w
        }
    };
    if wallet <= 0.0 {
        return;
    }
    let over = wallet > reservation + 1e-9;
    cost.charged_usd = Some(wallet);
    cost.over_reservation = over;
    log::line(&usage_line(
        (&bill.provider, input, output),
        &bill.target,
        &cost.amount,
        &bill.request_id,
        &bill.revision,
        true,
    ));
    bill.hold.settle(wallet, over);
}

struct StreamState {
    stream: Option<OpenStream>,
    hold: Option<Reservation>,
    family: &'static str,
    request_id: String,
    target: Target,
    revision: String,
    est_input_tokens: i64,
    index: i64,
    streamed_bytes: usize,
    client_saw_output: bool,
}

impl StreamState {
    fn maybe_settle_partial(&mut self, provider: &str) {
        if !self.client_saw_output || self.target.class != "api" {
            return;
        }
        let Some(stream) = self.stream.as_ref() else {
            return;
        };
        let meter = stream.metered_usage();
        let Some(hold) = self.hold.take() else {
            return;
        };
        settle_partial_stream(PartialStreamBill {
            hold,
            target: self.target.clone(),
            meter,
            streamed_bytes: self.streamed_bytes,
            est_input_tokens: self.est_input_tokens,
            provider: provider.to_string(),
            request_id: self.request_id.clone(),
            revision: self.revision.clone(),
        });
    }

    /// The next SSE frame; the upstream stream (and its permit) is dropped at the terminal chunk.
    async fn frame(&mut self) -> Option<String> {
        let stream = self.stream.as_mut()?;
        let terminal = |s: &mut Self, chunk: Chunk| {
            s.stream = None;
            Some(chunk.frame())
        };
        match stream.next().await {
            Ok(Some(Event::Delta(text))) => {
                self.client_saw_output = !text.is_empty();
                self.streamed_bytes += text.len();
                let c = Chunk::delta(self.family, &self.request_id, self.index, text);
                self.index += 1;
                Some(c.frame())
            }
            Ok(Some(Event::Stop(stop))) => {
                let bill = token_bill(
                    stop.input_tokens,
                    stop.output_tokens,
                    stop.cache_read_tokens,
                    stop.cache_write_tokens,
                );
                let cost = wire::price_call(
                    self.target.rates,
                    bill,
                    &self.target.class,
                    stop.charged,
                    self.hold.as_ref().map(Reservation::reservation_usd),
                );
                log::line(&usage_line(
                    (&stop.provider, stop.input_tokens, stop.output_tokens),
                    &self.target,
                    &cost.amount,
                    &self.request_id,
                    &self.revision,
                    false,
                ));
                settle(self.hold.take(), &cost);
                let mut c = Chunk::delta(self.family, &self.request_id, self.index, String::new());
                c.kind = "stop";
                c.stop_reason = stop.reason;
                c.provider = stop.provider;
                c.model.clone_from(&self.target.name);
                c.tool_calls = stop.tool_calls;
                c.usage = Some(Usage {
                    input_tokens: stop.input_tokens,
                    output_tokens: stop.output_tokens,
                });
                c.cost = Some(cost);
                terminal(self, c)
            }
            Ok(Some(Event::UpstreamError(msg))) => {
                let provider = self.target.provider.clone();
                self.maybe_settle_partial(&provider);
                let msg = if msg.is_empty() {
                    "upstream stream error".to_string()
                } else {
                    msg
                };
                let e = WireError::runtime("gateway.upstream_error", &msg);
                let c = Chunk::error(self.family, &self.request_id, e);
                terminal(self, c)
            }
            Ok(None) => {
                let provider = self.target.provider.clone();
                self.maybe_settle_partial(&provider);
                let e = WireError::runtime(
                    "gateway.transport_failed",
                    "upstream stream ended without a stop event",
                );
                let c = Chunk::error(self.family, &self.request_id, e);
                terminal(self, c)
            }
            Err(e) => {
                let provider = self.target.provider.clone();
                self.maybe_settle_partial(&provider);
                let c = Chunk::error(self.family, &self.request_id, CallError::to_wire(&e));
                terminal(self, c)
            }
        }
    }
}

impl Drop for StreamState {
    fn drop(&mut self) {
        if self.hold.is_none() {
            return;
        }
        let provider = self.target.provider.clone();
        self.maybe_settle_partial(&provider);
    }
}

fn stream_response(
    stream: OpenStream,
    family: &'static str,
    request_id: String,
    target: Target,
    revision: String,
    hold: Option<Reservation>,
    est_input_tokens: i64,
) -> Response<Body> {
    let state = StreamState {
        stream: Some(stream),
        hold,
        family,
        request_id,
        target,
        revision,
        est_input_tokens,
        index: 0,
        streamed_bytes: 0,
        client_saw_output: false,
    };
    let frames = futures_util::stream::unfold(state, |mut s| async move {
        let f = s.frame().await?;
        Some((Ok::<_, Infallible>(Frame::data(Bytes::from(f))), s))
    });
    let mut r = Response::new(BodyExt::boxed_unsync(StreamBody::new(frames.boxed())));
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/event-stream"),
    );
    sse_headers(&mut r);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{ModelEntry, Observed};

    #[test]
    #[allow(clippy::unwrap_used)]
    fn agent_debug_row_hides_message_text() {
        let secret = "super-secret-prompt-text";
        let body = format!(r#"{{"messages":[{{"role":"user","content":"{secret}"}}]}}"#);
        let raw: serde_json::Value = serde_json::from_str(&body).unwrap();
        let row = agent_debug_row(&raw["messages"], body.as_bytes());
        let line = row.to_string();
        assert!(!line.contains(secret));
        assert!(line.contains("payload_sha256"));
        assert_eq!(row["message_count"], 1);
        assert_eq!(row["roles"], serde_json::json!(["user"]));
    }

    #[test]
    fn usage_line_names_the_bound_revision() {
        let t = Target {
            name: "m".into(),
            provider: "vllm".into(),
            rates: ResolvedRates {
                input_per_1k: 0.0,
                output_per_1k: 0.0,
                cache_read_per_1k: 0.0,
                cache_write_per_1k: 0.0,
            },
            class: "free_local".into(),
        };
        assert_eq!(
            usage_line(
                ("vllm", 3, 2),
                &t,
                "0.000000",
                "req-1",
                "r-0123456789ab",
                false
            ),
            "USAGE provider=vllm model=m in=3 out=2 cost=0.000000 class=free_local req=req-1 rev=r-0123456789ab"
        );
    }

    #[test]
    fn resolution_rules() {
        let e = |name: &str, provider: &str, tier: i64| ModelEntry {
            name: name.into(),
            provider: provider.into(),
            tier,
            ..ModelEntry::default()
        };
        let seen = Observed {
            vllm: vec!["local-coder".into(), "big".into()],
            ..Observed::default()
        };
        let inv = Inventory::build(
            Some(vec![e("local-coder", "vllm", 1), e("paid", "openai", 1)]),
            &seen,
        );
        assert_eq!(
            resolve(&inv, "").ok().map(|t| t.name),
            Some("local-coder".into())
        );
        assert_eq!(
            resolve(&inv, "nope").err().map(|e| e.code),
            Some("gateway.unknown_model")
        );
        assert_eq!(
            resolve(&inv, "paid").ok().map(|t| t.class),
            Some("api".into())
        );
        let api_only = Inventory::build(Some(vec![e("paid", "openai", 1)]), &Observed::default());
        assert_eq!(
            resolve(&api_only, "").err().map(|e| e.code),
            Some("gateway.api_explicit_only")
        );
    }
}
