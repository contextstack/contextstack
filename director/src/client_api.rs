//! The client API, version 1 (`tests/director-spec/CLIENT-API.md`): work, decisions, evidence and live updates
//! for any client credential; pairing and revoking for the owner.
//!
//! | Call | Who |
//! | --- | --- |
//! | `GET me` → `{credential_id, owner}`, `GET nodes` | any client credential |
//! | `POST intents` (Idempotency-Key) → 201 `{intent, jobs, runs}`; `GET intents[?node=][&conversation=]`, `GET intents/{id}` | any |
//! | `GET jobs[?node=]`, `GET jobs/{id}`, `GET jobs/{id}/files/{path}`; `POST jobs/{id}/cancel` (key) → 202 | any |
//! | `GET runs[?node=]`, `GET runs/{id}`, `GET runs/{id}/files/{path}` | any |
//! | `POST runs/{id}/decision` (key) → 201 receipt; `GET runs/{id}/decision`, `GET decisions/{id}` | any |
//! | `GET cancels/{id}`; `PUT objects/{sha256}`, `GET objects/{sha256}` | any |
//! | `GET models` (usable executor templates from the gateway, labelled) | any |
//! | `GET repos` (repo inventories reported by machines) | any |
//! | `GET nodes/{name}`; `GET runs/{id}/events[?after=]` (live output) | any |
//! | `GET schedules[?node=]`, `GET schedules/{node}/{id}`; `POST schedules/{node}/{id}/changes` (key) → 202 | any |
//! | `GET schedule-changes/{id}` | any |
//! | conversations, turns, summaries (`conversations.rs`; CLIENT-API.md §11) | any |
//! | projects, budgets, usage, source budgets (`spend.rs`; CLIENT-API.md §12) | any; setting source budgets: owner |
//! | `GET events[?conversation=]` (server-sent events) | any |
//! | `GET credentials`, `POST pairings` `{"name"}`, `DELETE credentials/{id}` | owner (local or paired) |
//! | `POST /api/v1/pair` `{"code"}` → `{credential_id, name, token}` | holder of a live pairing code |
//!
//! Paths are under `/api/v1/client/` unless shown whole.

use crate::access::{self, Kind};
use crate::records;
use crate::server::{Call, Reply, State, bytes, internal, json, refusal};
use crate::store::Record;
use crate::util;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::Arc;

fn intent_route(
    state: &Arc<State>,
    call: &Call,
    client: &access::Client,
    id: &str,
    rest: &[&str],
) -> Option<Reply> {
    match (call.method.as_str(), rest) {
        ("GET", ["work-trace"]) => Some(crate::work_trace::get(state, id)),
        ("PUT", ["owner-messages", msg, "mark"]) => {
            Some(crate::work_trace::set_mark(state, call, client, id, msg))
        }
        _ => None,
    }
}

const PREFIX: &str = "/api/v1/client/";

/// Serve a non-node call.
pub fn handle(state: &Arc<State>, call: &Call) -> Reply {
    if call.path == "/api/v1/pair" && call.method == "POST" {
        return pair_request(state, call);
    }
    let client = match call.bearer().map(|t| access::client(&state.store, t)) {
        Some(Ok(Some(c))) => c,
        Some(Err(e)) => return internal(&e),
        _ => {
            return refusal(
                401,
                "unauthenticated",
                "a client credential is required",
                Value::Null,
            );
        }
    };
    handle_as(state, call, &client)
}

