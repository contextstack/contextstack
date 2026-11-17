//! Client writes (CLIENT-API.md §4, §6, §7): submit an intent, decide a run's result, request a job's cancel.
//!
//! Every write names an `Idempotency-Key`. Its content is the operation, its target and the normalised
//! body; the key is remembered with the record it made, in the transaction that made it. The same key with
//! the same content answers 200 with that record as it is now; with other content, 409. A refused write
//! records nothing and does not use up its key. A write is answered only after it is committed.

use crate::access::Client;
use crate::jobs::{self, Plan, Unusable};
use crate::records;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::{Record, touch};
use crate::util::{self, now_utc, sha256_hex};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

/// The longest idempotency key accepted.
const MAX_KEY: usize = 128;

fn invalid(detail: &str) -> Reply {
    refusal(422, "invalid_request", detail, Value::Null)
}

fn not_found(what: &str) -> Reply {
    refusal(404, "not_found", &format!("no such {what}"), Value::Null)
}

/// A refusal on its way out of a helper: status, code, detail and extra fields.
pub struct No(pub u16, pub &'static str, pub String, pub Value);

impl No {
    pub fn invalid(detail: &str) -> Self {
        Self(422, "invalid_request", detail.to_string(), Value::Null)
    }

    pub fn internal(e: &impl std::fmt::Display) -> Self {
        crate::logf!("internal error: {e}");
        Self(
            500,
            "internal",
            "the Director could not complete the request".into(),
            Value::Null,
        )
    }

    pub fn reply(self) -> Reply {
        refusal(self.0, self.1, &self.2, self.3)
    }
}

/// The request's idempotency key: 1–128 of `[A-Za-z0-9._:-]`.
pub fn key(call: &Call) -> Result<String, No> {
    validate_key(call.header("idempotency-key"))
}

/// Validate a dispatch ID or the legacy idempotency header with the same bounds.
pub fn validate_key(value: Option<&str>) -> Result<String, No> {
    match value {
        Some(k)
            if !k.is_empty()
                && k.len() <= MAX_KEY
                && k.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
                }) =>
        {
            Ok(k.to_string())
        }
        Some(_) => Err(No(
            422,
            "idempotency_key_invalid",
            "Idempotency-Key must be 1-128 characters of A-Z a-z 0-9 . _ : -".into(),
            Value::Null,
        )),
        None => Err(No(
            428,
            "idempotency_key_required",
            "every write names an Idempotency-Key header".into(),
            Value::Null,
        )),
    }
}

/// The content a key stands for.
pub fn content(op: &str, target: &str, body: &Value) -> String {
    sha256_hex(
        json!({"op": op, "target": target, "body": body})
            .to_string()
            .as_bytes(),
    )
}

/// Check lead scope under the same database lock used for idempotency replay/conflicts.
pub fn scoped_prior(
    db: &Connection,
    key: &str,
    sha: &str,
    client: &Client,
) -> Result<Option<(Record, String)>, No> {
    if client.kind == crate::access::Kind::Lead {
        let found: Option<(String, String)> = db
            .query_row(
                "SELECT record_kind, record_id FROM writes WHERE key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| No::internal(&e))?;
        if let Some((kind, id)) = found {
            let allowed = Record::named(&kind)
                .map(|k| crate::lead::visible(db, client, k, &id))
                .transpose()
                .map_err(|e| No::internal(&e))?
                .unwrap_or(false);
            if !allowed {
                return Err(No(
                    403,
                    "owner_only",
                    "only the local credential may do this".into(),
                    Value::Null,
                ));
            }
        }
    }
    prior(db, key, sha)
}

/// What a key already made: `Ok(Some(..))` for the same content, a 409 for other content.
pub fn prior(db: &Connection, key: &str, sha: &str) -> Result<Option<(Record, String)>, No> {
    let found: Option<(String, String, String)> = db
        .query_row(
            "SELECT request_sha256, record_kind, record_id FROM writes WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| No::internal(&e))?;
    match found {
        None => Ok(None),
        Some((was, kind, id)) if was == sha => Ok(Record::named(&kind).map(|k| (k, id))),
        Some((_, kind, id)) => Err(No(
            409,
            "idempotency_conflict",
            "this Idempotency-Key was used for a different request".into(),
            json!({"record": {"kind": kind, "id": id}}),
        )),
    }
}

pub fn remember(
    db: &Connection,
    key: &str,
    sha: &str,
    kind: Record,
    id: &str,
    client: &Client,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO writes (key, request_sha256, record_kind, record_id, credential_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key, sha, kind.name(), id, client.id, now_utc()],
    )
    .map(|_| ())
}

/// Remember a node-route write (idempotency); `node_id` is stored as `node:{id}`.
pub fn remember_node(
    db: &Connection,
    key: &str,
    sha: &str,
    kind: Record,
    id: &str,
    node_id: i64,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO writes (key, request_sha256, record_kind, record_id, credential_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key, sha, kind.name(), id, format!("node:{node_id}"), now_utc()],
    )
    .map(|_| ())
}

