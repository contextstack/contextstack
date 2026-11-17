//! The Client API's usage and budgets (CLIENT-API.md §12).
//!
//! | Call | Who |
//! | --- | --- |
//! | `POST projects` (key) `{name, budget?}` → 201 `{project}`; `GET projects`, `GET projects/{id}` | any |
//! | `PUT projects/{id}/budget` `{usd, period?}`, `DELETE projects/{id}/budget` → `{project}` | any |
//! | `PUT conversations/{id}/budget`, `DELETE conversations/{id}/budget` → `{conversation}` | any |
//! | `GET usage?by=&period=&date=&project=&conversation=&intent=&node=` | any |
//! | `GET source-budgets` (the gateway's sources, labelled) | any |
//! | `PUT source-budgets` (the gateway's whole `limits.json`) | owner |
//! | `PUT source-budgets/{source}` `{usd, period}`, `DELETE source-budgets/{source}` (one source; `*` is shared) | owner |
//!
//! A budget write replaces the budget, so it needs no key. Setting, changing or removing one offers any waiting
//! job that now fits.

use crate::access::Client;
use crate::budget::{self, Period};
use crate::gateway::{self, Fault};
use crate::records;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::{Record, touch};
use crate::usage::{self, By, Only, Span};
use crate::util::{self, now_utc};
use crate::work::{No, body, content, key, prior, remember, render};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};

/// Serve a call under `projects`, `usage`, `source-budgets`, or a conversation's `budget`.
pub fn handle(state: &State, call: &Call, client: &Client, parts: &[&str]) -> Reply {
    let db = || state.store.db();
    let found = match (call.method.as_str(), parts) {
        ("POST", ["projects"]) => return create(state, call, client),
        ("GET", ["projects"]) => records::list(&db(), Record::Project, None)
            .map(|p| json!({"projects": p}))
            .map_err(|e| e.to_string()),
        ("GET", ["projects", id]) => records::one(&db(), Record::Project, id)
            .map(|p| p.map_or(Value::Null, |p| json!({"project": p})))
            .map_err(|e| e.to_string()),
        ("PUT" | "DELETE", [kind @ ("projects" | "conversations"), id, "budget"]) => {
            return set_budget(state, call, client, kind, id);
        }
        ("GET", ["usage"]) => return usage(state, call, client),
        ("GET", ["source-budgets"]) => return sources(state, None),
        ("PUT", ["source-budgets"]) => {
            if let Some(no) = not_owner(client) {
                return no;
            }
            if call.body.len() > gateway::MAX_DOCUMENT {
                return refusal(
                    422,
                    "invalid_request",
                    "a budgets document is at most 64 KiB",
                    Value::Null,
                );
            }
            return sources(state, Some(&call.body));
        }
        ("PUT" | "DELETE", ["source-budgets", source]) => {
            if let Some(no) = not_owner(client) {
                return no;
            }
            return one_source(state, call, source);
        }
        _ => return refusal(404, "not_found", "no such route or record", Value::Null),
    };
    match found {
        Ok(Value::Null) => refusal(404, "not_found", "no such record", Value::Null),
        Ok(v) => json(200, &v),
        Err(e) => internal(&e),
    }
}

/// A budget body: `{usd, period?}`; `usd` is dollars (a number or decimal string, 0 or more), `period` is `total`
/// (the default), `day` or `month` (local days and months on this host, `budget::Period`).
fn parse_budget(m: &Map<String, Value>) -> Result<(budget::Micros, Period), No> {
    let usd = m
        .get("usd")
        .and_then(budget::amount)
        .ok_or_else(|| No::invalid("usd must be a dollar amount of 0 or more"))?;
    let period = match m.get("period") {
        None | Some(Value::Null) => Period::Total,
        Some(Value::String(p)) => {
            Period::named(p).ok_or_else(|| No::invalid("period must be total, day or month"))?
        }
        Some(_) => return Err(No::invalid("period must be total, day or month")),
    };
    Ok((usd, period))
}