/// Dispatch for an already authenticated local UI session; never exposes a bearer to the browser.
#[allow(clippy::too_many_lines)]
pub(crate) fn handle_as(state: &Arc<State>, call: &Call, client: &access::Client) -> Reply {
    let Some(rest) = call.path.strip_prefix(PREFIX) else {
        return not_found();
    };
    let parts: Vec<&str> = rest.split('/').collect();
    if let Some(no) = crate::lead::guard(state, call, client, &parts) {
        return no;
    }
    let owner = client.is_owner();
    let node = call.query("node");
    let db = || state.store.db();
    let found = match (call.method.as_str(), parts.as_slice()) {
        (_, ["action-approvals", ..]) => {
            return crate::action_approvals::client(state, call, client, &parts);
        }
        ("GET", ["chief-of-staff"]) => return crate::chief_of_staff::get(state),
        ("PUT", ["chief-of-staff"]) => return crate::chief_of_staff::put(state, call, client),
        ("GET", ["me"]) => Ok(me_answer(client)),
        ("GET", ["lead-node"]) => crate::lead_node::get_answer(&db()).map_err(|e| e.clone()),
        ("PUT", ["lead-node"]) if owner => return crate::lead_node::put(state, call),
        ("GET", ["nodes"]) => list(state, Record::Node, None)
            .map(|n| json!({"nodes": n, "stale_after_seconds": records::stale_after()})),
        ("GET", ["nodes", name]) => one(state, Record::Node, name),
        ("GET", ["agents"] | ["agents", _]) => {
            return match crate::lane::agents(parts.get(1).copied()) {
                Ok(body) => json(200, &body),
                Err(e) => refusal(e.0, e.1, &e.2, e.3),
            };
        }
        ("GET", ["models"]) => return models_list(state),
        ("GET", ["repos"]) => crate::repos::list(state),
        ("GET" | "POST", ["schedules" | "schedule-changes", ..] | ["runs", _, "events"]) => {
            return step5(state, call, client, &parts, node);
        }
        ("GET", ["events"]) => {
            return crate::events::stream(state, client, call.query("conversation"));
        }
        (_, ["projects", id, "backlog", rest @ ..]) => {
            return crate::backlog::handle(state, call, client, id, rest);
        }
        (_, ["projects", id, "heartbeat"]) => {
            return crate::heartbeat::handle(state, call, client, id);
        }
        (_, ["projects", id, "decision_log", rest @ ..]) => {
            return crate::decision_log::handle(state, call, client, id, rest);
        }
        (_, ["projects", ..] | ["usage" | "source-budgets"] | ["source-budgets", _])
        | ("PUT" | "DELETE", ["conversations", _, "budget"]) => {
            return crate::spend::handle(state, call, client, &parts);
        }
        (
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE",
            ["conversations", ..] | ["turns" | "summaries", _],
        ) => {
            return crate::conversations::handle(state, call, client, &parts);
        }
        ("GET" | "PUT", ["intents", id, ..]) if parts.len() > 2 => {
            return intent_route(state, call, client, id, &parts[2..]).unwrap_or_else(not_found);
        }
        ("POST", ["intents", id, "jobs"]) => {
            return crate::work::add_job(state, call, client, id);
        }
        ("POST", ["intents"]) => return crate::work::submit(state, call, client),
        ("GET", ["intents"]) => return work_list(state, node, call, client),
        ("GET", ["intents", id]) if parts.get(2).is_none() => records::intent_bundle(&db(), id)
            .map(Option::unwrap_or_default)
            .map_err(|e| e.to_string()),
        ("GET", ["jobs"]) => records::list_work(&db(), Record::Job, node, client)
            .map_err(|e| e.to_string())
            .map(|j| json!({"jobs": j})),
        ("GET", ["jobs", id]) => one(state, Record::Job, id),
        ("POST", ["jobs", id, "cancel"]) => return crate::work::cancel(state, call, client, id),
        ("GET", ["runs"]) => records::list_work(&db(), Record::Run, node, client)
            .map_err(|e| e.to_string())
            .map(|r| json!({"runs": r})),
        ("GET", ["runs", id]) => one(state, Record::Run, id),
        ("POST", ["runs", id, "decision"]) => {
            return crate::work::decide(state, call, client, id);
        }
        ("GET", ["runs", id, "decision"]) => decision_of(state, id),
        ("GET", ["decisions", id]) => one(state, Record::Decision, id),
        ("GET", ["cancels", id]) => one(state, Record::Cancel, id),
        ("GET", ["jobs" | "runs", id, "files", path @ ..]) if !path.is_empty() => {
            return file(state, parts[0], id, &path.join("/"));
        }
        ("GET", ["objects", sha]) => return object(state, sha),
        ("PUT", ["objects", sha]) => return upload(state, call, sha, client),
        ("GET", ["credentials"]) if owner => access::credentials(&state.store),
        ("POST", ["credentials", "lead"]) if owner => return mint_lead(state, call, client),
        ("POST", ["pairings"]) if owner => return issue(state, call, client),
        ("DELETE", ["credentials", id]) if owner => {
            if client.kind == Kind::Device && client.id == *id {
                return refusal(
                    409,
                    "self_revoke",
                    "Use Forget this device to remove the credential locally.",
                    Value::Null,
                );
            }
            return match access::revoke_credential(&state.store, id, &client.id) {
                Ok(()) => json(200, &json!({"id": id, "revoked": true})),
                Err(e) => refusal(404, "not_found", &e, Value::Null),
            };
        }
        ("PUT", ["lead-node"]) | ("GET" | "POST" | "DELETE", ["credentials" | "pairings", ..]) => {
            return refusal(
                403,
                "owner_only",
                "only an owner credential may do this",
                Value::Null,
            );
        }
        _ => return not_found(),
    };
    match found {
        Ok(Value::Null) => not_found(),
        Ok(v) => json(200, &v),
        Err(e) => internal(&e),
    }
}