pub fn body(call: &Call) -> Result<Map<String, Value>, No> {
    let raw: &[u8] = if call.body.is_empty() {
        b"{}"
    } else {
        &call.body
    };
    match serde_json::from_slice(raw) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(No::invalid("the body must be a JSON object")),
    }
}

/// An optional string field of at most `max` bytes.
fn text(m: &Map<String, Value>, field: &str, max: usize) -> Result<Option<String>, No> {
    match m.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= max => Ok(Some(s.clone())),
        Some(_) => Err(No::invalid(&format!(
            "{field} must be a string of at most {max} bytes"
        ))),
    }
}

/// The intent a body asks for, normalised (defaults filled), so equal requests hash equal.
fn normalise_intent(m: &Map<String, Value>) -> Result<Value, No> {
    let need = |f: &str| {
        m.get(f)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .ok_or_else(|| No::invalid(&format!("an intent names its {f}")))
    };
    let (node, spec, task) = (need("node")?, need("spec")?, need("task")?);
    let trials = match m.get("trials") {
        None | Some(Value::Null) => 1,
        Some(t) => t
            .as_u64()
            .filter(|n| (1..=100).contains(n))
            .ok_or_else(|| No::invalid("trials must be an integer from 1 to 100"))?,
    };
    let meta = match m.get("meta") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(o)) if o.values().all(Value::is_string) => o.clone(),
        Some(_) => return Err(No::invalid("meta must be an object of strings")),
    };
    let source = match (m.get("files"), m.get("from_job")) {
        (Some(f), None) => json!({"files": f}),
        (None, Some(Value::String(j))) => json!({"from_job": j}),
        _ => return Err(No::invalid("give exactly one of files or from_job")),
    };
    let mut want = json!({"node": node, "spec": spec, "task": task.trim_end_matches('/'), "trials": trials,
                          "grant": text(m, "grant", 128)?, "meta": meta, "title": text(m, "title", 200)?,
                          "source": source});
    // Only when given, so a request without it hashes as it did before projects existed.
    if let Some(p) = text(m, "project_id", 128)? {
        want["project_id"] = json!(p);
    }
    Ok(want)
}

