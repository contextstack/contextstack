//! HTTP: one handler served on TCP and, optionally, a unix socket. Bearer auth and
//! admission apply identically on both; loopback-origin routes (cli.run) refuse over the socket,
//! which has no peer IP.

mod cli;
mod model_call;
mod routes;
mod typed;

use crate::gateway::Gateway;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited, combinators::UnsyncBoxBody};
use hyper::{Request, Response, StatusCode, body::Incoming};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;

/// A response body: full or streamed.
pub type Body = UnsyncBoxBody<Bytes, Infallible>;

/// Where a connection came from.
#[derive(Debug, Clone, Copy)]
pub enum Peer {
    /// TCP, with the peer address.
    Tcp(IpAddr),
    /// The unix socket.
    Unix,
}

/// The current route inventory: (path, method), a path once per method. GET also answers HEAD.
const ROUTES: [(&str, &str); 14] = [
    ("/v1/admission", "GET"),
    ("/v1/budgets", "GET"),
    ("/v1/budgets", "PUT"),
    ("/v1/health", "GET"),
    ("/v1/capabilities", "GET"),
    ("/v1/agent/model.call", "POST"),
    ("/v1/cs/model.call", "POST"),
    ("/v1/models", "GET"),
    ("/v1/models/cli", "GET"),
    ("/v1/inventory/refresh", "POST"),
    ("/v1/discovery/health", "GET"),
    ("/v1/catalog", "GET"),
    ("/v1/agent/cli.run", "POST"),
    ("/v1/ai/typed-question", "POST"),
];

/// A full response.
pub fn full(status: u16, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    let mut r = Response::new(Full::new(body.into()).boxed_unsync());
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(v) = content_type.parse() {
        r.headers_mut().insert(hyper::header::CONTENT_TYPE, v);
    }
    r
}

/// A JSON response (one line, newline-terminated).
pub fn json(status: u16, value: &impl serde::Serialize) -> Response<Body> {
    let mut text = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    text.push('\n');
    full(status, "application/json", text)
}

/// `{"error": msg, "code": code}`.
pub fn error(status: u16, msg: &str, code: &str) -> Response<Body> {
    let mut v = serde_json::json!({"error": msg});
    if !code.is_empty() {
        v["code"] = serde_json::json!(code);
    }
    json(status, &v)
}

/// A plain-text error (Go's `http.Error`).
/// The most of a refused request's body read before answering: Go net/http's `maxPostHandlerReadBytes`.
const REFUSED_BODY_MAX: usize = 256 * 1024;
/// Answer `resp` to a request refused before its body was read, reading that body first (at most
/// 256 KiB, for at most 5 s). hyper closes a connection whose body is unread, so a client still
/// sending it (Python's http.client writes headers and body separately) got a broken pipe instead of
/// the refusal whenever the gateway answered first, as under load. Go's net/http drains the same.
async fn refuse(req: Request<Incoming>, resp: Response<Body>) -> Response<Body> {
    let small = req
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<usize>().ok())
        .is_none_or(|n| n <= REFUSED_BODY_MAX);
    if small {
        let read = Limited::new(req.into_body(), REFUSED_BODY_MAX).collect();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), read).await;
    }
    resp
}

pub fn text_error(status: u16, msg: &str) -> Response<Body> {
    let mut r = full(status, "text/plain; charset=utf-8", format!("{msg}\n"));
    r.headers_mut().insert(
        "x-content-type-options",
        hyper::header::HeaderValue::from_static("nosniff"),
    );
    r
}

/// An OS error in the lower-case form logs and operators have always seen ("no such file or directory").
pub fn os_error(e: &std::io::Error) -> String {
    let s = e.to_string();
    let s = s.split(" (os error").next().unwrap_or(&s).to_string();
    let mut c = s.chars();
    c.next()
        .map_or(s.clone(), |f| f.to_lowercase().chain(c).collect())
}

/// Serve one request.
pub async fn handle(
    gw: Arc<Gateway>,
    peer: Peer,
    req: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let started = std::time::Instant::now();
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let resp = route(&gw, peer, req).await;
    crate::logf!(
        "{method} {path} {} {}",
        resp.status().as_u16(),
        crate::log::format_duration(std::time::Duration::from_millis(
            u64::try_from(started.elapsed().as_millis()).unwrap_or(0)
        ))
    );
    Ok(resp)
}