fn models_list(state: &State) -> Reply {
    match crate::models::list(&state.settings.gateway) {
        Ok(body) => json(200, &body),
        Err(crate::gateway::Fault::Unavailable(e)) => {
            refusal(502, "gateway_unavailable", &e, Value::Null)
        }
        Err(crate::gateway::Fault::Token(e)) => {
            refusal(502, "gateway_owner_token", &e, Value::Null)
        }
    }
}

/// `GET runs/{id}/decision`: the run's receipt.
fn decision_of(state: &State, run_id: &str) -> Result<Value, String> {
    let found = state
        .store
        .db()
        .query_row(
            "SELECT decision_id FROM decisions WHERE run_id = ?1",
            params![run_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    found.map_or(Ok(Value::Null), |d| one(state, Record::Decision, &d))
}

/// `GET intents`: the work, newest first, with the jobs of the intents listed.
/// `conversation=ID` keeps one lead's intents, `project=ID` keeps one project, `lanes=1` only task lanes,
/// `limit=N` (1..=500) caps the count. `dispatch_id=ID` recovers one submission by its key.
fn work_list(state: &State, node: Option<&str>, call: &Call, client: &access::Client) -> Reply {
    let db = state.store.db();
    if let Some(dispatch) = call.query("dispatch_id") {
        let dispatch = util::percent_decode(dispatch).unwrap_or_default();
        if let Err(no) = crate::work::validate_key(Some(&dispatch)) {
            return no.reply();
        }
        let id: Option<String> = match db
            .query_row(
                "SELECT record_id FROM writes WHERE key = ?1 AND record_kind = 'intent'",
                params![dispatch],
                |r| r.get(0),
            )
            .optional()
        {
            Ok(id) => id,
            Err(e) => return internal(&e),
        };
        let rows = match id {
            Some(id) => match records::one(&db, Record::Intent, &id) {
                Ok(Some(row)) => vec![row],
                Ok(None) => vec![],
                Err(e) => return internal(&e),
            },
            None => vec![],
        };
        let rows = match crate::lead::filter(&db, client, Record::Intent, rows) {
            Ok(rows) => rows,
            Err(e) => return internal(&e),
        };
        let ids: Vec<&str> = rows
            .iter()
            .filter_map(|i| i["intent_id"].as_str())
            .collect();
        return match records::jobs_of_intents(&db, &ids, None) {
            Ok(jobs) => json(200, &json!({"intents": rows, "jobs": jobs})),
            Err(e) => internal(&e),
        };
    }
    let lanes = call.query("lanes") == Some("1");
    let limit = call
        .query("limit")
        .and_then(|l| l.parse::<u32>().ok())
        .filter(|l| (1..=records::MAX_INTENTS).contains(l))
        .unwrap_or(records::MAX_INTENTS);
    let project = match call.query("project") {
        None => None,
        Some(id) => match crate::work::project_exists(&db, Some(id)) {
            Ok(()) => Some(id),
            Err(no) => return no.reply(),
        },
    };
    let intents = match records::list_intents(
        &db,
        node,
        call.query("conversation"),
        lanes,
        project,
        limit,
        Some(client),
    ) {
        Ok(rows) => rows,
        Err(e) => return internal(&e),
    };
    let ids: Vec<&str> = intents
        .iter()
        .filter_map(|i| i["intent_id"].as_str())
        .collect();
    let jobs = match records::jobs_of_intents(&db, &ids, node) {
        Ok(rows) => rows,
        Err(e) => return internal(&e),
    };
    json(200, &json!({"intents": intents, "jobs": jobs}))
}

fn list(state: &State, kind: Record, node: Option<&str>) -> Result<Vec<Value>, String> {
    records::list(&state.store.db(), kind, node).map_err(|e| e.to_string())
}

fn one(state: &State, kind: Record, id: &str) -> Result<Value, String> {
    records::one(&state.store.db(), kind, id)
        .map(Option::unwrap_or_default)
        .map_err(|e| e.to_string())
}

/// A content type that never renders as a page: text for the text evidence, JSON, else bytes.
fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "json" => "application/json",
        "md" | "txt" | "log" | "yaml" | "yml" | "jsonl" | "diff" | "patch" | "csv" => {
            "text/plain; charset=utf-8"
        }
        _ => "application/octet-stream",
    }
}