/// `POST projects`: a project, with an optional budget.
fn create(state: &State, call: &Call, client: &Client) -> Reply {
    let (key, m) = match (key(call), body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    let Some(name) = m
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty() && n.len() <= 200)
    else {
        return No::invalid("a project has a name of 1 to 200 bytes").reply();
    };
    let budget = match m.get("budget") {
        None | Some(Value::Null) => None,
        Some(Value::Object(b)) => match parse_budget(b) {
            Ok(b) => Some(b),
            Err(r) => return r.reply(),
        },
        Some(_) => return No::invalid("budget must be {usd, period}").reply(),
    };
    let want = json!({"name": name, "budget": budget.map(|(u, p)| json!({"usd_micros": u, "period": p.name()}))});
    let sha = content("project", "", &want);
    let mut db = state.store.db();
    match prior(&db, &key, &sha) {
        Ok(Some((kind, id))) => return wrapped(&db, 200, kind, &id, true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let made = (|| -> Result<String, String> {
        let id = format!("prj_{}", util::ulid()?.to_lowercase());
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO projects (project_id, name, created_by, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, client.id, now_utc()],
        )
        .map_err(|e| e.to_string())?;
        if let Some((usd, period)) = budget {
            crate::scopes::set(&tx, "project", &id, usd, period, &client.id)?;
        }
        touch(&tx, Record::Project, &id).map_err(|e| e.to_string())?;
        remember(&tx, &key, &sha, Record::Project, &id, client).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(id)
    })();
    match made {
        Ok(id) => wrapped(&db, 201, Record::Project, &id, false),
        Err(e) => internal(&e),
    }
}

/// A project or conversation as `{project}` / `{conversation}`.
fn wrapped(
    db: &rusqlite::Connection,
    status: u16,
    kind: Record,
    id: &str,
    replayed: bool,
) -> Reply {
    if kind != Record::Project {
        return render(db, status, kind, id, replayed);
    }
    match records::one(db, kind, id) {
        Ok(Some(p)) => {
            let mut reply = json(status, &json!({"project": p}));
            if replayed && let Ok(h) = "true".parse() {
                reply.headers_mut().insert("idempotent-replayed", h);
            }
            reply
        }
        Ok(None) => refusal(404, "not_found", "no such project", Value::Null),
        Err(e) => internal(&e),
    }
}

/// `PUT` or `DELETE` `{projects|conversations}/{id}/budget`.
fn set_budget(state: &State, call: &Call, client: &Client, kind: &str, id: &str) -> Reply {
    let (scope, record, table, col) = if kind == "projects" {
        ("project", Record::Project, "projects", "project_id")
    } else {
        (
            "conversation",
            Record::Conversation,
            "conversations",
            "conversation_id",
        )
    };
    let wanted = if call.method == "PUT" {
        match body(call).and_then(|m| parse_budget(&m)) {
            Ok(b) => Some(b),
            Err(r) => return r.reply(),
        }
    } else {
        None
    };
    let mut db = state.store.db();
    let exists = db
        .query_row(
            &format!("SELECT 1 FROM {table} WHERE {col} = ?1"),
            params![id],
            |_| Ok(()),
        )
        .optional();
    match exists {
        Ok(Some(())) => {}
        Ok(None) => return refusal(404, "not_found", &format!("no such {scope}"), Value::Null),
        Err(e) => return internal(&e),
    }
    let done = (|| -> Result<(), String> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        match wanted {
            Some((usd, period)) => {
                crate::scopes::set(&tx, scope, id, usd, period, &client.id)?;
            }
            None => crate::scopes::clear(&tx, scope, id)?,
        }
        crate::reservations::release(&state.store, &tx)?;
        tx.commit().map_err(|e| e.to_string())
    })();
    if let Err(e) = done {
        return internal(&e);
    }
    crate::logf!(
        "{scope} {id}: budget {} by {}",
        if wanted.is_some() { "set" } else { "removed" },
        client.id
    );
    match records::one(&db, record, id) {
        Ok(Some(r)) => json(200, &json!({scope: r})),
        Ok(None) => refusal(404, "not_found", &format!("no such {scope}"), Value::Null),
        Err(e) => internal(&e),
    }
}

/// `GET usage`.
fn usage(state: &State, call: &Call, client: &Client) -> Reply {
    let Some(by) = By::named(call.query("by").unwrap_or("source")) else {
        return No::invalid("by must be source, project, conversation, intent or node").reply();
    };
    let span = match Span::new(call.query("period").unwrap_or("day"), call.query("date")) {
        Ok(s) => s,
        Err(e) => return No::invalid(&e).reply(),
    };
    let q = |k: &str| call.query(k).map(String::from);
    let only = Only {
        lead: (client.kind == crate::access::Kind::Lead).then(|| client.clone()),
        project: q("project"),
        conversation: q("conversation"),
        intent: q("intent"),
        node: q("node"),
    };
    match usage::rollup(&state.store.db(), by, &span, &only) {
        Ok(v) => json(200, &v),
        Err(e) => internal(&e),
    }
}

/// Only the owner (the local credential) sets source budgets.
fn not_owner(client: &Client) -> Option<Reply> {
    (client.kind != crate::access::Kind::Local).then(|| {
        refusal(
            403,
            "owner_only",
            "only the local credential may set source budgets",
            Value::Null,
        )
    })
}

/// Source-budget writes through this Director, one at a time, so two edits never lose one another.
static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How often a per-source edit reads again when the gateway's budgets change under it.
const EDIT_ATTEMPTS: usize = 3;

