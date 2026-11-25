//! Request-scoped traces. Buffer the small set of records and append them before the terminal frame.

use crate::errors::CallError;
use crate::pricing::{ResolvedRates, TokenBill, settle_usd};
use crate::spend::{Guard, Reservation};
use crate::wire::{Cost, Request};
use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use hyper::body::{Body, Frame, SizeHint};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

#[derive(Clone, Copy)]
pub struct Stamp {
    wall: DateTime<Utc>,
    clock: Instant,
}

impl Stamp {
    pub fn now() -> Self {
        Self {
            wall: Utc::now(),
            clock: Instant::now(),
        }
    }
}

struct Attempt {
    start: Stamp,
    fields: Value,
    first_output: bool,
    body_sent: Option<Arc<AtomicBool>>,
    connection_lost: bool,
    /// Usage the provider reported on a 2xx body the adapter then rejected.
    measured: Option<TokenBill>,
}

/// A request body whose flag turns true once the transport has consumed its final frame.
pub fn tracked_body(raw: Bytes) -> (reqwest::Body, Arc<AtomicBool>) {
    let sent = Arc::new(AtomicBool::new(false));
    let body = reqwest::Body::wrap(TrackedBody {
        bytes: raw,
        sent: Arc::clone(&sent),
    });
    (body, sent)
}

// The HTTP transport marks the body sent when it consumes the final frame.
struct TrackedBody {
    bytes: Bytes,
    sent: Arc<AtomicBool>,
}

impl Body for TrackedBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.bytes.is_empty() {
            return Poll::Ready(None);
        }
        let len = self.bytes.len().min(16 * 1024);
        let frame = self.bytes.split_to(len);
        if self.bytes.is_empty() {
            self.sent.store(true, Ordering::Relaxed);
        }
        Poll::Ready(Some(Ok(Frame::data(frame))))
    }

    fn is_end_stream(&self) -> bool {
        self.bytes.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.bytes.len() as u64)
    }
}

pub struct Exchange {
    start: Stamp,
    path: Option<PathBuf>,
    trace_id: String,
    id: String,
    parent: String,
    identity: Value,
    rows: Vec<Value>,
    billing_source: &'static str,
    saw_output: bool,
    provider_charge_usd: Option<f64>,
    attempts: u32,
    attempt: Option<Attempt>,
    backoff: Option<Stamp>,
    final_attempt: Option<&'static str>,
    finished: bool,
    tool_choice_dropped: bool,
    guard: Option<Arc<Guard>>,
    hold: Option<Reservation>,
    rates: Option<ResolvedRates>,
    write: Option<tokio::task::JoinHandle<()>>,
}

impl Exchange {
    pub fn new(home: Option<&Path>, start: Stamp, req: &Request) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        static MACHINE: OnceLock<String> = OnceLock::new();
        let machine = MACHINE.get_or_init(|| {
            rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned()
        });
        let id = format!(
            "gateway-{}-{}-{}-{}",
            machine,
            std::process::id(),
            start.wall.timestamp_micros(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let run = req.run_id.as_deref().filter(|run| !run.is_empty());
        let file = match run {
            None => format!("unattributed-{}", start.wall.date_naive()),
            Some(run)
                if run.len() <= 160
                    && !run.starts_with("unattributed-")
                    && run
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) =>
            {
                run.to_owned()
            }
            Some(run) => format!("run+{}", crate::log::sha256_hex(run.as_bytes())),
        };
        let mut identity = json!({
            "machine": machine,
            "run_id": run,
            "job_id": req.job_id,
            "step_id": req.step_id.as_ref().filter(|step| step.is_string() || step.is_u64()),
            "request_id": req.request_id,
            "send_index": req.send_index.unwrap_or(1),
        });
        if let Some(fields) = identity.as_object_mut() {
            fields.retain(|_, value| !value.is_null());
        }
        let parent = if req.request_id.is_empty() {
            format!("{id}-unattributed-call")
        } else {
            req.request_id.clone()
        };
        Self {
            start,
            path: home.map(|home| {
                home.join(".contextstack/gateway/traces")
                    .join(format!("{file}.jsonl"))
            }),
            trace_id: run.unwrap_or(&id).to_owned(),
            id,
            parent,
            identity,
            rows: Vec::new(),
            billing_source: "pay_as_you_go",
            saw_output: false,
            provider_charge_usd: None,
            attempts: 0,
            attempt: None,
            backoff: None,
            final_attempt: None,
            finished: false,
            tool_choice_dropped: false,
            guard: None,
            hold: None,
            rates: None,
            write: None,
        }
    }

