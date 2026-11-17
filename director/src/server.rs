//! HTTP: listeners (plain or TLS), the transport and origin rules, and routing to the node API and
//! the client API. Credentials are checked from the headers before the body is read; the loopback UI
//! uses paired browser sessions.

use crate::access;
use crate::settings::Settings;
use crate::store::Store;
use bytes::Bytes;
use http_body_util::{BodyExt, Either, Full, LengthLimitError, Limited};
use hyper::{HeaderMap, Request, Response, StatusCode, body::Incoming};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Object uploads and envelope posts: evidence and bundles come whole.
const MAX_BODY: usize = 256 * 1024 * 1024;
/// Every route that is not an object upload or an envelope post.
const NORMAL_BODY: usize = 1024 * 1024;
/// The loopback UI sends small JSON. Kept tighter than [`NORMAL_BODY`].
const UI_BODY: usize = 256 * 1024;
/// How much of a refused request to read so a client that already sent a small body still gets the refusal.
/// A declared length above this is not read: an unauthenticated multi-megabyte post must not be buffered.
const REFUSED_BODY_MAX: usize = 256 * 1024;
/// How long shutdown waits for in-flight requests.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// A response: whole, or an event stream.
pub type Reply = Response<Either<Full<Bytes>, crate::events::Stream>>;

/// What every request handler shares.
pub struct State {
    /// The data directory.
    pub store: Store,
    /// The serve settings.
    pub settings: Settings,
    /// The live-update publisher.
    pub hub: crate::events::Hub,
}

/// Where a connection came from and how.
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    /// The remote address.
    pub ip: IpAddr,
    /// Whether the listener serves TLS.
    pub tls: bool,
    /// The actual bound listener address (UI is confined to loopback listeners).
    pub local: std::net::SocketAddr,
}

/// A request, read whole.
#[derive(Clone)]
pub struct Call {
    /// The method.
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// The query string.
    pub query: String,
    /// The headers.
    pub headers: HeaderMap,
    /// The body.
    pub body: Bytes,
}

impl Call {
    /// A header as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The bearer token, when well formed.
    pub fn bearer(&self) -> Option<&str> {
        access::bearer(self.header("authorization"))
    }

    /// One query parameter (no percent-decoding: values here are plain names).
    pub fn query(&self, key: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }
}

/// A JSON response.
pub fn json(status: u16, value: &Value) -> Reply {
    bytes(status, "application/json", Bytes::from(value.to_string()))
}