/// Evidence bytes, marked so a browser never sniffs or runs them.
fn evidence(sha: &str, path: &str, data: Vec<u8>) -> Reply {
    let mut reply = bytes(200, content_type(path), data.into());
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("content-security-policy", "sandbox"),
        ("contextstack-sha256", sha),
    ] {
        if let Ok(v) = value.parse() {
            reply.headers_mut().insert(name, v);
        }
    }
    reply
}

/// `GET jobs/{id}/files/{path}` (a bundle file) or `GET runs/{id}/files/{path}` (run evidence).
fn file(state: &State, of: &str, id: &str, raw_path: &str) -> Reply {
    let Some(path) = util::percent_decode(raw_path).filter(|p| util::plain_path(p)) else {
        return not_found();
    };
    let sql = if of == "jobs" {
        "SELECT sha256 FROM job_files WHERE job_id = ?1 AND path = ?2"
    } else {
        "SELECT sha256 FROM run_files WHERE run_id = ?1 AND path = ?2"
    };
    let found = state
        .store
        .db()
        .query_row(sql, params![id, path], |r| r.get::<_, String>(0))
        .optional();
    match found {
        Ok(Some(sha)) => match state.store.read_object(&sha) {
            Some(data) => evidence(&sha, &path, data),
            None => not_found(),
        },
        Ok(None) => not_found(),
        Err(e) => internal(&e),
    }
}

/// `GET objects/{sha256}`: an object some job's bundle or some run's evidence lists.
fn object(state: &State, sha: &str) -> Reply {
    if !util::is_sha256(sha) {
        return not_found();
    }
    let listed = state
        .store
        .db()
        .query_row(
            "SELECT 1 FROM run_files WHERE sha256 = ?1 UNION ALL SELECT 1 FROM job_files WHERE sha256 = ?1 LIMIT 1",
            params![sha],
            |_| Ok(()),
        )
        .optional();
    match listed {
        Ok(Some(())) => match state.store.read_object(sha) {
            Some(data) => evidence(sha, "", data),
            None => not_found(),
        },
        Ok(None) => not_found(),
        Err(e) => internal(&e),
    }
}

/// `PUT objects/{sha256}`: bundle bytes for an intent. Named by their hash, so a repeat is harmless.
fn upload(state: &State, call: &Call, sha: &str, client: &access::Client) -> Reply {
    if !util::is_sha256(sha) {
        return not_found();
    }
    if util::sha256_hex(&call.body) != sha {
        return refusal(
            422,
            "invalid_request",
            "the bytes do not hash to the object's name",
            Value::Null,
        );
    }
    match state.store.put_object(&call.body) {
        Ok(stored) => {
            // Even an existing object proves possession when the caller supplied its bytes.
            if client.kind == Kind::Lead
                && let Err(e) = state.store.db().execute(
                    "INSERT OR IGNORE INTO lead_objects (credential_id, sha256) VALUES (?1, ?2)",
                    params![client.id, sha],
                )
            {
                return internal(&e);
            }
            json(200, &json!({"sha256": sha, "stored": stored}))
        }
        Err(e) => internal(&e),
    }
}