    pub fn attributes(&self, extra: Value) -> Value {
        let mut fields = self.identity.clone();
        if let (Some(fields), Value::Object(extra)) = (fields.as_object_mut(), extra) {
            fields.extend(extra);
        }
        fields
    }

    pub fn billing(&mut self, class: &str) {
        self.billing_source = match class {
            "free_local" => "free_local",
            "subscription" => "subscription",
            _ => "pay_as_you_go",
        };
    }

    pub fn begin_attempt(&mut self, provider: &str, body: &[u8], stream: bool) {
        self.attempts += 1;
        let reservation = self.reservation();
        self.attempt = Some(Attempt {
            start: Stamp::now(),
            fields: json!({
                "attempt": self.attempts, "provider": provider, "stream": stream,
                "request_bytes": body.len(), "request_sha256": crate::log::sha256_hex(body),
            }),
            first_output: false,
            body_sent: None,
            connection_lost: false,
            measured: None,
        });
        if let Some(reserved) = reservation
            && let Some(attempt) = &mut self.attempt
        {
            attempt.fields["reservation_usd"] = json!(reserved);
        }
    }

    pub fn wallet(&mut self, guard: Arc<Guard>, hold: Option<Reservation>) {
        self.guard = Some(guard);
        self.hold = hold;
    }

    /// The resolved target's rates, used to price usage measured on a failed 2xx reply.
    pub fn price_with(&mut self, rates: ResolvedRates) {
        self.rates = Some(rates);
    }

    /// Usage a 2xx body reported before the adapter rejected the body (e.g. a negative charge).
    pub fn measured_usage(&mut self, bill: TokenBill) {
        if let Some(attempt) = &mut self.attempt {
            attempt.measured = Some(bill);
        }
    }

    pub fn reservation(&self) -> Option<f64> {
        self.hold.as_ref().map(Reservation::reservation_usd)
    }

    pub fn request_body(&mut self, raw: Bytes) -> reqwest::Body {
        if let Some(attempt) = &mut self.attempt {
            let (body, sent) = tracked_body(raw);
            attempt.body_sent = Some(sent);
            return body;
        }
        raw.into()
    }

    pub fn request_body_sent(&self) -> bool {
        self.attempt
            .as_ref()
            .and_then(|attempt| attempt.body_sent.as_ref())
            .is_some_and(|sent| sent.load(Ordering::Relaxed))
    }

    pub fn connection_lost(&mut self) {
        if let Some(attempt) = &mut self.attempt {
            attempt.connection_lost = true;
        }
    }

    /// Whether the exchange was aborted rather than answered: deadline, disconnect or connection loss.
    pub fn aborted(&self, outcome: &str) -> bool {
        matches!(outcome, "deadline" | "client_disconnect")
            || self.attempt.as_ref().is_some_and(|a| a.connection_lost)
    }

    /// The reservation a paid attempt owes when it ends without a usable reply: its body was sent and
    /// the provider did not refuse, and either the exchange was aborted (deadline, disconnect,
    /// connection loss) or the provider had already answered 2xx (the call is billed; a body the
    /// gateway cannot use does not make it free).
    pub fn aborted_reservation(&self, outcome: &str) -> Option<f64> {
        let attempt = self.attempt.as_ref()?;
        let sent = attempt.body_sent.as_ref()?.load(Ordering::Relaxed);
        let status = attempt.fields["http_status"].as_u64();
        let refused = status.is_some_and(|s| s >= 300);
        let answered = status.is_some_and(|s| s < 300);
        if sent
            && !refused
            && (answered
                || attempt.connection_lost
                || matches!(outcome, "deadline" | "client_disconnect"))
        {
            self.reservation()
        } else {
            None
        }
    }

