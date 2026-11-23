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
use crate::trace::{Exchange, Stamp};
use crate::wire::{self, Chunk, Cost, ReplyMessage, Request, Response as Envelope, Usage};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, LengthLimitError, Limited, StreamBody};
use hyper::Response;
use hyper::body::{Frame, Incoming};
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
async fn undecodable(trace: &mut Exchange, err: &str) -> Response<Body> {
    trace.finish("refused_before_provider_contact", Some("bad_request"));
    trace.flush().await;
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
async fn refuse(
    trace: &mut Exchange,
    stream: bool,
    schema: &'static str,
    request_id: &str,
    e: WireError,
) -> Response<Body> {
    trace.finish("refused_before_provider_contact", Some(e.code));
    trace.flush().await;
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
        tool_choice: req.tool_choice.clone(),
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
    trace: &Exchange,
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
    crate::log::row(&trace.attributes(serde_json::json!({
        "event": "wire_request", "schema_version": schema, "caller": caller, "path": path,
    })));
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

/// Receive the request body under the model-call size limit, including it in exchange time.
pub async fn receive(gw: &Gateway, path: &str, caller: &str, body: Incoming) -> Response<Body> {
    const MODEL_CALL_MAX: usize = 32 * 1024 * 1024;
    let start = Stamp::now();
    match Limited::new(body, MODEL_CALL_MAX).collect().await {
        Ok(body) => handle(gw, path, caller, &body.to_bytes(), start).await,
        Err(error) => {
            let mut trace = Exchange::new(gw.settings.home.as_deref(), start, &Request::default());
            if error.downcast_ref::<LengthLimitError>().is_some() {
                trace.finish("refused_before_provider_contact", Some("payload_too_large"));
                trace.flush().await;
                super::error(413, "model call over 32 MiB", "payload_too_large")
            } else {
                undecodable(&mut trace, &error.to_string()).await
            }
        }
    }
}

async fn handle(
    gw: &Gateway,
    path: &str,
    caller: &str,
    body: &Bytes,
    start: Stamp,
) -> Response<Body> {
    let req: Request = match crate::protocol::openai::first_value(body) {
        Ok(r) => r,
        Err(e) => {
            return undecodable(
                &mut Exchange::new(gw.settings.home.as_deref(), start, &Request::default()),
                &e,
            )
            .await;
        }
    };
    let mut trace = Exchange::new(gw.settings.home.as_deref(), start, &req);
    let schema = match accept(gw, path, caller, &req, &trace) {
        Ok(s) => s,
        Err(e) => return refuse(&mut trace, false, wire::REQUEST_V2, &req.request_id, e).await,
    };
    handle_request(gw, body, req, schema, trace).await
}

async fn handle_request(
    gw: &Gateway,
    body: &Bytes,
    req: Request,
    schema: &'static str,
    mut trace: Exchange,
) -> Response<Body> {
    if req.messages.is_empty() {
        let e = WireError::fatal(
            "gateway.empty_messages",
            "messages array must contain at least one entry",
        );
        return refuse(&mut trace, req.stream, schema, &req.request_id, e).await;
    }
    let snap = gw.bind(&req.request_id);
    let target = match resolve(&snap.inventory(), &req.capability.model_tier_hint) {
        Ok(t) => t,
        Err(e) => return refuse(&mut trace, req.stream, schema, &req.request_id, e).await,
    };
    trace.billing(&target.class);
    if gw.settings.env.get("CSGATEWAY_AGENT_DEBUG", "") == "1" {
        let raw: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        crate::log::row(&trace.attributes(agent_debug_row(&raw["messages"], body)));
    }
    let call = match invocation(gw, &req, &target) {
        Ok(c) => c,
        Err(e) => return refuse(&mut trace, req.stream, schema, &req.request_id, e).await,
    };
    // Pay-as-you-go calls reserve before connecting. Other routes can report a charge afterward.
    let hold = if target.class == "api" {
        let est = crate::spend::estimate(target.rates, body.len(), req.budget.max_output_tokens);
        match gw
            .spend
            .admit_async(&target.provider, &req.budget.max_usd, est)
            .await
        {
            Ok(h) => Some(h),
            Err(d) => {
                return refuse(
                    &mut trace,
                    req.stream,
                    schema,
                    &req.request_id,
                    WireError::budget(d),
                )
                .await;
            }
        }
    } else {
        None
    };
    trace.wallet(Arc::clone(&gw.spend), hold);
    let timeout = u64::try_from(req.budget.timeout_ms)
        .ok()
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
    let deadline = tokio::time::Instant::now() + timeout;
    match with_retry(gw, &snap, &target, &call, deadline, req.stream, &mut trace).await {
        Ok(UpstreamReply::Stream(stream)) => stream_response(StreamState {
            stream: Some(stream),
            family: wire::chunk_family(schema),
            request_id: req.request_id,
            target,
            revision: snap.revision.clone(),
            est_input_tokens: i64::try_from(body.len().div_ceil(4)).unwrap_or(i64::MAX),
            index: 0,
            streamed_bytes: 0,
            trace,
        }),
        Ok(UpstreamReply::Unary(reply)) => {
            if !reply.text.is_empty() || !reply.tool_calls.is_empty() {
                trace.output();
            }
            trace.end_attempt("success");
            let response = ok(
                schema,
                req.request_id,
                &target,
                &snap.revision,
                reply,
                &mut trace,
            );
            trace.finish("completed", None);
            trace.flush().await;
            response
        }
        Err(error) => upstream_error(&mut trace, &req, schema, error, deadline).await,
    }
}

async fn upstream_error(
    trace: &mut Exchange,
    req: &Request,
    schema: &'static str,
    error: CallError,
    deadline: Instant,
) -> Response<Body> {
    let (outcome, refusal) = trace.failure(&error, Instant::now() >= deadline);
    if error.is_connection_lost() {
        trace.connection_lost();
    }
    let cost = trace.settle_abort(outcome);
    trace.finish(outcome, refusal);
    trace.flush().await;
    let error = error.to_wire();
    if req.stream {
        let mut chunk = Chunk::error(wire::chunk_family(schema), &req.request_id, error);
        if let Some(cost) = cost {
            chunk.cost = Some(cost);
        }
        sse(chunk.frame())
    } else {
        let mut response = Envelope::error(schema, &req.request_id, error);
        if let Some(cost) = cost {
            response.cost = cost;
        }
        json(200, &response)
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

enum UpstreamReply {
    Unary(crate::protocol::Reply),
    Stream(Box<OpenStream>),
}

async fn with_retry(
    gw: &Gateway,
    snap: &crate::gateway::Snapshot,
    target: &Target,
    call: &Call,
    deadline: Instant,
    stream: bool,
    trace: &mut Exchange,
) -> Result<UpstreamReply, CallError> {
    let provider = &target.provider;
    let mut attempt = 0;
    loop {
        let result = if stream {
            gw.stream(snap, provider, call, deadline, trace)
                .await
                .map(|stream| UpstreamReply::Stream(Box::new(stream)))
        } else {
            gw.invoke(snap, provider, call, deadline, trace)
                .await
                .map(UpstreamReply::Unary)
        };
        match result {
            Err(e) if e.is_connection_lost() && target.class != FREE_LOCAL => {
                return Err(e.without_transport_retry());
            }
            Err(e) if e.gateway_retryable() => {
                let wait_ms = retry_backoff_ms(attempt, e.retry_after_ms());
                if e.is_connection_lost() {
                    trace.connection_lost();
                    // Let the terminal error handler finish the last attempt.
                    if attempt == MAX_UPSTREAM_RETRIES
                        || Duration::from_millis(wait_ms)
                            >= deadline.saturating_duration_since(Instant::now())
                    {
                        return Err(e);
                    }
                }
                trace.end_attempt("retryable_failure");
                if attempt == MAX_UPSTREAM_RETRIES || Instant::now() >= deadline {
                    return Err(e);
                }
                crate::log::row(&trace.attributes(serde_json::json!({
                    "event": "gateway_upstream_retry",
                    "provider": provider,
                    "attempt": attempt + 1,
                    "wait_ms": wait_ms,
                    "stream": stream,
                    "code": e.to_wire().code,
                })));
                attempt += 1;
                let wait = Duration::from_millis(wait_ms)
                    .min(deadline.saturating_duration_since(Instant::now()));
                if !wait.is_zero() {
                    trace.begin_backoff();
                    tokio::time::sleep(wait).await;
                    trace.end_backoff("completed");
                }
                if Instant::now() >= deadline {
                    return Err(e);
                }
            }
            other => return other,
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
    trace: &mut Exchange,
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
        trace.reservation(),
    );
    log::line(&usage_line(
        (&reply.provider, reply.input_tokens, reply.output_tokens),
        target,
        &cost.amount,
        &request_id,
        revision,
        false,
    ));
    trace.settle(&target.provider, Some(bill), &cost, false, false);
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

struct PartialStreamBill<'a> {
    target: &'a Target,
    meter: MeteredUsage,
    streamed_bytes: usize,
    est_input_tokens: i64,
    provider: &'a str,
    request_id: &'a str,
    revision: &'a str,
}

/// After streamed output, a faulted stream still owes priced tokens (list-price settlement capped by the
/// admission estimate; a provider-reported charge above the hold is recorded in full with `over_reservation`).
fn settle_partial_stream(
    bill: &PartialStreamBill<'_>,
    trace: &mut Exchange,
    estimated: Option<f64>,
) -> Cost {
    let reservation = trace.reservation();
    let mut input = bill.meter.input_tokens;
    let mut output = bill.meter.output_tokens;
    if output == 0 && bill.streamed_bytes > 0 {
        output = i64::try_from(bill.streamed_bytes.div_ceil(4))
            .unwrap_or(i64::MAX)
            .max(1);
    }
    if input == 0 && bill.meter.cache_read_tokens == 0 && bill.meter.cache_write_tokens == 0 {
        input = bill.est_input_tokens;
    }
    let token_bill = token_bill(
        input,
        output,
        bill.meter.cache_read_tokens,
        bill.meter.cache_write_tokens,
    );
    let provider_charge = bill.meter.charged.filter(|c| c.is_finite() && *c > 0.0);
    let mut cost = wire::price_call(
        bill.target.rates,
        token_bill,
        &bill.target.class,
        provider_charge,
        reservation,
    );
    let list = settle_usd(bill.target.rates, token_bill);
    let wallet = match (provider_charge, estimated) {
        (Some(c), _) if c > 0.0 => c,
        (_, Some(reserve)) => reserve,
        _ => {
            let mut w = if list > 0.0 {
                list
            } else if output > 0 {
                f64::EPSILON
            } else {
                0.0
            };
            w = w.min(reservation.unwrap_or_default());
            w
        }
    };
    let over = reservation.is_some_and(|held| wallet > held + 1e-9);
    cost.charged_usd = Some(wallet);
    cost.over_reservation = over;
    log::line(&usage_line(
        (bill.provider, input, output),
        bill.target,
        &cost.amount,
        bill.request_id,
        bill.revision,
        true,
    ));
    trace.settle(
        &bill.target.provider,
        Some(token_bill),
        &cost,
        true,
        estimated.is_some(),
    );
    cost
}

struct StreamState {
    stream: Option<Box<OpenStream>>,
    family: &'static str,
    request_id: String,
    target: Target,
    revision: String,
    est_input_tokens: i64,
    index: i64,
    streamed_bytes: usize,
    trace: Exchange,
}

impl StreamState {
    fn maybe_settle_partial(&mut self, provider: &str, outcome: &str) -> Option<Cost> {
        let meter = self.stream.as_ref()?.metered_usage();
        let reported = meter
            .charged
            .is_some_and(|charge| charge.is_finite() && charge > 0.0);
        let reservation = self.trace.aborted_reservation(outcome);
        if !(reported || self.trace.saw_output() && self.trace.reservation().is_some()) {
            return self.trace.settle_abort(outcome);
        }
        Some(settle_partial_stream(
            &PartialStreamBill {
                target: &self.target,
                meter,
                streamed_bytes: self.streamed_bytes,
                est_input_tokens: self.est_input_tokens,
                provider,
                request_id: &self.request_id,
                revision: &self.revision,
            },
            &mut self.trace,
            reservation,
        ))
    }

    async fn error_chunk(
        &mut self,
        provider: &str,
        error: WireError,
        outcome: &'static str,
    ) -> Chunk {
        let cost = self.maybe_settle_partial(provider, outcome);
        self.trace.end_attempt(outcome);
        let mut chunk = Chunk::error(self.family, &self.request_id, error);
        if let Some(cost) = cost {
            chunk.cost = Some(cost);
        }
        self.stream = None;
        self.trace.finish(outcome, None);
        self.trace.flush().await;
        chunk
    }

    /// The next SSE frame; the upstream stream (and its permit) is dropped at the terminal chunk.
    async fn frame(&mut self) -> Option<String> {
        let stream = self.stream.as_mut()?;
        match stream.next().await {
            Ok(Some(Event::Delta(text))) => {
                if !text.is_empty() {
                    self.trace.output();
                }
                self.streamed_bytes += text.len();
                let c = Chunk::delta(self.family, &self.request_id, self.index, text);
                self.index += 1;
                Some(c.frame())
            }
            Ok(Some(Event::Stop(stop))) => {
                if !stop.tool_calls.is_empty() {
                    self.trace.output();
                }
                self.stream = None;
                self.trace.end_attempt("success");
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
                    self.trace.reservation(),
                );
                log::line(&usage_line(
                    (&stop.provider, stop.input_tokens, stop.output_tokens),
                    &self.target,
                    &cost.amount,
                    &self.request_id,
                    &self.revision,
                    false,
                ));
                self.trace
                    .settle(&self.target.provider, Some(bill), &cost, false, false);
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
                self.trace.finish("completed", None);
                self.trace.flush().await;
                Some(c.frame())
            }
            Ok(Some(Event::UpstreamError(msg))) => {
                let provider = self.target.provider.clone();
                let msg = if msg.is_empty() {
                    "upstream stream error".to_string()
                } else {
                    msg
                };
                let e = WireError::runtime("gateway.upstream_error", &msg);
                Some(
                    self.error_chunk(&provider, e, "upstream_error")
                        .await
                        .frame(),
                )
            }
            Ok(None) => {
                let provider = self.target.provider.clone();
                let e = WireError::runtime(
                    "gateway.transport_failed",
                    "upstream stream ended without a stop event",
                );
                Some(self.error_chunk(&provider, e, "stream_fault").await.frame())
            }
            Err(e) => {
                if e.is_connection_lost() {
                    self.trace.connection_lost();
                }
                let provider = self.target.provider.clone();
                let outcome = if e.is_deadline() {
                    "deadline"
                } else {
                    "stream_fault"
                };
                Some(
                    self.error_chunk(&provider, e.to_wire(), outcome)
                        .await
                        .frame(),
                )
            }
        }
    }
}

impl Drop for StreamState {
    fn drop(&mut self) {
        if self.stream.is_some() {
            let provider = self.target.provider.clone();
            let _ = self.maybe_settle_partial(&provider, "client_disconnect");
            self.stream = None;
            self.trace.finish("client_disconnect", None);
        }
    }
}

fn stream_response(state: StreamState) -> Response<Body> {
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
    use crate::spend::Guard;

    #[tokio::test]
    async fn settles_each_cost_class_with_and_without_a_report()
    -> Result<(), Box<dyn std::error::Error>> {
        use serde_json::{Value, json};

        for class in ["api", "subscription", "free_local"] {
            for reported in [None, Some(0.25)] {
                let home = tempfile::tempdir()?;
                let guard = Arc::new(Guard::new(Some(home.path())));
                assert!(
                    guard
                        .replace(br#"{"sources":{"*":{"usd":1,"period":"day"}}}"#)
                        .is_ok()
                );
                let hold = (class == "api")
                    .then(|| guard.admit("vllm", "", 0.1))
                    .transpose()?;
                let request = serde_json::from_value(json!({"run_id": "billing"}))?;
                let mut trace = Exchange::new(Some(home.path()), Stamp::now(), &request);
                trace.billing(class);
                trace.wallet(Arc::clone(&guard), hold);
                let cost = Cost::priced("0.100000".into(), class, reported, reported.is_some());
                trace.settle("vllm", Some(token_bill(3, 2, 0, 0)), &cost, false, false);
                trace.finish("completed", None);
                trace.flush().await;

                let expected = reported.or((class == "api").then_some(0.1));
                let budget = guard.report(["vllm".to_owned()]);
                assert_eq!(
                    budget["total"]["spent_usd"].as_f64(),
                    expected.or(Some(0.0))
                );
                assert_eq!(budget["total"]["held_usd"], json!(0.0));
                let rows = std::fs::read_to_string(
                    home.path()
                        .join(".contextstack/gateway/traces/billing.jsonl"),
                )?;
                let rows = rows
                    .lines()
                    .map(serde_json::from_str::<Value>)
                    .collect::<Result<Vec<_>, _>>()?;
                let settlement = rows.iter().find(|row| row["kind"] == "settlement");
                assert_eq!(
                    settlement.and_then(|row| row["attributes"]["charged_usd"].as_f64()),
                    expected
                );
                assert_eq!(
                    settlement.and_then(|row| row["attributes"]["basis"].as_str()),
                    reported.map(|_| "provider_reported"),
                );
            }
        }
        Ok(())
    }

    #[test]
    fn agent_debug_row_hides_message_text() -> serde_json::Result<()> {
        let secret = "super-secret-prompt-text";
        let body = format!(r#"{{"messages":[{{"role":"user","content":"{secret}"}}]}}"#);
        let raw: serde_json::Value = serde_json::from_str(&body)?;
        let row = agent_debug_row(&raw["messages"], body.as_bytes());
        let line = row.to_string();
        assert!(!line.contains(secret));
        assert!(line.contains("payload_sha256"));
        assert_eq!(row["message_count"], 1);
        assert_eq!(row["roles"], serde_json::json!(["user"]));
        Ok(())
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
