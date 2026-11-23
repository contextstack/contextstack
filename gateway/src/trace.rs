//! Request-scoped traces. Buffer the small set of records and append them before the terminal frame.

use crate::errors::CallError;
use crate::pricing::TokenBill;
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
    charge: Option<Box<dyn FnOnce() + Send>>,
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
            charge: None,
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

    pub fn reservation(&self) -> Option<f64> {
        self.hold.as_ref().map(Reservation::reservation_usd)
    }

    pub fn request_body(&mut self, raw: Bytes) -> reqwest::Body {
        if let Some(attempt) = &mut self.attempt {
            let sent = Arc::new(AtomicBool::new(false));
            attempt.body_sent = Some(Arc::clone(&sent));
            return reqwest::Body::wrap(TrackedBody { bytes: raw, sent });
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

    pub fn aborted_reservation(&self, outcome: &str) -> Option<f64> {
        let attempt = self.attempt.as_ref()?;
        let sent = attempt.body_sent.as_ref()?.load(Ordering::Relaxed);
        let refused = attempt.fields["http_status"]
            .as_u64()
            .is_some_and(|s| s >= 300);
        if sent
            && !refused
            && (attempt.connection_lost || matches!(outcome, "deadline" | "client_disconnect"))
        {
            self.reservation()
        } else {
            None
        }
    }

    pub fn settle_abort(&mut self, outcome: &str) -> Option<Cost> {
        let reserved = self.aborted_reservation(outcome)?;
        let mut cost = Cost::priced(format!("{reserved:.6}"), "api", None, false);
        cost.est_usd = Some(reserved);
        cost.charged_usd = Some(reserved);
        self.settle("", None, &cost, true, true);
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

    pub fn settle(
        &mut self,
        source: &str,
        tokens: Option<TokenBill>,
        cost: &Cost,
        partial: bool,
        reservation_estimate: bool,
    ) {
        let now = Stamp::now();
        if let Some(tokens) = tokens {
            self.child("usage", now, json!({
                "partial": partial, "estimated_usd": cost.est_usd,
                "input_tokens": tokens.input_tokens, "output_tokens": tokens.output_tokens,
                "cache_read_tokens": tokens.cache_read_tokens, "cache_write_tokens": tokens.cache_write_tokens,
            }));
        }
        let reported = cost.reported_charge();
        let settled = if let Some(hold) = self.hold.take() {
            let charged = cost.charged_usd.unwrap_or_default();
            let over = cost.over_reservation;
            self.charge = Some(Box::new(move || {
                hold.settle(charged, over);
            }));
            Some(charged)
        } else {
            if let Some(charge) = reported
                && let Some(guard) = self.guard.take()
            {
                let source = source.to_owned();
                self.charge = Some(Box::new(move || guard.debit_reported(&source, charge)));
            }
            reported
        };
        if let Some(charged) = settled {
            self.provider_charge_usd = reported;
            let mut fields = json!({"partial": partial, "charged_usd": charged});
            if reported.is_some() {
                fields["basis"] = json!("provider_reported");
            } else if reservation_estimate {
                fields["basis"] = json!("reservation_estimate");
            }
            self.child("settlement", now, fields);
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

    pub fn saw_output(&self) -> bool {
        self.saw_output
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
        let path = self.path.take();
        let charge = self.charge.take();
        if path.is_some() || charge.is_some() {
            let rows = std::mem::take(&mut self.rows);
            self.write = Some(tokio::task::spawn_blocking(move || {
                if let Some(charge) = charge {
                    charge();
                }
                if let Some(path) = path
                    && let Err(err) = append(&path, &rows)
                {
                    crate::logf!("gateway trace append failed: {err}");
                }
            }));
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
mod tests {
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

    #[tokio::test]
    async fn trace_flush_waits_for_the_charge_on_a_blocking_worker()
    -> Result<(), Box<dyn std::error::Error>> {
        let current = std::thread::current().id();
        let (send, mut receive) = tokio::sync::oneshot::channel();
        let mut trace = Exchange::new(None, Stamp::now(), &Request::default());
        trace.charge = Some(Box::new(move || {
            let _ = send.send(std::thread::current().id());
        }));
        trace.finish("completed", None);
        trace.flush().await;
        assert_ne!(receive.try_recv()?, current);
        Ok(())
    }
}
