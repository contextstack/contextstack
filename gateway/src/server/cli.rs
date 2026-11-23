//! CLI work orders: `POST /v1/agent/cli.run` (unary, or live output with `stream: true`) and
//! `POST /v1/agent/cli.run/{execution_id}/cancel`.
//!
//! Who may call: both routes take the same rule. Bearer auth when the gateway has a token (as every
//! route), and a loopback TCP peer (never the unix socket). A run started with an
//! `X-ContextStack-Caller` header is cancelled only by a request carrying the same value.

use super::{Body, Peer, error, full, json, text_error};
use crate::agentstream::{Dialect, Mapper};
use crate::clirun::Cancel;
use crate::errors::WireError;
use crate::gateway::Gateway;
use crate::workorder::{self, Executor, HttpReply};
use bytes::Bytes;
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::body::Frame;
use hyper::{Request, Response, body::Incoming};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

/// The chunk family of a streamed work order.
pub const CHUNK: &str = "agent.gateway.cli_run.chunk.v1";
/// The response header naming a run's execution id.
const ID_HEADER: &str = "x-contextstack-execution-id";

fn local(peer: Peer) -> bool {
    matches!(peer, Peer::Tcp(ip) if ip.is_loopback())
}

fn chunk(id: &str, index: i64, kind: &str, extra: Value) -> String {
    let mut c = json!({"schema_version": CHUNK, "execution_id": id, "index": index, "kind": kind});
    if let (Some(c), Value::Object(extra)) = (c.as_object_mut(), extra) {
        c.extend(extra);
    }
    format!("data: {c}\n\n")
}

fn sse(body: Body) -> Response<Body> {
    let mut r = Response::new(body);
    let h = r.headers_mut();
    for (k, v) in [
        (hyper::header::CONTENT_TYPE, "text/event-stream"),
        (hyper::header::CACHE_CONTROL, "no-cache"),
        (hyper::header::CONNECTION, "keep-alive"),
    ] {
        h.insert(k, hyper::header::HeaderValue::from_static(v));
    }
    r
}

/// POST /v1/agent/cli.run.
pub async fn execute(
    gw: &Arc<Gateway>,
    peer: Peer,
    caller: &str,
    req: Request<Incoming>,
) -> Response<Body> {
    let Some(executor) = &gw.executor else {
        return super::refuse(
            req,
            error(503, "CLI executor not configured", "not_configured"),
        )
        .await;
    };
    if !local(peer) {
        return super::refuse(req, text_error(403, "CLI execution is local only")).await;
    }
    let body = match Limited::new(req.into_body(), workorder::MAX_BODY)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(_) => return text_error(400, "invalid work order"),
    };
    let parsed = match workorder::parse(&body) {
        Ok(p) => p,
        Err((status, _, text)) => return text_error(status, text.trim_end()),
    };
    if parsed.wants_receipt() && caller.is_empty() {
        return text_error(400, "CLI execution receipt requires X-ContextStack-Caller");
    }
    let dialect = if parsed.stream {
        let found = crate::agentstream::dialect(&parsed.executor, &gw.current().inventory());
        let Some(d) = found else {
            let e = WireError::fatal(
                "gateway.streaming_unsupported",
                &format!(
                    "{}: no streaming mode (claude, codex, gemini, cursor-agent and antigravity executors stream); call unary",
                    parsed.executor
                ),
            );
            let id = parsed.execution_id.as_deref().unwrap_or("");
            return sse(full(200, "", chunk(id, 0, "error", json!({"error": e}))).into_body());
        };
        Some(d)
    } else {
        None
    };
    let run = match gw
        .runs
        .register(parsed.execution_id.clone(), parsed.job(), caller)
    {
        Ok(h) => h,
        Err(id) => {
            return error(
                409,
                &format!("execution_id {id} is running"),
                "execution_in_use",
            );
        }
    };
    let id = run.id().to_string();
    let mut resp = if let Some(d) = dialect {
        stream(gw, executor.clone(), parsed, run, d)
    } else {
        let disconnect = CancellationToken::new();
        let _guard = disconnect.clone().drop_guard();
        let (status, content_type, text) =
            workorder::execute(&gw.admission, executor, parsed, run, disconnect, None).await;
        if content_type.starts_with("text/plain") {
            text_error(status, text.trim_end())
        } else {
            full(status, content_type, text)
        }
    };
    if let Ok(v) = hyper::header::HeaderValue::from_str(&id) {
        resp.headers_mut().insert(ID_HEADER, v);
    }
    resp
}

/// A streamed run: deltas from the agent's stdout lines, then the unary reply as a stop chunk's
/// `result`, or an error chunk when the unary reply would have been an error status.
struct Live {
    lines: mpsc::Receiver<Vec<u8>>,
    task: Option<tokio::task::JoinHandle<HttpReply>>,
    mapper: Mapper,
    id: String,
    index: i64,
    /// Dropping the body (the caller went away) stops the run.
    _guard: DropGuard,
}

