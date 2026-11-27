//! The node API, protocols 1 and 2 (`tests/director-spec/NODE-PROTOCOL.md`): five calls under `/api/v1/nodes/me/`,
//! and protocol 2's six more (§9), which exist only for a request that names version 2.
//!
//! Order on every call: node token (401), protocol version (426), installation (421), then the route.
//! Every answer names this Director's installation and the versions it speaks; an answer whose request
//! version was accepted also names that version.

use crate::server::{Call, Reply, State, bytes, internal, json, refusal};
use crate::{access, control, envelope, fleet, jobs, live, util};
use hyper::header::HeaderValue;
use serde_json::{Value, json};
use std::sync::Arc;

/// Where the node routes live.
pub const PREFIX: &str = "/api/v1/nodes/me/";
/// The protocol versions this Director speaks, ascending.
pub const SUPPORTED: [u32; 2] = [1, 2];

const H_PROTOCOL: &str = "contextstack-protocol";
const H_SUPPORTED: &str = "contextstack-protocol-supported";
const H_INSTALLATION: &str = "contextstack-installation";

/// Add the headers every node-route answer carries (§2).
pub fn stamp(reply: &mut Reply, installation: &str) {
    let supported = SUPPORTED.map(|v| v.to_string()).join(",");
    for (name, value) in [
        (H_INSTALLATION, installation),
        (H_SUPPORTED, supported.as_str()),
    ] {
        if let Ok(v) = HeaderValue::from_str(value) {
            reply.headers_mut().insert(name, v);
        }
    }
}

/// A positive decimal integer of at most nine digits, as the node sent it.
fn version(sent: Option<&str>) -> Option<u32> {
    let s = sent?;
    let ok = !s.is_empty()
        && s.len() <= 9
        && !s.starts_with('0')
        && s.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| s.parse().ok()).flatten()
}

/// Serve one node call; `rest` is the path after [`PREFIX`].
pub async fn handle(state: &Arc<State>, call: &Call, rest: &str) -> Reply {
    let Some(token) = call.bearer() else {
        return refusal(
            401,
            "unauthenticated",
            "a node token is required",
            Value::Null,
        );
    };
    let principal = match access::node(&state.store, token) {
        Ok(Some(found)) => found,
        Ok(None) => {
            return refusal(
                401,
                "unauthenticated",
                "unknown or revoked node token",
                Value::Null,
            );
        }
        Err(e) => return internal(&e),
    };
    let node_id = principal.id;
    let name = principal.name;
    let sent = version(call.header(H_PROTOCOL));
    let Some(v) = sent.filter(|v| SUPPORTED.contains(v)) else {
        return refusal(
            426,
            "protocol_unsupported",
            &format!(
                "this Director speaks node protocol {}",
                SUPPORTED.map(|v| v.to_string()).join(", ")
            ),
            json!({"protocol": {"node": sent, "supported": SUPPORTED}}),
        );
    };
    let installation = &state.store.installation;
    let mut reply = if call.header(H_INSTALLATION) == Some(installation.as_str()) {
        let noted = {
            let db = state.store.db();
            debug_assert_eq!(db.scope().org_id, principal.registered_by.scope.org_id);
            fleet::note_protocol(&db, node_id, &name, v)
        };
        if let Err(e) = noted {
            return internal(&e);
        }
        let node = Node {
            id: node_id,
            name: &name,
            version: v,
        };
        if v >= 2 && call.method == "GET" && rest == "work" {
            live::work(state, call, node_id).await
        } else if v >= 2
            && call.method == "POST"
            && let ["jobs", job, "action-approvals", parts @ ..] =
                rest.split('/').collect::<Vec<_>>().as_slice()
        {
            crate::action_approvals::node(state, call, node_id, job, parts)
        } else if v >= 2 && crate::policy::node_land_route(&call.method, rest) {
            crate::lead_land::node(state, call, node_id, rest)
        } else if v >= 2 && crate::policy::node_release_route(&call.method, rest) {
            crate::lead_install::node(state, call, node_id, rest)
        } else if v >= 2 && crate::policy::node_browser_action_route(&call.method, rest) {
            crate::action_approvals::browser_jobs(state, node_id)
        } else {
            route(state, call, &node, rest)
        }
    } else {
        refusal(
            421,
            "installation_mismatch",
            "this node is configured for another Director; set the installation printed at registration",
            json!({"installation": installation}),
        )
    };
    if let Ok(value) = HeaderValue::from_str(&v.to_string()) {
        reply.headers_mut().insert(H_PROTOCOL, value);
    }
    reply
}

