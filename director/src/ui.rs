//! Embedded newcomer UI and its loopback-only, cookie-authenticated Client API adapter (ADR 0022).
use crate::access::Client;
use crate::policy::{self, ui_route};
use crate::server::{Call, Peer, Reply, State, bytes, internal, json, refusal};
use crate::settings::{Args, DEFAULT_LISTEN};
use crate::store::Store;
use bytes::Bytes;
use hyper::HeaderMap;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'";

/// A fragment keeps the one-time code out of HTTP paths, logs and referrers.
pub fn link(store: &Store, args: &Args) -> Result<String, String> {
    let listen = args
        .one("listen", "DIRECTOR_LISTEN")
        .unwrap_or_else(|| DEFAULT_LISTEN.into());
    let addr: SocketAddr = listen
        .parse()
        .map_err(|_| "ui-link --listen needs a numeric loopback address and port")?;
    if !addr.ip().is_loopback() || addr.port() == 0 {
        return Err("ui-link requires a loopback address and a nonzero port".into());
    }
    let code = crate::ui_auth::issue(store)?;
    Ok(format!("http://{addr}/ui/#code={code}"))
}

pub fn is_path(path: &str) -> bool {
    path == "/ui" || path.starts_with("/ui/")
}

/// Routes that answer before a browser session exists: the static assets and the one-time code exchange.
pub fn open(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        (
            "GET",
            "/ui" | "/ui/" | "/ui/app.js" | "/ui/markdown.js" | "/ui/style.css"
        ) | ("POST", "/ui/pair")
    )
}

/// Whether the `cs_ui` cookie names a live session. Checked from the headers, before the body is read.
pub fn session_live(store: &Store, headers: &HeaderMap) -> Result<bool, String> {
    let Some(token) = cookie_token(headers) else {
        return Ok(false);
    };
    Ok(crate::ui_auth::authenticate(store, token)?.is_some())
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())?
        .split(';')
        .filter_map(|v| v.trim().split_once('='))
        .find(|(k, _)| *k == "cs_ui")
        .map(|(_, v)| v)
}

/// Reject remote listeners, DNS rebinding and cross-origin requests even when API CORS allows them.
pub fn admit(peer: Peer, headers: &HeaderMap) -> bool {
    if !peer.ip.is_loopback() || !peer.local.ip().is_loopback() {
        return false;
    }
    let Some(host) = headers.get("host").and_then(|h| h.to_str().ok()) else {
        return false;
    };
    if host != peer.local.to_string() {
        return false;
    }
    let origin = format!("{}://{host}", if peer.tls { "https" } else { "http" });
    if headers
        .get("origin")
        .is_some_and(|o| o.as_bytes() != origin.as_bytes())
    {
        return false;
    }
    !headers
        .get("sec-fetch-site")
        .is_some_and(|v| !matches!(v.to_str(), Ok("same-origin" | "none")))
}

/// Apply to every UI response, including refusals and static assets.
pub fn stamp(reply: &mut Reply) {
    for (name, value) in [
        ("content-security-policy", CSP),
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
    ] {
        reply
            .headers_mut()
            .insert(name, hyper::header::HeaderValue::from_static(value));
    }
}

fn denied(status: u16, code: &str) -> Reply {
    refusal(status, code, code, Value::Null)
}