async fn route(gw: &Arc<Gateway>, peer: Peer, req: Request<Incoming>) -> Response<Body> {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let token = &gw.settings.auth_token;
    let public = method == "GET" && (path == "/v1/health" || path == "/v1/capabilities");
    if !token.is_empty() && !public {
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // Compare the secret itself, not a freshly formatted `Bearer …` string.
        let presented = auth.strip_prefix("Bearer ").unwrap_or("");
        if !crate::spend::constant_time_eq(token.as_bytes(), presented.as_bytes()) {
            return refuse(req, error(401, "unauthorized", "auth_required")).await;
        }
    }
    let caller = req
        .headers()
        .get("x-contextstack-caller")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if let Some(id) = path
        .strip_prefix("/v1/agent/cli.run/")
        .and_then(|r| r.strip_suffix("/cancel"))
    {
        if method != "POST" {
            let mut r = full(405, "text/plain; charset=utf-8", "Method Not Allowed\n");
            r.headers_mut()
                .insert("allow", hyper::header::HeaderValue::from_static("POST"));
            return refuse(req, r).await;
        }
        let id = id.to_string();
        return cli::cancel(gw, peer, &caller, &id, req).await;
    }
    let wants: Vec<&str> = ROUTES
        .iter()
        .filter(|(p, _)| *p == path)
        .map(|(_, m)| *m)
        .collect();
    if wants.is_empty() {
        return refuse(
            req,
            full(404, "text/plain; charset=utf-8", "404 page not found\n"),
        )
        .await;
    }
    if !wants
        .iter()
        .any(|w| method == *w || (*w == "GET" && method == "HEAD"))
    {
        let mut r = full(405, "text/plain; charset=utf-8", "Method Not Allowed\n");
        let allow = wants
            .iter()
            .map(|w| if *w == "GET" { "GET, HEAD" } else { w })
            .collect::<Vec<_>>()
            .join(", ");
        if let Ok(v) = hyper::header::HeaderValue::from_str(&allow) {
            r.headers_mut().insert("allow", v);
        }
        return refuse(req, r).await;
    }
    let query = parse_query(req.uri().query().unwrap_or(""));
    match path.as_str() {
        "/v1/agent/model.call" | "/v1/cs/model.call" => {
            model_call::receive(gw, &path, &caller, req.into_body()).await
        }
        "/v1/agent/cli.run" => cli::execute(gw, peer, &caller, req).await,
        "/v1/ai/typed-question" => typed::handle(gw, req).await,
        "/v1/health" => routes::health(gw).await,
        "/v1/capabilities" => routes::capabilities(gw),
        "/v1/models" => routes::models(gw, &query),
        "/v1/models/cli" => routes::models_cli(gw, &query),
        "/v1/inventory/refresh" => routes::refresh(gw).await,
        "/v1/discovery/health" => json(200, &gw.health.report(chrono::Utc::now())),
        "/v1/catalog" => json(200, &gw.catalog_status()),
        "/v1/admission" => json(200, &gw.admission.report()),
        "/v1/budgets" if method == "PUT" => routes::put_budgets(gw, peer, req).await,
        "/v1/budgets" => routes::budgets(gw).await,
        _ => full(404, "text/plain; charset=utf-8", "404 page not found\n"),
    }
}

fn parse_query(q: &str) -> BTreeMap<String, String> {
    let decode = |s: &str| {
        let s = s.replace('+', " ");
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    let mut out = BTreeMap::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.entry(decode(k)).or_insert_with(|| decode(v));
    }
    out
}

/// Accepted-request counts for the v1 → v2 contraction gate (WIRE.md Migration).
#[derive(Debug, Default)]
pub struct WireUsage {
    by_schema: BTreeMap<String, i64>,
    by_caller: BTreeMap<String, BTreeMap<String, i64>>,
    consecutive_v2: i64,
}

/// v2 requests in a row before v1 may retire.
const THRESHOLD: i64 = 500;

impl WireUsage {
    /// Count one accepted request.
    pub fn record(&mut self, schema: &str, caller: &str) {
        *self.by_schema.entry(schema.into()).or_default() += 1;
        *self
            .by_caller
            .entry(caller.into())
            .or_default()
            .entry(schema.into())
            .or_default() += 1;
        if schema == crate::wire::REQUEST_V1 {
            self.consecutive_v2 = 0;
        } else {
            self.consecutive_v2 += 1;
        }
    }

    /// /v1/health's `wire_cutover`.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "totals_by_schema": self.by_schema,
            "totals_by_caller_schema": self.by_caller,
            "consecutive_v2": self.consecutive_v2,
            "threshold": THRESHOLD,
            "ready": self.consecutive_v2 >= THRESHOLD,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_errors() {
        let q = parse_query("tier=2&capability=gen%20eral&available_only=false");
        assert_eq!(q.get("capability").map(String::as_str), Some("gen eral"));
        assert_eq!(q.get("tier").map(String::as_str), Some("2"));
        let e = std::io::Error::from_raw_os_error(2);
        assert_eq!(os_error(&e), "no such file or directory");
    }

    #[test]
    fn wire_counts_reset_on_v1() {
        let mut w = WireUsage::default();
        w.record(crate::wire::REQUEST_V2, "a");
        w.record(crate::wire::REQUEST_V1, "b");
        w.record(crate::wire::REQUEST_V2, "a");
        let s = w.snapshot();
        assert_eq!(s["consecutive_v2"], 1);
        assert_eq!(
            s["totals_by_caller_schema"]["a"][crate::wire::REQUEST_V2],
            2
        );
    }
}