/// `GET source-budgets`, or with a document `PUT source-budgets`: the gateway's answer, labelled.
fn sources(state: &State, document: Option<&[u8]>) -> Reply {
    let g = &state.settings.gateway;
    let answer = tokio::task::block_in_place(|| match document {
        None => g.budgets(),
        Some(d) => {
            let _one = WRITES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.put_budgets(d)
        }
    });
    answered(state, answer, document.is_some())
}

/// `PUT source-budgets/{source}` `{usd, period}` sets one source's budget, `DELETE` turns it off; `*` is the shared
/// budget. Read-modify-write against the gateway: read its report and rebuild `limits.json` from it
/// (`gateway::limits_of`), change the one entry, read again and compare, and write only when nothing changed in
/// between, else start over (up to three times, then 409 `budgets_changed`). The gateway offers no revision, so a
/// writer outside this Director that changes the file between the last read and the write is overwritten.
fn one_source(state: &State, call: &Call, raw: &str) -> Reply {
    let Some(source) = util::percent_decode(raw).and_then(|s| gateway::source_name(&s)) else {
        return No::invalid(
            "a source is a gateway source id (letters, digits, . _ - :) or * for the shared budget",
        )
        .reply();
    };
    let set = if call.method == "PUT" {
        let m = match body(call) {
            Ok(m) => m,
            Err(r) => return r.reply(),
        };
        let Some(usd) = m.get("usd").and_then(budget::amount) else {
            return No::invalid("usd must be a dollar amount of 0 or more").reply();
        };
        let Some(period) = m
            .get("period")
            .and_then(Value::as_str)
            .filter(|p| matches!(*p, "day" | "month"))
        else {
            return No::invalid("period must be day or month (the gateway's periods)").reply();
        };
        #[allow(clippy::cast_precision_loss, reason = "money is far below 2^53 micros")]
        Some((usd as f64 / 1e6, period.to_string()))
    } else {
        None
    };
    let g = &state.settings.gateway;
    let set = set.as_ref().map(|(u, p)| (*u, p.as_str()));
    match tokio::task::block_in_place(|| edit(g, &source, set)) {
        Edit::Answer(answer) => {
            if matches!(answer, Ok((200, _))) {
                crate::logf!(
                    "source budget {source}: {} through the gateway at {}",
                    if set.is_some() { "set" } else { "turned off" },
                    g.addr
                );
            }
            answered(state, answer, false)
        }
        Edit::Unreadable(e) => refusal(502, "gateway_refused", &e, Value::Null),
        Edit::Changed => refusal(
            409,
            "budgets_changed",
            "the gateway's budgets kept changing while this was applied; read them and try again",
            Value::Null,
        ),
    }
}

/// How a per-source edit ended.
enum Edit {
    /// The gateway answered (the write's answer, or a failed read's).
    Answer(Result<(u16, Value), Fault>),
    /// Its report could not be turned back into `limits.json`.
    Unreadable(String),
    /// It changed between every read and the next.
    Changed,
}

/// Read, change one entry, read again and compare, write (`one_source`).
fn edit(g: &gateway::Gateway, source: &str, set: Option<(f64, &str)>) -> Edit {
    let _one = WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let read = || match g.budgets() {
        Ok((200, report)) => gateway::limits_of(&report).map_err(Edit::Unreadable),
        other => Err(Edit::Answer(other)),
    };
    for _ in 0..EDIT_ATTEMPTS {
        let before = match read() {
            Ok(l) => l,
            Err(e) => return e,
        };
        let doc = json!({"sources": gateway::edited(before.clone(), source, set)});
        match read() {
            Ok(again) if again == before => {}
            Ok(_) => continue,
            Err(e) => return e,
        }
        return Edit::Answer(g.put_budgets(doc.to_string().as_bytes()));
    }
    Edit::Changed
}

/// The gateway's answer as the Client API gives it.
fn answered(state: &State, answer: Result<(u16, Value), Fault>, replaced: bool) -> Reply {
    let g = &state.settings.gateway;
    match answer {
        Ok((200, report)) => {
            if replaced {
                crate::logf!("source budgets replaced through the gateway at {}", g.addr);
            }
            json(
                200,
                &json!({"gateway": g.addr.to_string(), "budgets": gateway::labelled(report)}),
            )
        }
        Ok((status @ (400 | 413 | 422), body)) => refusal(
            422,
            "invalid_request",
            "the gateway refused the budgets document",
            json!({"gateway_status": status, "gateway": body}),
        ),
        Ok((status, body)) => refusal(
            502,
            "gateway_refused",
            "the gateway refused the request",
            json!({"gateway_status": status, "gateway": body}),
        ),
        Err(Fault::Unavailable(e)) => refusal(502, "gateway_unavailable", &e, Value::Null),
        Err(Fault::Token(e)) => refusal(502, "gateway_owner_token", &e, Value::Null),
    }
}