/// A project named in a write must exist.
pub fn project_exists(db: &Connection, id: Option<&str>) -> Result<(), No> {
    let Some(id) = id else { return Ok(()) };
    let found = db
        .query_row(
            "SELECT 1 FROM projects WHERE project_id = ?1",
            params![id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| No::internal(&e))?;
    found.ok_or_else(|| No::invalid(&format!("no project {id}")))
}

/// A record as a reply; an intent answers with its jobs and runs.
pub fn render(db: &Connection, status: u16, kind: Record, id: &str, replayed: bool) -> Reply {
    let found = if kind == Record::Intent {
        records::intent_bundle(db, id)
    } else {
        records::one(db, kind, id)
    };
    match found {
        Ok(Some(v)) => {
            let mut reply = json(status, &v);
            if replayed && let Ok(h) = "true".parse() {
                reply.headers_mut().insert("idempotent-replayed", h);
            }
            reply
        }
        Ok(None) => not_found(kind.name()),
        Err(e) => internal(&e),
    }
}

/// An intent's bundle: its listed objects, or an earlier job's bundle (`from_job`) with the intent that job's spec
/// was stamped with (`stamp.rs`), which the new intent may replace.
fn source(
    state: &State,
    db: &Connection,
    s: &Value,
) -> Result<(Vec<jobs::BundleFile>, Option<String>), No> {
    if let Some(job) = s.get("from_job") {
        let job = job.as_str().unwrap_or_default();
        let stamped = db
            .query_row(
                "SELECT intent_id FROM jobs WHERE job_id = ?1",
                params![job],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| No::internal(&e))?
            .flatten();
        return match jobs::bundle_of_job(db, job) {
            Ok(f) if !f.is_empty() => Ok((f, stamped)),
            Ok(_) => Err(No::invalid(&format!("from_job: no job {job}"))),
            Err(e) => Err(No::internal(&e)),
        };
    }
    match jobs::bundle_of_objects(&state.store, &s["files"]) {
        Ok(f) => Ok((f, None)),
        Err(Unusable::Invalid(e)) => Err(No::invalid(&e)),
        Err(Unusable::Missing(missing)) => Err(No(
            422,
            "objects_missing",
            "upload these objects first (PUT /api/v1/client/objects/{sha256})".into(),
            json!({"missing": missing}),
        )),
    }
}

/// A built-in replay answers before reading the current installed profile.
fn prepare_lane(
    state: &State,
    client: &Client,
    key: &str,
    m: &mut Map<String, Value>,
) -> Result<Option<String>, Box<Reply>> {
    // Built-in requests hash their submitted form, independent of later profile revisions.
    let builtin_sha = m
        .contains_key("lane")
        .then(|| content("intent_lane", "", &json!(&*m)));
    if let Some(sha) = &builtin_sha {
        let db = state.store.db();
        match scoped_prior(&db, key, sha, client) {
            Ok(Some((kind, id))) => return Err(Box::new(render(&db, 200, kind, &id, true))),
            Ok(None) => {}
            Err(r) => return Err(Box::new(r.reply())),
        }
    }
    if let Err(e) = crate::lane::expand(state, m) {
        return Err(Box::new(e.reply()));
    }
    Ok(builtin_sha)
}

/// Resolve the caller's dispatch key before hashing the submitted request.
fn dispatch_body(call: &Call) -> Result<(String, Map<String, Value>), No> {
    let mut m = body(call)?;
    let key = match m.remove("dispatch_id") {
        Some(Value::String(id)) => validate_key(Some(&id)),
        Some(_) => Err(No::invalid("dispatch_id must be a string")),
        None => key(call),
    }?;
    Ok((key, m))
}

/// `POST intents`: record the intent and the one job it names, offered to its node.
pub fn submit(state: &State, call: &Call, client: &Client) -> Reply {
    let (key, mut m) = match dispatch_body(call) {
        Ok(pair) => pair,
        Err(no) => return no.reply(),
    };
    let builtin_sha = match prepare_lane(state, client, &key, &mut m) {
        Ok(sha) => sha,
        Err(reply) => return *reply,
    };
    let want = match normalise_intent(&m) {
        Ok(w) => w,
        Err(r) => return r.reply(),
    };
    let sha = builtin_sha.unwrap_or_else(|| content("intent", "", &want));
    let mut db = state.store.db();
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some((kind, id))) => return render(&db, 200, kind, &id, true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let (files, stamped_before) = match source(state, &db, &want["source"]) {
        Ok(found) => found,
        Err(r) => return r.reply(),
    };
    let s = |f: &str| want[f].as_str().unwrap_or_default().to_string();
    let project = want["project_id"].as_str();
    if let Err(r) = project_exists(&db, project) {
        return r.reply();
    }
    let mut plan = match Plan::new(
        &db,
        &s("node"),
        &s("spec"),
        &s("task"),
        u32::try_from(want["trials"].as_u64().unwrap_or(1)).unwrap_or(1),
        want["grant"].as_str(),
        want["meta"].as_object().cloned().unwrap_or_default(),
        files,
    ) {
        Ok(p) => p.under(crate::reservations::JobScope {
            project: project.map(String::from),
            conversation: None,
        }),
        Err(e) => return invalid(&e),
    };
    if (want["source"].get("from_job").is_some() || plan.spec_names_task(&state.store))
        && let Err(e) = plan.fill_missing_check(&state.store)
    {
        return internal(&e);
    }
    // Provenance is Director-owned, never supplied through request metadata. It survives
    // credential rotation and bundle reuse through arbitrarily many descendant intents.
    let lead_conversation = (client.kind == crate::access::Kind::Lead)
        .then(|| client.conversation_id.clone())
        .flatten();
    let conversation = if client.kind == crate::access::Kind::Lead {
        client.conversation_id.clone()
    } else if let Some(job) = want["source"]["from_job"].as_str() {
        match db.query_row(
            "SELECT COALESCE((SELECT conversation_id FROM turns WHERE job_id = ?1),
                             (SELECT i.conversation_id FROM jobs j JOIN intents i
                              ON i.intent_id = j.intent_id WHERE j.job_id = ?1))",
            params![job],
            |r| r.get::<_, Option<String>>(0),
        ) {
            Ok(c) => c,
            Err(e) => return internal(&e),
        }
    } else {
        None
    };
    let intent_id = match util::ulid() {
        Ok(id) => id,
        Err(e) => return internal(&e),
    };
    match plan.stamp(&state.store, &intent_id, stamped_before.as_deref()) {
        Ok(()) => {}
        Err(crate::stamp::Refused::Mismatch(m)) => {
            return refusal(422, "invalid_request", &m, json!({"field": "intent"}));
        }
        Err(crate::stamp::Refused::Invalid(m)) => return invalid(&m),
    }
    let made = (|| -> Result<String, String> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO intents (intent_id, title, node, spec, task, trials, grant_id, meta, bundle_sha256,
                                  created_by, created_at, project_id, conversation_id, lead_conversation_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![intent_id, want["title"].as_str(), s("node"), s("spec"), s("task"),
                    want["trials"].as_i64(), want["grant"].as_str(), json!(plan.meta()).to_string(),
                    plan.digest, client.id, now_utc(), project, conversation, lead_conversation],
        )
        .map_err(|e| e.to_string())?;
        plan.insert(&state.store, &tx, Some(&intent_id))?;
        touch(&tx, Record::Intent, &intent_id).map_err(|e| e.to_string())?;
        remember(&tx, &key, &sha, Record::Intent, &intent_id, client).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(intent_id)
    })();
    match made {
        Ok(id) => render(&db, 201, Record::Intent, &id, false),
        Err(e) => internal(&e),
    }
}