/// A response with raw bytes.
pub fn bytes(status: u16, content_type: &str, body: Bytes) -> Reply {
    let mut r = Response::new(Either::Left(Full::new(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(v) = content_type.parse() {
        r.headers_mut().insert(hyper::header::CONTENT_TYPE, v);
    }
    r
}

/// A refusal: `{error, code, detail}` plus any extra fields (NODE-PROTOCOL.md §3).
pub fn refusal(status: u16, code: &str, detail: &str, extra: Value) -> Reply {
    let reason = StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Error");
    let mut body = json!({"error": reason, "code": code, "detail": detail});
    if let (Some(b), Value::Object(more)) = (body.as_object_mut(), extra) {
        b.extend(more);
    }
    json(status, &body)
}

/// A 500 for a store failure, logged.
pub fn internal(error: &impl std::fmt::Display) -> Reply {
    crate::logf!("internal error: {error}");
    refusal(
        500,
        "internal",
        "the Director could not complete the request",
        Value::Null,
    )
}

/// Serve one request: transport rule, origin rule, then the route.
async fn handle(
    state: Arc<State>,
    peer: Peer,
    req: Request<Incoming>,
) -> Result<Reply, Infallible> {
    let started = std::time::Instant::now();
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str().to_string();
    let path = parts.uri.path().to_string();
    let is_ui = crate::ui::is_path(&path);
    let mut reply = if is_ui && !crate::ui::admit(peer, &parts.headers) {
        refusal(
            403,
            "ui_origin_refused",
            "the UI requires its loopback origin",
            Value::Null,
        )
    } else if !access::admit(peer.ip, peer.tls) {
        refusal(
            403,
            "transport_refused",
            "plain HTTP from another machine is refused: connect over Tailscale or configure TLS",
            Value::Null,
        )
    } else if !is_ui
        && access::foreign_origin(
            parts.headers.get("origin").and_then(|v| v.to_str().ok()),
            parts
                .headers
                .get("sec-fetch-site")
                .and_then(|v| v.to_str().ok()),
            &state.settings.allowed_origins,
        )
    {
        refusal(
            403,
            "origin_refused",
            "browser requests from this origin are not accepted",
            Value::Null,
        )
    } else if let Some(denied) = unauthenticated(&state, &method, &path, &parts.headers) {
        discard_small(body, &parts.headers).await;
        denied
    } else {
        match Limited::new(body, body_limit(&method, &path))
            .collect()
            .await
        {
            Ok(b) => {
                let call = Call {
                    method: method.clone(),
                    path: path.clone(),
                    query: parts.uri.query().unwrap_or("").to_string(),
                    headers: parts.headers,
                    body: b.to_bytes(),
                };
                let reply = if is_ui {
                    crate::ui::handle(&state, &call, peer)
                } else {
                    route(&state, &call).await
                };
                state.hub.publish(&state.store);
                reply
            }
            Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => refusal(
                413,
                "payload_too_large",
                "the request body is too large for this route",
                Value::Null,
            ),
            Err(e) => refusal(
                422,
                "invalid_request",
                &format!("request body: {e}"),
                Value::Null,
            ),
        }
    };
    if is_ui {
        crate::ui::stamp(&mut reply);
    }
    if path.starts_with(crate::node_api::PREFIX) {
        crate::node_api::stamp(&mut reply, &state.store.installation);
    }
    crate::logf!(
        "{} {method} {path} {} {}ms",
        peer.ip,
        reply.status().as_u16(),
        started.elapsed().as_millis()
    );
    Ok(reply)
}

async fn route(state: &Arc<State>, call: &Call) -> Reply {
    if let Some(rest) = call.path.strip_prefix(crate::node_api::PREFIX) {
        return crate::node_api::handle(state, call, rest).await;
    }
    let state = Arc::clone(state);
    let call = call.clone();
    match tokio::task::spawn_blocking(move || crate::client_api::handle(&state, &call)).await {
        Ok(reply) => reply,
        Err(e) => internal(&format!("request handler: {e}")),
    }
}

/// `Some` when this route requires a credential and the headers do not carry a live one.
fn unauthenticated(state: &State, method: &str, path: &str, headers: &HeaderMap) -> Option<Reply> {
    if crate::ui::is_path(path) {
        if crate::ui::open(method, path) {
            return None;
        }
        return match crate::ui::session_live(&state.store, headers) {
            Ok(true) => None,
            Ok(false) => Some(refusal(
                401,
                "ui_login_required",
                "ui_login_required",
                Value::Null,
            )),
            Err(e) => Some(internal(&e)),
        };
    }
    if method == "POST" && path == "/api/v1/pair" {
        return None;
    }
    if path.starts_with(crate::node_api::PREFIX) {
        return node_token(state, headers);
    }
    client_token(state, headers)
}

fn authorization(headers: &HeaderMap) -> Option<&str> {
    access::bearer(headers.get("authorization").and_then(|v| v.to_str().ok()))
}

fn node_token(state: &State, headers: &HeaderMap) -> Option<Reply> {
    let Some(token) = authorization(headers) else {
        return Some(refusal(
            401,
            "unauthenticated",
            "a node token is required",
            Value::Null,
        ));
    };
    match access::node(&state.store, token) {
        Ok(Some(_)) => None,
        Ok(None) => Some(refusal(
            401,
            "unauthenticated",
            "unknown or revoked node token",
            Value::Null,
        )),
        Err(e) => Some(internal(&e)),
    }
}

fn client_token(state: &State, headers: &HeaderMap) -> Option<Reply> {
    let Some(token) = authorization(headers) else {
        return Some(refusal(
            401,
            "unauthenticated",
            "a client credential is required",
            Value::Null,
        ));
    };
    match access::client(&state.store, token) {
        Ok(Some(_)) => None,
        Ok(None) => Some(refusal(
            401,
            "unauthenticated",
            "a client credential is required",
            Value::Null,
        )),
        Err(e) => Some(internal(&e)),
    }
}

/// 256 MiB for `PUT …/objects/{sha}` and `POST …/envelopes`; 256 KiB for the UI; 1 MiB otherwise.
fn body_limit(method: &str, path: &str) -> usize {
    if crate::ui::is_path(path) {
        UI_BODY
    } else if object_upload(method, path) || envelope_post(method, path) {
        MAX_BODY
    } else {
        NORMAL_BODY
    }
}

fn object_upload(method: &str, path: &str) -> bool {
    if method != "PUT" {
        return false;
    }
    let Some((dir, sha)) = path.rsplit_once('/') else {
        return false;
    };
    dir.ends_with("/objects") && crate::util::is_sha256(sha)
}

fn envelope_post(method: &str, path: &str) -> bool {
    method == "POST" && path.strip_prefix(crate::node_api::PREFIX) == Some("envelopes")
}

/// Read a small refused body so the client receives the status. A declared length above
/// [`REFUSED_BODY_MAX`] is left unread.
async fn discard_small(body: Incoming, headers: &HeaderMap) {
    let large = headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > REFUSED_BODY_MAX);
    if large {
        return;
    }
    let read = Limited::new(body, REFUSED_BODY_MAX).collect();
    let _ = tokio::time::timeout(Duration::from_secs(5), read).await;
}

/// Load the TLS certificate chain and key.
fn tls_acceptor(
    cert: &std::path::Path,
    key: &std::path::Path,
) -> Result<tokio_rustls::TlsAcceptor, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| format!("tls cert {}: {e}", cert.display()))?;
    let key =
        PrivateKeyDer::from_pem_file(key).map_err(|e| format!("tls key {}: {e}", key.display()))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("tls: {e}"))?
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .map_err(|e| format!("tls: {e}"))?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Bind every listen address, serve until SIGTERM or SIGINT, then drain.
pub async fn serve(state: Arc<State>) -> Result<(), String> {
    let tls = match &state.settings.tls {
        Some((cert, key)) => Some(tls_acceptor(cert, key)?),
        None => None,
    };
    let stop = CancellationToken::new();
    let graceful = Arc::new(GracefulShutdown::new());
    let mut accepting = Vec::new();
    for addr in &state.settings.listen {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("listen {addr}: {e}"))?;
        let bound = listener.local_addr().map_err(|e| e.to_string())?;
        crate::logf!(
            "listening on {}://{bound} (installation {})",
            if tls.is_some() { "https" } else { "http" },
            state.store.installation
        );
        if !bound.ip().is_loopback() && tls.is_none() {
            crate::logf!(
                "{bound} is not loopback: only Tailscale peers (100.64.0.0/10) will be served without TLS"
            );
        }
        accepting.push(tokio::spawn(accept(
            listener,
            Arc::clone(&state),
            tls.clone(),
            Arc::clone(&graceful),
            stop.clone(),
        )));
    }
    let ticker = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            // Changes made by the administrative commands reach the streams within a second, and held
            // reservations past their bounds close (CLIENT-API.md §12.3).
            let mut every = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    () = state.hub.stop.cancelled() => return,
                    _ = every.tick() => {
                        if let Err(e) = crate::reservations::sweep(&state.store, state.settings.reservations) {
                            crate::logf!("budgets: closing reservations past their bounds: {e}");
                        }
                        state.hub.publish(&state.store);
                    }
                }
            }
        })
    };
    signals().await?;
    stop.cancel();
    state.hub.stop.cancel();
    let _ = ticker.await;
    for task in accepting {
        let _ = task.await;
    }
    if let Ok(g) = Arc::try_unwrap(graceful) {
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, g.shutdown()).await;
    }
    crate::util::log("stopped");
    Ok(())
}