    /// Settle an attempt that ended without a reply, keeping the reservation as a minimum.
    pub fn settle_abort(&mut self, outcome: &str) -> Option<Cost> {
        let reserved = self.aborted_reservation(outcome)?;
        let measured = self.attempt.as_ref().and_then(|a| a.measured);
        let list = measured
            .zip(self.rates)
            .map(|(bill, rates)| settle_usd(rates, bill))
            .filter(|usd| usd.is_finite());
        let charged = list.map_or(reserved, |usd| usd.max(reserved));
        let mut cost = Cost::priced(
            format!("{charged:.6}"),
            "api",
            None,
            charged > reserved + 1e-9,
        );
        cost.est_usd = Some(charged);
        cost.charged_usd = Some(charged);
        self.settle("", measured, &mut cost, true, list.is_none());
        Some(cost)
    }

    fn attempt_id(&self) -> String {
        format!("{}-attempt-{}", self.id, self.attempts)
    }

    pub fn headers(&mut self, status: u16) {
        if let Some(attempt) = &mut self.attempt {
            attempt.fields["http_status"] = json!(status);
        }
        if status < 300 {
            self.attempt_event("connection_open");
        }
    }

    pub fn upstream_error_body(&mut self, body: &str) {
        if let Some(attempt) = &mut self.attempt {
            attempt.fields["upstream_error_bytes"] = json!(body.len());
            attempt.fields["upstream_error_sha256"] =
                json!(crate::log::sha256_hex(body.as_bytes()));
        }
    }

    pub fn attempt_diagnostics(&mut self, fields: Value) {
        if let Some(attempt) = &mut self.attempt
            && let (Some(existing), Value::Object(fields)) =
                (attempt.fields.as_object_mut(), fields)
        {
            existing.extend(fields);
        }
    }

    fn attempt_event(&mut self, kind: &str) {
        let parent = self.attempt_id();
        let now = Stamp::now();
        self.record(
            kind,
            &format!("{parent}-{kind}"),
            &parent,
            (now, now),
            "completed",
            json!({}),
        );
    }

    pub fn output(&mut self) {
        self.saw_output = true;
        if let Some(attempt) = &mut self.attempt
            && !attempt.first_output
        {
            attempt.first_output = true;
            self.attempt_event("first_output");
        }
    }