pub fn handle(state: &Arc<State>, call: &Call, peer: Peer) -> Reply {
    match (call.method.as_str(), call.path.as_str()) {
        ("GET", "/ui") => {
            let mut r = bytes(302, "text/plain", Bytes::new());
            r.headers_mut()
                .insert("location", hyper::header::HeaderValue::from_static("/ui/"));
            return r;
        }
        ("GET", "/ui/") => {
            return bytes(
                200,
                "text/html; charset=utf-8",
                Bytes::from_static(include_str!("ui_assets/index.html").as_bytes()),
            );
        }
        ("GET", "/ui/app.js") => {
            return bytes(
                200,
                "text/javascript; charset=utf-8",
                Bytes::from_static(include_str!("ui_assets/app.js").as_bytes()),
            );
        }
        ("GET", "/ui/markdown.js") => {
            return bytes(
                200,
                "text/javascript; charset=utf-8",
                Bytes::from_static(include_str!("ui_assets/markdown.js").as_bytes()),
            );
        }
        ("GET", "/ui/style.css") => {
            return bytes(
                200,
                "text/css; charset=utf-8",
                Bytes::from_static(include_str!("ui_assets/style.css").as_bytes()),
            );
        }
        ("POST", "/ui/pair") => return pair(state, call, peer.tls),
        _ => {}
    }
    let Some(token) = cookie(call) else {
        return denied(401, "ui_login_required");
    };
    let _mutation = if call.method == "GET" {
        None
    } else {
        match state.store.lock_client_mutation() {
            Ok(lock) => Some(lock),
            Err(error) => return internal(&error),
        }
    };
    let (client, csrf) = match crate::ui_auth::authenticate(&state.store, token) {
        Ok(Some(session)) => session,
        Ok(None) => return denied(401, "ui_login_required"),
        Err(e) => return internal(&e),
    };
    if let Some(cap) = policy::ui_session_route(&call.method, &call.path)
        && !policy::allows(
            &client,
            cap,
            policy::Target::Org(&state.store.db().scope().org_id),
        )
    {
        return policy::denied();
    }
    if call.method == "GET" && call.path == "/ui/session" {
        return json(200, &json!({"csrf": csrf}));
    }
    if call.method != "GET" && call.header("x-csrf-token") != Some(csrf.as_str()) {
        return denied(403, "csrf_refused");
    }
    if call.method == "POST" && call.path == "/ui/logout" {
        if let Err(e) = crate::ui_auth::logout(&state.store, token) {
            return internal(&e);
        }
        let mut r = json(200, &json!({"logged_out": true}));
        let cleared = if peer.tls {
            "cs_ui=; Path=/ui; HttpOnly; SameSite=Strict; Max-Age=0; Secure"
        } else {
            "cs_ui=; Path=/ui; HttpOnly; SameSite=Strict; Max-Age=0"
        };
        r.headers_mut().insert(
            "set-cookie",
            hyper::header::HeaderValue::from_static(cleared),
        );
        return r;
    }
    let Some(rest) = call.path.strip_prefix("/ui/api/") else {
        return denied(404, "not_found");
    };
    if !ui_route(&call.method, rest) {
        return denied(404, "not_found");
    }
    forward(state, call, rest, token, &client)
}

fn forward(state: &Arc<State>, call: &Call, rest: &str, token: &str, client: &Client) -> Reply {
    if rest == "events" {
        let Some(conversation) = event_conversation(&call.query) else {
            return denied(400, "conversation_scope_required");
        };
        if let Err(no) = crate::policy::guard(state, call, client, &["events"]) {
            return no;
        }
        return crate::events::browser_stream(state, client, conversation, token);
    }
    let forwarded = Call {
        method: call.method.clone(),
        path: format!("/api/v1/client/{rest}"),
        query: call.query.clone(),
        headers: call.headers.clone(),
        body: call.body.clone(),
    };
    crate::client_api::handle_as(state, &forwarded, client)
}

// UI streams have exactly one scope; other keys and duplicate scopes fail closed.
fn event_conversation(query: &str) -> Option<&str> {
    let (key, value) = query.split_once('=')?;
    (key == "conversation" && !value.is_empty() && !value.contains(['&', '='])).then_some(value)
}

fn cookie(call: &Call) -> Option<&str> {
    cookie_token(&call.headers)
}