impl Live {
    async fn next(&mut self) -> Option<String> {
        let task = self.task.as_mut()?;
        while let Some(line) = self.lines.recv().await {
            let text = self.mapper.text(&line);
            if !text.is_empty() {
                self.index += 1;
                return Some(chunk(
                    &self.id,
                    self.index - 1,
                    "delta",
                    json!({"content": text}),
                ));
            }
        }
        let (status, content_type, text) = task
            .await
            .unwrap_or_else(|e| (502, "text/plain", e.to_string()));
        self.task = None;
        if content_type == "application/json"
            && let Ok(result) = serde_json::from_str::<Value>(&text)
        {
            return Some(chunk(
                &self.id,
                self.index,
                "stop",
                json!({"result": result}),
            ));
        }
        let message = text.trim_end();
        let e = if status == 503 {
            WireError::retryable("gateway.capacity_exhausted", message)
        } else {
            WireError::runtime("gateway.transport_failed", message)
        };
        Some(chunk(
            &self.id,
            self.index,
            "error",
            json!({"status": status, "error": e}),
        ))
    }
}

fn stream(
    gw: &Arc<Gateway>,
    executor: Executor,
    req: workorder::Request,
    run: crate::clirun::Handle,
    dialect: Dialect,
) -> Response<Body> {
    let id = run.id().to_string();
    let (tx, rx) = mpsc::channel(32);
    let disconnect = CancellationToken::new();
    let guard = disconnect.clone().drop_guard();
    let manager = Arc::clone(&gw.admission);
    let task = tokio::spawn(async move {
        workorder::execute(&manager, &executor, req, run, disconnect, Some(tx)).await
    });
    let live = Live {
        lines: rx,
        task: Some(task),
        mapper: Mapper::new(dialect),
        id,
        index: 0,
        _guard: guard,
    };
    let frames = futures_util::stream::unfold(live, |mut s| async move {
        let f = s.next().await?;
        Some((Ok::<_, Infallible>(Frame::data(Bytes::from(f))), s))
    });
    sse(BodyExt::boxed_unsync(StreamBody::new(frames)))
}

/// The most a cancel body (`{"reason": text}`, optional) may be.
const MAX_CANCEL_BODY: usize = 64 * 1024;

/// POST /v1/agent/cli.run/{id}/cancel, where `id` is an execution id or a work order's `CS_JOB_ID`:
/// stop each matching run's process group, release its permit, and answer once all are done. The
/// original call then ends with `outcome: "cancelled"`. `cancelled` says whether anything was stopped.
pub async fn cancel(
    gw: &Arc<Gateway>,
    peer: Peer,
    caller: &str,
    id: &str,
    req: Request<Incoming>,
) -> Response<Body> {
    if gw.executor.is_none() {
        return super::refuse(
            req,
            error(503, "CLI executor not configured", "not_configured"),
        )
        .await;
    }
    if !local(peer) {
        return super::refuse(req, text_error(403, "CLI execution is local only")).await;
    }
    let reason = match Limited::new(req.into_body(), MAX_CANCEL_BODY)
        .collect()
        .await
    {
        Ok(b) => serde_json::from_slice::<Value>(&b.to_bytes())
            .ok()
            .and_then(|v| v["reason"].as_str().map(str::to_string))
            .unwrap_or_default(),
        Err(_) => {
            return error(
                413,
                "cancel body over 64 KiB or unreadable",
                "invalid_cancel",
            );
        }
    };
    let found = if crate::clirun::valid_id(id) {
        gw.runs.cancel(id, caller).await
    } else {
        Cancel::Unknown
    };
    let (status, body) = match found {
        Cancel::Cancelled => (
            200,
            json!({"execution_id": id, "cancelled": true, "result": "cancelled"}),
        ),
        Cancel::AlreadyFinished(o) => (
            200,
            json!({"execution_id": id, "cancelled": false, "result": "already_finished", "outcome": o.as_str()}),
        ),
        Cancel::Cancelling => (
            202,
            json!({"execution_id": id, "cancelled": true, "result": "cancelling"}),
        ),
        Cancel::Unknown => (
            404,
            json!({"error": "unknown execution_id", "code": "unknown_execution"}),
        ),
        Cancel::NotOwner => (
            403,
            json!({"error": "execution started by another caller (X-ContextStack-Caller)", "code": "not_owner"}),
        ),
    };
    crate::log::row(&json!({
        "event": "cli_cancel", "execution_id": id, "status": status,
        "result": body.get("result").or_else(|| body.get("code")), "reason": reason,
    }));
    json(status, &body)
}