    pub fn end_attempt(&mut self, outcome: &'static str) {
        let Some(mut attempt) = self.attempt.take() else {
            return;
        };
        if let Some(sent) = attempt.body_sent {
            attempt.fields["request_body_sent"] = json!(sent.load(Ordering::Relaxed));
            if attempt.connection_lost {
                attempt.fields["connection_lost"] = json!(true);
            }
        }
        let end = Stamp::now();
        if self.identity["request_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
        {
            let mut row = self.attributes(attempt.fields.clone());
            row["event"] = json!("provider_http_trace");
            row["started_at"] = json!(crate::log::rfc3339(attempt.start.wall));
            row["complete_ms"] = json!((end.clock - attempt.start.clock).as_secs_f64() * 1000.0);
            row["failed"] = json!(outcome != "success");
            crate::log::row(&row);
        }
        self.final_attempt = Some(outcome);
        self.record(
            "provider_attempt",
            &self.attempt_id(),
            &self.id.clone(),
            (attempt.start, end),
            outcome,
            attempt.fields,
        );
    }

    pub fn begin_backoff(&mut self) {
        self.backoff = Some(Stamp::now());
    }

    pub fn end_backoff(&mut self, outcome: &str) {
        let Some(start) = self.backoff.take() else {
            return;
        };

        self.record(
            "retry_backoff",
            &format!("{}-wait", self.attempt_id()),
            &self.id.clone(),
            (start, Stamp::now()),
            outcome,
            json!({}),
        );
    }

    /// Record usage and settle the charge now, on the calling thread: a charge never waits on a task
    /// the runtime may not run (shutdown). A spend-file write that fails twice clears `charged_usd` so
    /// the reply never reports an unrecorded charge, and marks the settlement span `spend_write_failed`.
    pub fn settle(
        &mut self,
        source: &str,
        tokens: Option<TokenBill>,
        cost: &mut Cost,
        partial: bool,
        reservation_estimate: bool,
    ) {
        let now = Stamp::now();
        let reported = cost.reported_charge();
        let (settled, written) = if let Some(hold) = self.hold.take() {
            let charged = cost.charged_usd.unwrap_or_default();
            (Some(charged), hold.settle(charged, cost.over_reservation))
        } else if let Some(charge) = reported
            && let Some(guard) = self.guard.take()
        {
            (reported, guard.debit_reported(source, charge))
        } else {
            (reported, Ok(0.0))
        };
        let partial = partial || written.is_err();
        if let Some(tokens) = tokens {
            // Completed replies already carry the rounded list price. Partial replies carry the
            // settlement as their cost, so price the measured tokens independently for the floor.
            let estimated_usd = if partial {
                self.rates.map(|rates| settle_usd(rates, tokens))
            } else {
                cost.est_usd
            };
            self.child("usage", now, json!({
                "partial": partial, "estimated_usd": estimated_usd,
                "input_tokens": tokens.input_tokens, "output_tokens": tokens.output_tokens,
                "cache_read_tokens": tokens.cache_read_tokens, "cache_write_tokens": tokens.cache_write_tokens,
            }));
        } else if partial && (self.saw_output || reported.is_some()) {
            self.child("usage", now, json!({"partial": true}));
        }
        if let Some(charged) = settled {
            self.provider_charge_usd = reported;
            let basis = if reported.is_some() {
                "provider_reported"
            } else if reservation_estimate {
                "reservation_estimate"
            } else {
                "measured"
            };
            let mut fields = json!({"partial": partial, "basis": basis});
            let outcome = if written.is_ok() {
                fields["charged_usd"] = json!(charged);
                "completed"
            } else {
                *cost = Cost::zero();
                "spend_write_failed"
            };
            self.record(
                "settlement",
                &format!("{}-settlement", self.id),
                &self.id.clone(),
                (now, now),
                outcome,
                fields,
            );
        }
    }

    fn child(&mut self, kind: &str, now: Stamp, extra: Value) {
        self.record(
            kind,
            &format!("{}-{kind}", self.id),
            &self.id.clone(),
            (now, now),
            "completed",
            extra,
        );
    }

    pub fn failure(
        &self,
        error: &CallError,
        expired: bool,
    ) -> (&'static str, Option<&'static str>) {
        if expired || error.is_deadline() {
            ("deadline", None)
        } else if self.attempts == 0 {
            (
                "refused_before_provider_contact",
                Some(error.to_wire().code),
            )
        } else {
            ("upstream_error", None)
        }
    }

    pub fn drop_tool_choice(&mut self) {
        self.tool_choice_dropped = true;
    }

    pub fn finish(&mut self, outcome: &'static str, refusal: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.settle_abort(outcome);
        drop(self.hold.take());
        self.end_backoff(outcome);
        self.end_attempt(outcome);
        let mut fields = json!({
            "billing_source": self.billing_source, "client_saw_output": self.saw_output,
            "attempts_started": self.attempts, "final_attempt_outcome": self.final_attempt,
        });
        if self.tool_choice_dropped {
            fields["tool_choice_dropped"] = json!(true);
        }
        if let Some(charge) = self.provider_charge_usd {
            fields["provider_charge_usd"] = json!(charge);
        }
        if self.billing_source != "pay_as_you_go"
            && (outcome == "completed" || self.provider_charge_usd.is_some())
        {
            fields["wallet_charge_usd"] = json!(self.provider_charge_usd.unwrap_or_default());
        }
        if let Some(code) = refusal {
            fields["refusal_code"] = json!(match code {
                "gateway.streaming_unsupported" => "stream_unsupported",
                _ => code.strip_prefix("gateway.").unwrap_or(code),
            });
        }
        self.record(
            "gateway_exchange",
            &self.id.clone(),
            &self.parent.clone(),
            (self.start, Stamp::now()),
            outcome,
            fields,
        );
        if let Some(path) = self.path.take() {
            let rows = std::mem::take(&mut self.rows);
            let write = move || {
                if let Err(err) = append(&path, &rows) {
                    crate::logf!("gateway trace append failed: {err}");
                }
            };
            // Outside a runtime (or from a drop during shutdown) write inline.
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => self.write = Some(runtime.spawn_blocking(write)),
                Err(_) => write(),
            }
        }
    }