fn pair(state: &State, call: &Call, tls: bool) -> Reply {
    if call
        .header("content-type")
        .is_none_or(|v| v.split(';').next() != Some("application/json"))
    {
        return denied(415, "json_required");
    }
    let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
    let code = body["code"].as_str().unwrap_or("");
    match crate::ui_auth::exchange(&state.store, code) {
        Ok(Some((session, csrf))) => {
            let mut r = json(200, &json!({"csrf": csrf}));
            let cookie = format!(
                "cs_ui={session}; Path=/ui; HttpOnly; SameSite=Strict; Max-Age={}{}",
                crate::ui_auth::SESSION_TTL_SECONDS,
                if tls { "; Secure" } else { "" }
            );
            match cookie.parse() {
                Ok(v) => {
                    r.headers_mut().insert("set-cookie", v);
                    r
                }
                Err(e) => internal(&e),
            }
        }
        Ok(None) => denied(403, "pairing_refused"),
        Err(e) => internal(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> Peer {
        Peer {
            ip: std::net::Ipv4Addr::LOCALHOST.into(),
            tls: false,
            local: SocketAddr::from(([127, 0, 0, 1], 8780)),
        }
    }

    #[test]
    fn ui_requires_literal_loopback_origin() {
        let mut h = HeaderMap::new();
        h.insert(
            "host",
            hyper::header::HeaderValue::from_static("127.0.0.1:8780"),
        );
        assert!(admit(peer(), &h));
        h.insert(
            "origin",
            hyper::header::HeaderValue::from_static("http://127.0.0.1:8780"),
        );
        assert!(admit(peer(), &h));
        h.insert(
            "origin",
            hyper::header::HeaderValue::from_static("https://evil.test"),
        );
        assert!(!admit(peer(), &h));
        h.remove("origin");
        h.insert(
            "host",
            hyper::header::HeaderValue::from_static("evil.test:8780"),
        );
        assert!(!admit(peer(), &h));
    }

    #[test]
    fn ui_rejects_remote_and_wildcard_listeners() {
        let mut h = HeaderMap::new();
        h.insert(
            "host",
            hyper::header::HeaderValue::from_static("127.0.0.1:8780"),
        );
        let mut p = peer();
        p.ip = std::net::Ipv4Addr::new(100, 64, 0, 2).into();
        assert!(!admit(p, &h));
        p = peer();
        p.local = SocketAddr::from(([0, 0, 0, 0], 8780));
        assert!(!admit(p, &h));
    }

    #[test]
    fn ui_embeds_markdown_assets_and_strict_csp() {
        let html = include_str!("ui_assets/index.html");
        assert!(html.contains("/ui/markdown.js"));
        let md = include_str!("ui_assets/markdown.js");
        assert!(md.contains("renderMarkdown"));
        assert!(md.contains("textContent"));
        assert!(!CSP.contains("'unsafe-inline'"));
        assert!(!CSP.contains("'unsafe-eval'"));
        assert!(CSP.contains("connect-src 'self'"));
        assert_eq!(
            CSP,
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'"
        );
    }

    #[test]
    fn browser_adapter_requires_one_conversation_scope() {
        assert!(ui_route("GET", "events"));
        assert_eq!(event_conversation("conversation=c1"), Some("c1"));
        for query in [
            "",
            "conversation=",
            "node=n1",
            "conversation=c1&node=n1",
            "conversation=c1&conversation=c2",
        ] {
            assert_eq!(event_conversation(query), None, "{query}");
        }
    }

    #[test]
    fn markdown_renderer_node_checks() {
        use std::path::PathBuf;
        use std::process::Command;
        let script: PathBuf = [
            env!("CARGO_MANIFEST_DIR"),
            "tests",
            "markdown_renderer_test.js",
        ]
        .iter()
        .collect();
        let node = Command::new("node").arg(&script).status();
        let Ok(status) = node else {
            eprintln!("skip markdown_renderer_node_checks: node not available");
            return;
        };
        assert!(status.success(), "node {}", script.display());
    }

    #[test]
    fn browser_app_node_checks() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui_app_test.js");
        let Ok(status) = std::process::Command::new("node").arg(&script).status() else {
            eprintln!("skip browser_app_node_checks: node not available");
            return;
        };
        assert!(status.success(), "node {}", script.display());
    }

    #[test]
    fn browser_adapter_only_exposes_first_slice() {
        assert!(ui_route("GET", "conversations"));
        assert!(ui_route("POST", "conversations"));
        assert!(ui_route("GET", "models"));
        assert!(ui_route("POST", "conversations/c1/turns"));
        assert!(!ui_route("POST", "pairings"));
        assert!(!ui_route("DELETE", "credentials/owner"));
        assert!(!ui_route("GET", "objects/hash"));
    }
}