/// `POST intents/{id}/jobs`: one further attempt, retaining the task's identity and node.
pub fn add_job(state: &State, call: &Call, client: &Client, intent_id: &str) -> Reply {
    if !matches!(
        client.kind,
        crate::access::Kind::Local | crate::access::Kind::Lead
    ) {
        return crate::lead::denied();
    }
    let (key, m) = match (key(call), body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    let want = match normalise_intent(&m) {
        Ok(w) => w,
        Err(r) => return r.reply(),
    };
    if want["trials"] != 1 {
        return invalid("an additional job has exactly one trial");
    }
    let sha = content("intent_job", intent_id, &want);
    let mut db = state.store.db();
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some((kind, id))) => return render(&db, 200, kind, &id, true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let intent = match records::one(&db, Record::Intent, intent_id) {
        Ok(Some(i)) => i,
        Ok(None) => return not_found("intent"),
        Err(e) => return internal(&e),
    };
    // Intents have no lifecycle column; an explicit terminal marker lives in their metadata.
    if matches!(
        intent["meta"]["state"].as_str(),
        Some("landed" | "discarded")
    ) {
        return refusal(
            409,
            "intent_finished",
            "the task is already finished",
            Value::Null,
        );
    }
    if want["node"] != intent["node"]
        || want["spec"] != intent["spec"]
        || want["task"] != intent["task"]
    {
        return invalid("an additional job retains the intent's node, spec and task");
    }
    let (files, stamped) = match source(state, &db, &want["source"]) {
        Ok(f) => f,
        Err(r) => return r.reply(),
    };
    let mut plan = match Plan::new(
        &db,
        intent["node"].as_str().unwrap_or_default(),
        intent["spec"].as_str().unwrap_or_default(),
        intent["task"].as_str().unwrap_or_default(),
        1,
        intent["grant"].as_str(),
        want["meta"].as_object().cloned().unwrap_or_default(),
        files,
    ) {
        Ok(p) => p.under(crate::reservations::JobScope {
            project: intent["project_id"].as_str().map(String::from),
            conversation: intent["conversation_id"].as_str().map(String::from),
        }),
        Err(e) => return invalid(&e),
    };
    if want["source"].get("from_job").is_some()
        && let Err(e) = plan.fill_missing_check(&state.store)
    {
        return internal(&e);
    }
    if let Err(e) = plan.stamp(&state.store, intent_id, stamped.as_deref()) {
        return invalid(&format!("{e:?}"));
    }
    let made = (|| -> Result<String, String> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        let id = plan.insert(&state.store, &tx, Some(intent_id))?;
        touch(&tx, Record::Intent, intent_id).map_err(|e| e.to_string())?;
        remember(&tx, &key, &sha, Record::Job, &id, client).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(id)
    })();
    match made {
        Ok(id) => render(&db, 201, Record::Job, &id, false),
        Err(e) => internal(&e),
    }
}