fn not_found() -> Reply {
    refusal(404, "not_found", "no such route or record", Value::Null)
}

fn me_answer(client: &access::Client) -> Value {
    let mut me = json!({"credential_id": client.id, "owner": client.is_owner()});
    if let Some(c) = &client.conversation_id {
        me["conversation_id"] = json!(c);
    }
    me
}

fn issue(state: &State, call: &Call, client: &access::Client) -> Reply {
    let body: Value = serde_json::from_slice(&call.body).unwrap_or(Value::Null);
    let Some(name) = body.get("name").and_then(Value::as_str) else {
        return refusal(
            422,
            "invalid_request",
            "name the device being paired",
            Value::Null,
        );
    };
    match access::issue_pairing(&state.store, name, &client.id) {
        Ok((code, expires_at)) => json(
            200,
            &json!({"code": code, "name": name, "expires_at": expires_at}),
        ),
        Err(e) => refusal(422, "invalid_request", &e, Value::Null),
    }
}

fn pair_request(state: &State, call: &Call) -> Reply {
    if let Some(Ok(Some(client))) = call.bearer().map(|t| access::client(&state.store, t))
        && client.kind == Kind::Lead
    {
        return crate::lead::denied();
    }
    pair(state, call)
}

fn pair(state: &State, call: &Call) -> Reply {
    let body: Value = serde_json::from_slice(&call.body).unwrap_or(Value::Null);
    let code = body.get("code").and_then(Value::as_str).unwrap_or("");
    match access::exchange_pairing(&state.store, code) {
        Ok(Some((id, name, token))) => {
            crate::logf!("paired device {name} as {id}");
            json(
                200,
                &json!({"credential_id": id, "name": name, "token": token}),
            )
        }
        Ok(None) => refusal(
            403,
            "pairing_refused",
            "no live pairing code matches",
            Value::Null,
        ),
        Err(e) => internal(&e),
    }
}

/// The Client API's step-5 reads and writes: schedules, schedule changes and a run's live events.
fn step5(
    state: &Arc<State>,
    call: &Call,
    client: &access::Client,
    parts: &[&str],
    node: Option<&str>,
) -> Reply {
    let found = match (call.method.as_str(), parts) {
        ("GET", ["schedules"]) => {
            list(state, Record::Schedule, node).map(|s| json!({"schedules": s}))
        }
        ("GET", ["schedules", n, id]) => {
            crate::fleet::schedule_detail(&state.store.db(), &format!("{n}/{id}"))
                .map_err(|e| e.to_string())
        }
        ("POST", ["schedules", n, id, "changes"]) => {
            return crate::fleet::request_change(state, call, client, n, id);
        }
        ("GET", ["schedule-changes", id]) => one(state, Record::ScheduleChange, id),
        ("GET", ["runs", id, "events"]) => {
            let Ok(after) = call.query("after").map_or(Ok(0), str::parse::<i64>) else {
                return refusal(
                    422,
                    "invalid_request",
                    "after is a whole number",
                    Value::Null,
                );
            };
            crate::live::read_events(&state.store.db(), id, after)
                .map(Option::unwrap_or_default)
                .map_err(|e| e.to_string())
        }
        _ => return not_found(),
    };
    match found {
        Ok(Value::Null) => not_found(),
        Ok(v) => json(200, &v),
        Err(e) => internal(&e),
    }
}

fn mint_lead(state: &State, call: &Call, client: &access::Client) -> Reply {
    let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
    let (Some(name), Some(conversation)) =
        (body["name"].as_str(), body["conversation_id"].as_str())
    else {
        return refusal(
            422,
            "invalid_request",
            "name and conversation_id are required",
            Value::Null,
        );
    };
    let intent = match body.get("intent_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        _ => {
            return refusal(
                422,
                "invalid_request",
                "intent_id must be a string",
                Value::Null,
            );
        }
    };
    match access::mint_lead(&state.store, name, conversation, intent, &client.id) {
        Ok(v) => json(201, &v),
        Err(e) => refusal(422, "invalid_request", &e, Value::Null),
    }
}