async fn signals() -> Result<(), String> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    tokio::select! {
        _ = term.recv() => crate::util::log("SIGTERM received, shutting down"),
        _ = int.recv() => crate::util::log("SIGINT received, shutting down"),
    }
    Ok(())
}

async fn accept(
    listener: TcpListener,
    state: Arc<State>,
    tls: Option<tokio_rustls::TlsAcceptor>,
    graceful: Arc<GracefulShutdown>,
    stop: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            () = stop.cancelled() => return,
        };
        let Ok((stream, addr)) = accepted else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let Ok(local) = listener.local_addr() else {
            continue;
        };
        let peer = Peer {
            ip: addr.ip(),
            tls: tls.is_some(),
            local,
        };
        match &tls {
            None => serve_connection(TokioIo::new(stream), Arc::clone(&state), peer, &graceful),
            Some(acceptor) => {
                let acceptor = acceptor.clone();
                let state = Arc::clone(&state);
                let graceful = Arc::clone(&graceful);
                tokio::spawn(async move {
                    match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream))
                        .await
                    {
                        Ok(Ok(s)) => serve_connection(TokioIo::new(s), state, peer, &graceful),
                        _ => crate::logf!("{}: TLS handshake failed", addr),
                    }
                });
            }
        }
    }
}

fn serve_connection<I>(io: I, state: Arc<State>, peer: Peer, graceful: &GracefulShutdown)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req| handle(Arc::clone(&state), peer, req));
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30));
    let conn = graceful.watch(builder.serve_connection(io, svc));
    tokio::spawn(async move {
        let _ = conn.await;
    });
}