/// `POST runs/{id}/decision`: record approve or reject of one run's result. It authorises nothing further
/// Recording a decision does not authorise further work on the run. A run is decided once.
pub fn decide(state: &State, call: &Call, client: &Client, run_id: &str) -> Reply {
    let (key, m) = match (key(call), body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    let Some(decision) = m
        .get("decision")
        .and_then(Value::as_str)
        .filter(|d| matches!(*d, "approve" | "reject"))
    else {
        return invalid("decision must be approve or reject");
    };
    let note = match text(&m, "note", 4000) {
        Ok(n) => n,
        Err(r) => return r.reply(),
    };
    let sha = content(
        "decision",
        run_id,
        &json!({"decision": decision, "note": note}),
    );
    let mut db = state.store.db();
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some((kind, id))) => return render(&db, 200, kind, &id, true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let existing = db
        .query_row(
            "SELECT (SELECT decision_id FROM decisions WHERE run_id = ?1) FROM runs WHERE run_id = ?1",
            params![run_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional();
    match existing {
        Err(e) => return internal(&e),
        Ok(None) => return not_found("run"),
        Ok(Some(Some(earlier))) => {
            let receipt = records::one(&db, Record::Decision, &earlier).ok().flatten();
            return refusal(
                409,
                "already_decided",
                "this run's result is already decided",
                json!({"decision": receipt}),
            );
        }
        Ok(Some(None)) => {}
    }
    let made = (|| -> Result<String, String> {
        let id = format!("dec_{}", util::ulid()?.to_lowercase());
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO decisions (decision_id, run_id, decision, note, decided_by, decided_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, run_id, decision, note, client.id, now_utc()],
        )
        .map_err(|e| e.to_string())?;
        touch(&tx, Record::Decision, &id).map_err(|e| e.to_string())?;
        touch(&tx, Record::Run, run_id).map_err(|e| e.to_string())?;
        remember(&tx, &key, &sha, Record::Decision, &id, client).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(id)
    })();
    match made {
        Ok(id) => {
            crate::logf!("run {run_id}: {decision} by {} ({id})", client.id);
            render(&db, 201, Record::Decision, &id, false)
        }
        Err(e) => internal(&e),
    }
}

/// `POST jobs/{id}/cancel`: record a cancel request and the control message that carries it to the job's node
/// (NODE-PROTOCOL.md §9.1). It is requested until the node reports. A job has at most one; asking again answers
/// with it.
pub fn cancel(state: &State, call: &Call, client: &Client, job_id: &str) -> Reply {
    let (key, m) = match (key(call), body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    let reason = match text(&m, "reason", 4000) {
        Ok(r) => r,
        Err(r) => return r.reply(),
    };
    let sha = content("cancel", job_id, &json!({"reason": reason}));
    let mut db = state.store.db();
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some((kind, id))) => return render(&db, 200, kind, &id, true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let job = db
        .query_row(
            "SELECT state, (SELECT cancel_id FROM cancels WHERE job_id = ?1), node_id FROM jobs WHERE job_id = ?1",
            params![job_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?)),
        )
        .optional();
    let made = match job {
        Err(e) => return internal(&e),
        Ok(None) => return not_found("job"),
        Ok(Some((_, Some(earlier), _))) => (|| -> Result<(u16, String), String> {
            remember(&db, &key, &sha, Record::Cancel, &earlier, client)
                .map_err(|e| e.to_string())?;
            Ok((200, earlier))
        })(),
        Ok(Some((s, None, _))) if matches!(s.as_str(), "done" | "error") => {
            return refusal(
                409,
                "job_finished",
                &format!("the job is already {s}; there is nothing to cancel"),
                Value::Null,
            );
        }
        Ok(Some((_, None, node_id))) => (|| -> Result<(u16, String), String> {
            let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
            let id = issue_cancel(&tx, job_id, node_id, reason.as_deref(), &client.id)?;
            // A turn whose waiting job was stopped here ends now.
            crate::settle::settle_job(&state.store, &tx, job_id)?;
            remember(&tx, &key, &sha, Record::Cancel, &id, client).map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok((202, id))
        })(),
    };
    match made {
        Ok((status, id)) => render(&db, status, Record::Cancel, &id, false),
        Err(e) => internal(&e),
    }
}

/// Record a cancel request for a job and the control message that carries it, inside the caller's transaction.
/// Returns the cancel's id.
pub fn issue_cancel(
    tx: &Connection,
    job_id: &str,
    node_id: i64,
    reason: Option<&str>,
    requested_by: &str,
) -> Result<String, String> {
    let id = format!("can_{}", util::ulid()?.to_lowercase());
    let control_id = crate::control::issue(tx, node_id, "cancel", &json!({"job_id": job_id}))?;
    tx.execute(
        "INSERT INTO cancels (cancel_id, job_id, state, reason, requested_by, requested_at, control_id)
         VALUES (?1, ?2, 'requested', ?3, ?4, ?5, ?6)",
        params![id, job_id, reason, requested_by, now_utc(), control_id],
    )
    .map_err(|e| e.to_string())?;
    touch(tx, Record::Cancel, &id).map_err(|e| e.to_string())?;
    touch(tx, Record::Job, job_id).map_err(|e| e.to_string())?;
    // A job waiting for budget never reached its node: it is stopped here, and its control message settled, so
    // the node is never asked.
    let waiting = tx
        .query_row(
            "SELECT 1 FROM jobs WHERE job_id = ?1 AND state = 'waiting'",
            params![job_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .is_some();
    if waiting {
        tx.execute(
            "UPDATE controls SET state = 'applied', outcome = 'stopped', settled_at = ?1,
                                 detail = 'stopped before it reached its node: it was waiting for budget'
             WHERE control_id = ?2",
            params![now_utc(), control_id],
        )
        .map_err(|e| e.to_string())?;
        crate::reservations::cancel_waiting(tx, job_id)?;
    }
    Ok(id)
}