    pub async fn flush(&mut self) {
        if let Some(task) = self.write.take()
            && let Err(err) = task.await
        {
            crate::logf!("gateway trace writer failed: {err}");
        }
    }

    fn record(
        &mut self,
        kind: &str,
        id: &str,
        parent: &str,
        (start, end): (Stamp, Stamp),
        outcome: &str,
        extra: Value,
    ) {
        self.rows.push(json!({
            "trace_id": self.trace_id, "span_id": id, "parent_id": parent,
            "kind": kind, "name": kind, "outcome": outcome,
            "start": start.wall.to_rfc3339_opts(SecondsFormat::Micros, true),
            "end": end.wall.to_rfc3339_opts(SecondsFormat::Micros, true),
            "duration_ms": (end.clock - start.clock).as_secs_f64() * 1000.0,
            "attributes": self.attributes(extra),
        }));
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        self.finish("client_disconnect", None);
    }
}

fn append(path: &Path, rows: &[Value]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    static WRITER: Mutex<()> = Mutex::new(());
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row)?;
        bytes.push(b'\n');
    }
    let _writer = WRITER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_data()
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn read_spans(home: &Path, run: &str) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
        let path = home
            .join(".contextstack/gateway/traces")
            .join(format!("{run}.jsonl"));
        Ok(std::fs::read_to_string(path)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?)
    }

    pub fn check_spans(
        mut spans: Vec<Value>,
        terminal_charge: f64,
        missing: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let exchange = spans
            .iter()
            .find(|row| row["kind"] == "gateway_exchange")
            .ok_or("missing exchange")?;
        let envelope = |kind: &str, id: Value, parent: Value, fields: Value, outcome: &str| {
            let mut span = exchange.clone();
            span["kind"] = json!(kind);
            span["name"] = json!(kind);
            span["span_id"] = id;
            span["parent_id"] = parent;
            span["attributes"] = fields;
            span["attributes"]["machine"] = exchange["attributes"]["machine"].clone();
            span["outcome"] = json!(outcome);
            span
        };
        let request = &exchange["attributes"]["request_id"];
        let parents = [
            envelope(
                "model_call",
                exchange["parent_id"].clone(),
                json!("turn"),
                json!({
                    "request_id": request,
                    "sends": [{"send_index": 1, "outcome": "delivered"}],
                }),
                "error",
            ),
            envelope(
                "turn",
                json!("turn"),
                json!("run"),
                json!({"turn": 1}),
                "completed",
            ),
            envelope(
                "run",
                json!("run"),
                Value::Null,
                json!({
                    "run_id": exchange["trace_id"], "coverage": "full",
                    "terminal": {"turns": 1, "tool_calls": 0, "charged_usd": terminal_charge,
                                 "accounted_settlements": [{"request_id": request, "send_index": 1}]},
                }),
                "error",
            ),
        ];
        spans.extend(parents);
        let input = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(input.as_file(), &spans)?;
        let checker = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tracing/check-trace");
        let output = std::process::Command::new("python3")
            .arg(checker)
            .arg(input.path())
            .output()?;
        let result: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(result["findings"], json!([]));
        if let Some(missing) = missing {
            assert_eq!(output.status.code(), Some(1), "{result}");
            assert_eq!(result["result"], "incomplete");
            assert_eq!(
                result["missing"],
                json!([format!(
                    "3: {}/request r send 1 {missing}",
                    spans[0]["trace_id"].as_str().ok_or("trace ID")?
                )]),
            );
        } else {
            assert!(output.status.success(), "{result}");
            assert_eq!(result["result"], "complete");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{check_spans, read_spans};
    use super::*;

    #[tokio::test]
    async fn disconnect_before_body_dispatch_releases_reservation()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let guard = Arc::new(Guard::new(Some(home.path())));
        guard
            .replace(br#"{"api_usd_per_day": 1}"#)
            .map_err(|e| format!("{e:?}"))?;
        let reservation = guard.admit("provider", "", 0.1)?;
        let request = Request {
            run_id: Some("unsent".into()),
            ..Request::default()
        };
        let mut trace = Exchange::new(Some(home.path()), Stamp::now(), &request);
        trace.wallet(Arc::clone(&guard), Some(reservation));
        trace.begin_attempt("provider", b"{}", false);
        let body = trace.request_body(Bytes::from_static(b"{}"));
        drop(body);
        trace.finish("client_disconnect", None);
        trace.flush().await;
        let report = guard.report(["provider".to_owned()]);
        assert_eq!(report["total"]["spent_usd"], 0.0);
        assert_eq!(report["total"]["held_usd"], 0.0);
        let rows = std::fs::read_to_string(
            home.path()
                .join(".contextstack/gateway/traces/unsent.jsonl"),
        )?;
        assert!(rows.contains(r#""request_body_sent":false"#));
        assert!(!rows.contains(r#""kind":"settlement""#));
        Ok(())
    }

    fn paid(home: &Path, run: &str) -> Result<(Arc<Guard>, Exchange), Box<dyn std::error::Error>> {
        let guard = Arc::new(Guard::new(Some(home)));
        guard
            .replace(br#"{"api_usd_per_day": 1}"#)
            .map_err(|e| format!("{e:?}"))?;
        let reservation = guard.admit("provider", "", 0.1)?;
        let request = Request {
            run_id: Some(run.into()),
            request_id: "r".into(),
            ..Request::default()
        };
        let mut trace = Exchange::new(Some(home), Stamp::now(), &request);
        trace.wallet(Arc::clone(&guard), Some(reservation));
        Ok((guard, trace))
    }

    fn send(trace: &mut Exchange, status: Option<u16>) {
        trace.begin_attempt("provider", b"{}", false);
        let body = trace.request_body(Bytes::from_static(b"{}"));
        let mut body = std::pin::pin!(body);
        let waker = std::task::Waker::noop();
        while let Poll::Ready(Some(_)) =
            Body::poll_frame(body.as_mut(), &mut Context::from_waker(waker))
        {}
        if let Some(status) = status {
            trace.headers(status);
        }
    }

    // G4: no runtime, no flush (a shutting-down runtime runs neither): the charge is already recorded.
    #[test]
    fn settlement_records_the_charge_without_a_runtime() -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let (guard, mut trace) = paid(home.path(), "sync")?;
        send(&mut trace, Some(200));
        let mut cost = Cost::priced("0.050000".into(), "api", None, false);
        trace.settle("provider", None, &mut cost, false, false);
        let report = guard.report(["provider".to_owned()]);
        assert_eq!(report["total"]["spent_usd"], 0.05);
        assert_eq!(report["total"]["held_usd"], 0.0);
        trace.finish("completed", None);
        Ok(())
    }

    // G1: a 2xx whose body the gateway rejects is billed: the reservation, as an estimate.
    #[tokio::test]
    async fn a_rejected_2xx_body_settles_the_reservation() -> Result<(), Box<dyn std::error::Error>>
    {
        let home = tempfile::tempdir()?;
        let (guard, mut trace) = paid(home.path(), "rejected")?;
        send(&mut trace, Some(200));
        let cost = trace
            .settle_abort("upstream_error")
            .ok_or("no settlement")?;
        assert_eq!(cost.charged_usd, Some(0.1));
        assert_eq!(cost.amount, "0.100000");
        trace.finish("upstream_error", None);
        trace.flush().await;
        assert_eq!(
            guard.report(["provider".to_owned()])["total"]["spent_usd"],
            0.1
        );
        let rows = read_spans(home.path(), "rejected")?;
        let settlement = rows
            .iter()
            .find(|row| row["kind"] == "settlement")
            .ok_or("no settlement")?;
        assert_eq!(settlement["attributes"]["basis"], "reservation_estimate");
        check_spans(rows, 0.1, None)?;
        Ok(())
    }

    // G1: measured usage above the reservation is the floor.
    #[test]
    fn a_rejected_2xx_body_with_usage_settles_at_least_the_usage()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let (guard, mut trace) = paid(home.path(), "measured")?;
        trace.price_with(ResolvedRates {
            input_per_1k: 1.0,
            output_per_1k: 1.0,
            cache_read_per_1k: 0.0,
            cache_write_per_1k: 0.0,
        });
        send(&mut trace, Some(200));
        trace.measured_usage(TokenBill {
            input_tokens: 100,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        let cost = trace
            .settle_abort("upstream_error")
            .ok_or("no settlement")?;
        assert_eq!(cost.charged_usd, Some(0.2));
        assert!(cost.over_reservation);
        assert_eq!(
            guard.report(["provider".to_owned()])["total"]["spent_usd"],
            0.2
        );
        Ok(())
    }

    #[tokio::test]
    async fn aborted_usage_is_priced_independently_of_the_charge()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let (_, mut trace) = paid(home.path(), "usage-floor")?;
        trace.price_with(ResolvedRates {
            input_per_1k: 1.0,
            output_per_1k: 0.0,
            cache_read_per_1k: 0.0,
            cache_write_per_1k: 0.0,
        });
        send(&mut trace, Some(200));
        trace.measured_usage(TokenBill {
            input_tokens: 25,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        let cost = trace.settle_abort("deadline").ok_or("no settlement")?;
        assert_eq!(cost.charged_usd, Some(0.1));
        trace.finish("deadline", None);
        trace.flush().await;
        let mut rows = read_spans(home.path(), "usage-floor")?;
        let usage = rows
            .iter()
            .find(|row| row["kind"] == "usage")
            .ok_or("no usage")?;
        assert_eq!(usage["attributes"]["input_tokens"], 25);
        assert_eq!(usage["attributes"]["estimated_usd"], 0.025);
        check_spans(rows.clone(), 0.1, None)?;

        let settlement = rows
            .iter_mut()
            .find(|row| row["kind"] == "settlement")
            .ok_or("no settlement")?;
        assert_eq!(settlement["attributes"]["basis"], "measured");
        settlement["attributes"]["charged_usd"] = json!(0.02);
        check_spans(rows, 0.02, Some("charge below measured usage"))?;
        Ok(())
    }

    // A 2xx refusal of nothing: an HTTP refusal (>= 300) still releases at $0.
    #[test]
    fn an_http_refusal_still_releases() -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let (_guard, mut trace) = paid(home.path(), "refused")?;
        send(&mut trace, Some(400));
        assert!(trace.settle_abort("upstream_error").is_none());
        Ok(())
    }

    // G6: an unrecorded charge is not reported as charged, and the trace says so.
    #[tokio::test]
    async fn a_failed_spend_write_is_not_reported_charged() -> Result<(), Box<dyn std::error::Error>>
    {
        let home = tempfile::tempdir()?;
        let (_guard, mut trace) = paid(home.path(), "unwritten")?;
        std::fs::create_dir_all(home.path().join(".contextstack/gateway/spend.json"))?;
        send(&mut trace, Some(200));
        let mut cost = Cost::priced("0.050000".into(), "api", None, false);
        trace.settle("provider", None, &mut cost, false, false);
        assert_eq!(cost.charged_usd, None);
        assert_eq!(cost.amount, "0.000000");
        trace.finish("upstream_error", None);
        trace.flush().await;
        let rows = read_spans(home.path(), "unwritten")?;
        let settlement = rows
            .iter()
            .find(|row| row["kind"] == "settlement")
            .ok_or("no settlement")?;
        assert_eq!(settlement["outcome"], "spend_write_failed");
        assert!(settlement["attributes"].get("charged_usd").is_none());
        check_spans(rows, 0.0, Some("settlement charge"))?;
        Ok(())
    }
}