fn not_found() -> Reply {
    refusal(
        404,
        "not_found",
        "no such node route, job or object for this node",
        Value::Null,
    )
}

/// The node a call is from, and the version it spoke.
struct Node<'a> {
    id: i64,
    name: &'a str,
    version: u32,
}

fn route(state: &State, call: &Call, node: &Node, rest: &str) -> Reply {
    let parts: Vec<&str> = rest.split('/').collect();
    let node_id = node.id;
    if node.version >= 2
        && let Some(reply) = route_v2(state, call, node, &parts)
    {
        return reply;
    }
    match (call.method.as_str(), parts.as_slice()) {
        ("GET", ["jobs"]) => match jobs::for_node(&state.store.db(), node_id) {
            Ok(list) => json(200, &json!({"jobs": list})),
            Err(e) => internal(&e),
        },
        ("POST", ["jobs", job_id, "accept"]) => {
            match jobs::accept(&state.store.db(), node_id, job_id, &call.body) {
                Ok(jobs::Accept::State(s)) => json(200, &json!({"job_id": job_id, "state": s})),
                Ok(jobs::Accept::NotFound) => not_found(),
                Ok(jobs::Accept::Conflict) => refusal(
                    409,
                    "conflict",
                    "bundle_sha256 is not this job's",
                    Value::Null,
                ),
                Ok(jobs::Accept::Invalid) => refusal(
                    422,
                    "invalid_request",
                    "bundle_sha256 is required",
                    Value::Null,
                ),
                Err(e) => internal(&e),
            }
        }
        ("GET", ["objects", sha]) if util::is_sha256(sha) => {
            match jobs::node_may_read(&state.store.db(), node_id, sha) {
                Ok(true) => match state.store.read_object(sha) {
                    Some(data) => bytes(200, "application/octet-stream", data.into()),
                    None => not_found(),
                },
                Ok(false) => not_found(),
                Err(e) => internal(&e),
            }
        }
        ("PUT", ["objects", sha]) if util::is_sha256(sha) => {
            if util::sha256_hex(&call.body) != *sha {
                return refusal(
                    422,
                    "invalid_request",
                    "the bytes do not hash to the object's name",
                    Value::Null,
                );
            }
            match state.store.put_object(&call.body) {
                Ok(stored) => json(200, &json!({"sha256": sha, "stored": stored})),
                Err(e) => internal(&e),
            }
        }
        ("POST", ["envelopes"]) => json(200, &envelope::receive(&state.store, node_id, &call.body)),
        _ => not_found(),
    }
}

/// Protocol 2's calls (§9), other than the held-open `GET work`; `None` for any other route.
fn route_v2(state: &State, call: &Call, node: &Node, parts: &[&str]) -> Option<Reply> {
    Some(match (call.method.as_str(), parts) {
        ("GET", ["control"]) => match control::open_for(&state.store.db(), node.id) {
            Ok(list) => json(200, &json!({"control": list})),
            Err(e) => internal(&e),
        },
        ("POST", ["control", id]) => {
            match control::report(&state.store, &mut state.store.db(), node.id, id, &call.body) {
                Ok(v) => json(200, &v),
                Err(control::Refused(status, code, detail, extra)) => {
                    refusal(status, code, &detail, extra)
                }
            }
        }
        ("POST", ["runs", run_id, "events"]) => {
            live::post_events(state, call, node.id, node.name, run_id)
        }
        ("POST", ["runs", run_id, "land"]) => {
            crate::land::post_land(state, call, node.id, node.name, run_id)
        }
        ("POST", ["status"]) => fleet::post_status(state, call, node.id, node.name),
        ("POST", ["schedules"]) => fleet::post_schedules(state, call, node.id, node.name),
        ("POST", ["conversations", cid, "lead-credential"]) => {
            return Some(crate::lead_node::issue_credential(
                state, node.id, node.name, cid,
            ));
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_header() {
        assert_eq!(version(Some("1")), Some(1));
        for bad in ["0", "01", "1.0", "v1", "", "-1", "1234567890"] {
            assert_eq!(version(Some(bad)), None, "{bad}");
        }
        assert_eq!(version(None), None);
    }
}
