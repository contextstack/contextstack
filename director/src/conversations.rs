//! The Client API's conversations (CLIENT-API.md §11): create one on a spec, post turns, stop and
//! resume, read the whole thread, and redo a summary. Every write carries an `Idempotency-Key` (§7).
//!
//! | Call | Answer |
//! | --- | --- |
//! | `POST conversations` (key) `{node, spec, files \| from_job, title?, intent_id?}` or `{node, model, title?}` | 201 `{conversation}` |
//! | `GET conversations[?node=&intent=&lead=1&project=ID&limit=N]`, `GET conversations/{id}` | `{conversations}`, `{conversation}` |
//! | `GET conversations/{id}/thread` | `{conversation, turns, summaries}`: the whole thread, always |
//! | `POST conversations/{id}/turns` (key) `{message}` | 201 `{turn, conversation}` |
//! | `POST conversations/{id}/wake-turns` (key) `{nudge, reply, jobs?}` | 201 `{turn, conversation}`: `origin: wake` (hosted lead only) |
//! | `POST conversations/{id}/stop` (key) | 202 `{conversation, cancel}` |
//! | `POST conversations/{id}/resume` (key) | 200 `{conversation, cancel}` |
//! | `POST conversations/{id}/summaries/{sid}/redo` (key) | 202 `{summary}` |
//! | `PUT conversations/{id}/attention` `{question, job_id?}` | 200 `{conversation}` |
//! | `DELETE conversations/{id}/attention` | 200 `{conversation}` |
//! | `GET turns/{id}`, `GET summaries/{id}` | one record |

use crate::access::{Client, Kind};
use crate::jobs::{self, BundleFile, Unusable};
use crate::records;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::{Record, touch};
use crate::thread;
use crate::util::{self, now_utc};
use crate::work::{No, body, content, key, remember, scoped_prior};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

/// The longest message a turn takes.
const MAX_MESSAGE: usize = 200_000;
/// The longest attention question.
const MAX_ATTENTION: usize = 4_000;
/// The longest optional job id on attention.
const MAX_ATTENTION_JOB: usize = 128;

/// Serve a call under `conversations`, `turns` or `summaries`.
pub fn handle(state: &State, call: &Call, client: &Client, parts: &[&str]) -> Reply {
    let db = || state.store.db();
    let found = match (call.method.as_str(), parts) {
        ("PATCH", ["conversations", id]) => {
            return crate::chief_of_staff::rename(state, call, client, id);
        }
        ("DELETE", ["conversations", id]) => return delete(state, call, client, id),
        ("POST", ["conversations", id, word @ ("archive" | "unarchive")]) => {
            return archive(state, call, client, id, word);
        }
        ("POST", ["conversations"]) => return create(state, call, client),
        ("GET", ["conversations"]) => return list_conversations(&db(), call),
        ("GET", ["conversations", id]) => answer(&db(), id, false),
        ("GET", ["conversations", id, "thread"]) => thread_of(&db(), id),
        ("POST", ["conversations", id, "turns"]) => return post_turn(state, call, client, id),
        ("POST", ["conversations", id, "wake-turns"]) => {
            return post_wake_turn(state, call, client, id);
        }
        ("POST", ["conversations", id, word @ ("stop" | "resume")]) => {
            return hold(state, call, client, id, word);
        }
        ("POST", ["conversations", id, "summaries", sid, "redo"]) => {
            return redo(state, call, client, id, sid);
        }
        ("PUT", ["conversations", id, "attention"]) => {
            return set_attention(state, call, client, id);
        }
        ("DELETE", ["conversations", id, "attention"]) => {
            return clear_attention(state, client, id);
        }
        ("GET", ["turns", id]) => {
            records::one(&db(), Record::Turn, id).map(Option::unwrap_or_default)
        }
        ("GET", ["summaries", id]) => {
            records::one(&db(), Record::Summary, id).map(Option::unwrap_or_default)
        }
        _ => Ok(Value::Null),
    };
    match found {
        Ok(Value::Null) => refusal(404, "not_found", "no such route or record", Value::Null),
        Ok(v) => json(200, &v),
        Err(e) => internal(&e),
    }
}

fn list_conversations(db: &Connection, call: &Call) -> Reply {
    let limit = match call.query("limit") {
        None => records::MAX_INTENTS,
        Some(raw) => match raw
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=records::MAX_INTENTS).contains(n))
        {
            Some(n) => n,
            None => {
                return refusal(
                    422,
                    "invalid_request",
                    "limit must be from 1 to 500",
                    Value::Null,
                );
            }
        },
    };
    let lead_only = match call.query("lead") {
        None => false,
        Some("1") => true,
        Some(_) => return refusal(422, "invalid_request", "lead must be 1", Value::Null),
    };
    let archived = match call.query("archived") {
        None => Some(false),
        Some("1") => Some(true),
        Some("all") => None,
        Some(_) => return No::invalid("archived must be 1 or all").reply(),
    };
    let project = match call.query("project") {
        None => None,
        Some(id) => {
            if let Err(no) = crate::work::project_exists(db, Some(id)) {
                return no.reply();
            }
            Some(id)
        }
    };
    match records::list_conversations(
        db,
        call.query("node"),
        call.query("intent"),
        lead_only,
        project,
        limit,
        archived,
    ) {
        Ok(conversations) => json(200, &json!({"conversations": conversations})),
        Err(e) => internal(&e),
    }
}

/// `{conversation}`, with the active turn's `cancel` when asked; null for an unknown conversation.
fn answer(db: &Connection, id: &str, with_cancel: bool) -> rusqlite::Result<Value> {
    let Some(conversation) = records::one(db, Record::Conversation, id)? else {
        return Ok(Value::Null);
    };
    let mut out = json!({"conversation": conversation});
    if with_cancel {
        let cancel: Option<String> = db
            .query_row(
                "SELECT c.cancel_id FROM turns t JOIN cancels c ON c.job_id = t.job_id
                 WHERE t.conversation_id = ?1 ORDER BY t.seq DESC LIMIT 1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        out["cancel"] = match cancel {
            Some(c) => records::one(db, Record::Cancel, &c)?.unwrap_or_default(),
            None => Value::Null,
        };
    }
    Ok(out)
}

fn thread_of(db: &Connection, id: &str) -> rusqlite::Result<Value> {
    let Some(conversation) = records::one(db, Record::Conversation, id)? else {
        return Ok(Value::Null);
    };
    let turns = records::select(db, Record::Turn, "t.conversation_id = ?1", &id)?;
    let summaries = records::select(db, Record::Summary, "s.conversation_id = ?1", &id)?;
    Ok(json!({"conversation": conversation, "turns": turns, "summaries": summaries}))
}

fn reply_with(status: u16, v: rusqlite::Result<Value>, replayed: bool) -> Reply {
    match v {
        Ok(Value::Null) => refusal(404, "not_found", "no such record", Value::Null),
        Ok(v) => {
            let mut reply = json(status, &v);
            if replayed && let Ok(h) = "true".parse() {
                reply.headers_mut().insert("idempotent-replayed", h);
            }
            reply
        }
        Err(e) => internal(&e),
    }
}

/// The body of a create, normalised so equal requests hash equal.
fn normalise(m: &Map<String, Value>) -> Result<Value, No> {
    let need = |f: &str| {
        m.get(f)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .ok_or_else(|| No::invalid(&format!("a conversation names its {f}")))
    };
    let opt = |f: &str, max: usize| match m.get(f) {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::String(s)) if s.len() <= max => Ok(json!(s)),
        Some(_) => Err(No::invalid(&format!(
            "{f} must be a string of at most {max} bytes"
        ))),
    };
    let spec = need("spec")?;
    let source = match (m.get("files"), m.get("from_job")) {
        (Some(f), None) => json!({"files": f}),
        (None, Some(Value::String(j))) => json!({"from_job": j}),
        _ => return Err(No::invalid("give exactly one of files or from_job")),
    };
    let mut want = json!({"node": need("node")?, "spec": spec, "title": opt("title", 200)?,
                          "intent_id": opt("intent_id", 128)?, "source": source});
    // Only when given, so a create without it hashes as it did before projects existed.
    if let Value::String(p) = opt("project_id", 128)? {
        want["project_id"] = json!(p);
    }
    Ok(want)
}

/// The conversation's project: the one named, else its intent's. Naming another than its intent's is refused.
fn project_of(db: &Connection, want: &Value) -> Result<Option<String>, No> {
    let named = want["project_id"].as_str().map(String::from);
    crate::work::project_exists(db, named.as_deref())?;
    let Some(intent) = want["intent_id"].as_str() else {
        return Ok(named);
    };
    let of_intent: Option<String> = db
        .query_row(
            "SELECT project_id FROM intents WHERE intent_id = ?1",
            params![intent],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| No::internal(&e))?
        .flatten();
    match (named, of_intent) {
        (Some(n), Some(i)) if n != i => Err(No::invalid(
            "project_id differs from the project of the intent it attaches to",
        )),
        (n, i) => Ok(n.or(i)),
    }
}

/// The spec's facts and its instructions' size, checked against the bundle.
pub(crate) fn check_bundle(
    state: &State,
    spec: &str,
    files: &[BundleFile],
) -> Result<(thread::SpecFacts, i64), No> {
    let find = |p: &str| files.iter().find(|f| f.path == p);
    if let Some(f) = files
        .iter()
        .find(|f| f.path == thread::CONTEXT_PATH || f.path.starts_with("tasks/turn/"))
    {
        return Err(No::invalid(&format!(
            "{} is written by the Director for each turn; leave it out of the bundle",
            f.path
        )));
    }
    let spec_file = find(spec).ok_or_else(|| No::invalid(&format!("the bundle has no {spec}")))?;
    let yaml = state
        .store
        .read_object(&spec_file.sha256)
        .ok_or_else(|| No::invalid("the spec's object is not stored"))?;
    let facts = thread::read_spec(&yaml).map_err(|e| No::invalid(&e))?;
    let mut bytes = 0;
    for p in &facts.prompts {
        bytes += find(p)
            .ok_or_else(|| No::invalid(&format!("the bundle has no {p}, which the spec names")))?
            .bytes;
    }
    Ok((facts, i64::try_from(bytes.div_ceil(4)).unwrap_or(i64::MAX)))
}

/// Preserve the template bytes apart from the first top-level model line, like `cs_lead.bundle_files`.
fn lead_spec(template: &str, model: &str) -> String {
    let mut replaced = false;
    template
        .split_inclusive('\n')
        .map(|line| {
            if !replaced && line.starts_with("model: ") && line.ends_with('\n') {
                replaced = true;
                format!("model: {model}\n")
            } else {
                line.to_string()
            }
        })
        .collect()
}

fn collect_lead_bundle_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_lead_bundle_files(root, &entry.path(), out)?;
        } else if entry.file_type()?.is_file() {
            let rel = entry
                .path()
                .strip_prefix(root)
                .map(std::path::Path::to_path_buf)
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "bundle path outside root")
                })?;
            out.push(rel);
        }
    }
    Ok(())
}

pub(crate) fn lead_bundle(state: &State, m: &mut Map<String, Value>) -> Result<(), No> {
    let model = m
        .get("model")
        .and_then(Value::as_str)
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
        })
        .ok_or_else(|| No::invalid("a hosted-lead conversation names its model"))?;
    if !thread::lead_model_ok(model) {
        return Err(No(
            422,
            "lead_model_unsupported",
            format!(
                "model {model} cannot host a lead; choose one of {}",
                thread::LEAD_MODELS.join(", ")
            ),
            thread::lead_models_detail(),
        ));
    }
    if m.contains_key("files") || m.contains_key("from_job") {
        return Err(No::invalid("model cannot be combined with a custom bundle"));
    }
    let root = std::env::var_os("CS_LEAD_BUNDLE_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()?
                .parent()
                .map(|p| p.join("cs/lead"))
        })
        .ok_or_else(|| {
            No(
                422,
                "lead_bundle_missing",
                "installed lead bundle is missing".into(),
                Value::Null,
            )
        })?;
    if !root.is_dir() || !root.join("spec.yaml").is_file() {
        return Err(No(
            422,
            "lead_bundle_missing",
            "installed lead bundle is missing".into(),
            Value::Null,
        ));
    }
    let mut paths = Vec::new();
    collect_lead_bundle_files(&root, &root, &mut paths).map_err(|e| No::internal(&e))?;
    paths.sort();
    let mut files = Vec::new();
    for path in paths {
        let mut data = std::fs::read(root.join(&path)).map_err(|e| No::internal(&e))?;
        if path == std::path::Path::new("spec.yaml") {
            let template = std::str::from_utf8(&data).map_err(|e| No::internal(&e))?;
            data = lead_spec(template, model).into_bytes();
        }
        state
            .store
            .put_object(&data)
            .map_err(|e| No::internal(&e))?;
        files.push(
            json!({"path": path.to_string_lossy(), "sha256": util::sha256_hex(&data), "mode": 420}),
        );
    }
    m.insert("spec".into(), json!("spec.yaml"));
    m.insert("files".into(), json!(files));
    Ok(())
}

const CHAT_SYSTEM: &str =
    "You are a helpful assistant in a conversation. The thread so far is in your context.\n";

fn chat_spec(model: &str) -> String {
    format!(
        "id: chat\nmodel: \"{model}\"\nsystem: prompts/system.md\ncontext: [conversation/context.md]\ntools: []\nmcp: []\nbudget: {{turns: 4, tool_calls: 0, usd: 0, seconds: 300}}\ntasks: [tasks/turn]\ntrials: 1\n"
    )
}

fn default_bundle(state: &State, m: &mut Map<String, Value>) -> Result<(), No> {
    if m.get("spec").and_then(Value::as_str) == Some(thread::HOSTED_LEAD_SPEC_ID) {
        return lead_bundle(state, m);
    }
    let built_chat = m.get("spec").and_then(Value::as_str) == Some("chat");
    if built_chat && !m.contains_key("model") {
        return Err(No::invalid("a chat conversation names its model"));
    }
    let Some(model) = m.get("model") else {
        return Ok(());
    };
    let model = model
        .as_str()
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
        })
        .ok_or_else(|| No::invalid("choose a valid model"))?;
    if m.contains_key("files")
        || m.contains_key("from_job")
        || (m.contains_key("spec") && !built_chat)
    {
        return Err(No::invalid("model cannot be combined with a custom bundle"));
    }
    if built_chat {
        let catalog = crate::models::list(&state.settings.gateway).map_err(|e| match e {
            crate::gateway::Fault::Unavailable(message) => {
                No(502, "gateway_unavailable", message, Value::Null)
            }
            crate::gateway::Fault::Token(message) => {
                No(502, "gateway_owner_token", message, Value::Null)
            }
        })?;
        if !catalog["models"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["name"].as_str() == Some(model)))
        {
            return Err(No::invalid(&format!("unknown model {model}")));
        }
    }
    let spec = chat_spec(model);
    let mut files = Vec::new();
    for (path, data) in [
        ("spec.yaml", spec.as_bytes()),
        ("prompts/system.md", CHAT_SYSTEM.as_bytes()),
    ] {
        state.store.put_object(data).map_err(|e| No::internal(&e))?;
        files.push(json!({"path": path, "sha256": util::sha256_hex(data), "bytes": data.len(), "mode": 420}));
    }
    m.insert("spec".into(), json!("spec.yaml"));
    m.insert("files".into(), json!(files));
    Ok(())
}

fn conversation_source_files(
    state: &State,
    db: &Connection,
    want: &Value,
) -> Result<Vec<BundleFile>, Box<Reply>> {
    match &want["source"] {
        Value::Object(s) if s.contains_key("from_job") => {
            let job = s["from_job"].as_str().unwrap_or_default();
            match jobs::bundle_of_job(db, job) {
                Ok(f) if !f.is_empty() => Ok(f),
                Ok(_) => Err(Box::new(
                    No::invalid(&format!("from_job: no job {job}")).reply(),
                )),
                Err(e) => Err(Box::new(internal(&e))),
            }
        }
        s => match jobs::bundle_of_objects(&state.store, &s["files"]) {
            Ok(f) => Ok(f),
            Err(Unusable::Invalid(e)) => Err(Box::new(No::invalid(&e).reply())),
            Err(Unusable::Missing(missing)) => Err(Box::new(refusal(
                422,
                "objects_missing",
                "upload these objects first (PUT /api/v1/client/objects/{sha256})",
                json!({"missing": missing}),
            ))),
        },
    }
}

fn validate_create_targets(db: &Connection, want: &Value, node: &str) -> Result<(), Box<Reply>> {
    let known = |sql: &str, v: &str| {
        db.query_row(sql, params![v], |_| Ok(()))
            .optional()
            .map(|o| o.is_some())
    };
    match known(
        "SELECT 1 FROM nodes WHERE name = ?1 AND revoked_at IS NULL",
        node,
    ) {
        Ok(true) => {}
        Ok(false) => {
            return Err(Box::new(
                No::invalid(&format!("no live node named {node}")).reply(),
            ));
        }
        Err(e) => return Err(Box::new(internal(&e))),
    }
    if let Some(intent) = want["intent_id"].as_str() {
        match known("SELECT 1 FROM intents WHERE intent_id = ?1", intent) {
            Ok(true) => {}
            Ok(false) => {
                return Err(Box::new(
                    No::invalid(&format!("no intent {intent}")).reply(),
                ));
            }
            Err(e) => return Err(Box::new(internal(&e))),
        }
    }
    Ok(())
}

/// `POST conversations`: a conversation on a spec bundle, talking to one node.
fn create(state: &State, call: &Call, client: &Client) -> Reply {
    let (key, mut m) = match (key(call), body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    // Only the Director builds a hosted-lead bundle (from the installed release); a client bundle may not claim the id.
    let built_lead = m.get("spec").and_then(Value::as_str) == Some(thread::HOSTED_LEAD_SPEC_ID);
    let built_chat = m.get("spec").and_then(Value::as_str) == Some("chat");
    if let Err(e) = default_bundle(state, &mut m) {
        return e.reply();
    }
    if let Err(e) = crate::lead_node::apply_create_node(&state.store.db(), &mut m, built_lead) {
        return e.reply();
    }
    if built_chat && m.get("node").is_none_or(Value::is_null) {
        match crate::lead_node::effective(&state.store.db()) {
            Ok(Some(node)) => {
                m.insert("node".into(), json!(node));
            }
            Ok(None) => return No::invalid("register a node before creating a chat").reply(),
            Err(e) => return No::internal(&e).reply(),
        }
    }
    let want = match normalise(&m) {
        Ok(w) => w,
        Err(r) => return r.reply(),
    };
    let sha = content("conversation", "", &want);
    let mut db = state.store.db();
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some((_, id))) => return reply_with(200, answer(&db, &id, false), true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let files = match conversation_source_files(state, &db, &want) {
        Ok(f) => f,
        Err(r) => return *r,
    };
    let s = |f: &str| want[f].as_str().unwrap_or_default().to_string();
    let (facts, instructions) = match check_bundle(state, &s("spec"), &files) {
        Ok(f) => f,
        Err(r) => return r.reply(),
    };
    let lead = files
        .iter()
        .find(|f| f.path == s("spec"))
        .and_then(|f| state.store.read_object(&f.sha256))
        .is_some_and(|y| thread::is_lead_spec(&y));
    if lead && !built_lead {
        return refusal(
            422,
            "lead_bundle_reserved",
            "a hosted-lead conversation is created with spec \"hosted-lead\" and a model; its bundle comes from the installed release",
            Value::Null,
        );
    }
    if let Err(r) = validate_create_targets(&db, &want, &s("node")) {
        return *r;
    }
    let project = match project_of(&db, &want) {
        Ok(p) => p,
        Err(r) => return r.reply(),
    };
    let made = (|| -> Result<String, String> {
        let id = format!("con_{}", util::ulid()?.to_lowercase());
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        let spec_version = jobs::bundle_sha(&files);
        let lead_i = i64::from(lead);
        tx.execute(
            "INSERT INTO conversations (conversation_id, title, node, spec, spec_version, model, instructions_tokens,
                                        tools, intent_id, created_by, created_at, project_id, lead)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![id, want["title"].as_str(), s("node"), s("spec"), spec_version, facts.model,
                    instructions, facts.tools, want["intent_id"].as_str(), client.id, now_utc(), project, lead_i],
        )
        .map_err(|e| e.to_string())?;
        for f in &files {
            tx.execute(
                "INSERT INTO conversation_files (conversation_id, path, sha256, bytes, mode) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, f.path, f.sha256, i64::try_from(f.bytes).unwrap_or(i64::MAX), f.mode],
            )
            .map_err(|e| e.to_string())?;
        }
        touch(&tx, Record::Conversation, &id).map_err(|e| e.to_string())?;
        if let Some(intent) = want["intent_id"].as_str() {
            touch(&tx, Record::Intent, intent).map_err(|e| e.to_string())?;
        }
        remember(&tx, &key, &sha, Record::Conversation, &id, client).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(id)
    })();
    match made {
        Ok(id) => reply_with(201, answer(&db, &id, false), false),
        Err(e) => internal(&e),
    }
}

fn exists(db: &Connection, id: &str) -> Result<(), Box<Reply>> {
    match db
        .query_row(
            "SELECT 1 FROM conversations WHERE conversation_id = ?1",
            params![id],
            |_| Ok(()),
        )
        .optional()
    {
        Ok(Some(())) => Ok(()),
        Ok(None) => Err(Box::new(refusal(
            404,
            "not_found",
            "no such conversation",
            Value::Null,
        ))),
        Err(e) => Err(Box::new(internal(&e))),
    }
}

fn turn_answer(db: &Connection, turn: &str) -> rusqlite::Result<Value> {
    let Some(t) = records::one(db, Record::Turn, turn)? else {
        return Ok(Value::Null);
    };
    let conversation = records::one(
        db,
        Record::Conversation,
        t["conversation_id"].as_str().unwrap_or(""),
    )?;
    Ok(json!({"turn": t, "conversation": conversation}))
}

/// A wake turn's `nudge` or `reply`: a non-empty string of at most `MAX_MESSAGE` bytes.
fn wake_text<'a>(m: &'a Map<String, Value>, field: &str) -> Result<&'a str, No> {
    m.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= MAX_MESSAGE)
        .ok_or_else(|| {
            No::invalid(&format!(
                "{field} is a non-empty string of at most 200000 bytes"
            ))
        })
}

/// A wake turn's `jobs`: absent, null, or at most 100 job id strings.
fn wake_jobs(m: &Map<String, Value>) -> Result<Vec<&str>, No> {
    match m.get("jobs") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) if a.len() <= 100 => a
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| No::invalid("jobs is a list of job id strings")),
        Some(_) => Err(No::invalid("jobs is a list of at most 100 job id strings")),
    }
}

/// `POST conversations/{id}/wake-turns` `{nudge, reply, jobs?}`: record a lane wake-up already answered in the lead session.
/// The turn has `origin: "wake"`, an empty `message`, the follow-up as `reply`, and `detail: {nudge, jobs}`. It needs no
/// job and may land while a person's turn is queued or running: it is already done, so it never blocks the queue.
fn post_wake_turn(state: &State, call: &Call, client: &Client, id: &str) -> Reply {
    if client.kind != Kind::Lead || client.conversation_id.as_deref() != Some(id) {
        return refusal(
            403,
            "owner_only",
            "only the hosted lead for this conversation may record a wake reply",
            Value::Null,
        );
    }
    let (key, m) = match (key(call), body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    let (nudge, reply, jobs) = match (
        wake_text(&m, "nudge"),
        wake_text(&m, "reply"),
        wake_jobs(&m),
    ) {
        (Ok(n), Ok(r), Ok(j)) => (n, r, j),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e.reply(),
    };
    let detail = json!({"nudge": nudge, "jobs": jobs}).to_string();
    let sha = content(
        "wake_turn",
        id,
        &json!({"nudge": nudge, "reply": reply, "jobs": jobs}),
    );
    let mut db = state.store.db();
    for job in &jobs {
        match crate::lead::visible(&db, client, Record::Job, job) {
            Ok(true) => {}
            Ok(false) => return No::invalid(&format!("jobs: {job}")).reply(),
            Err(e) => return internal(&e),
        }
    }
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some((_, turn))) => return reply_with(200, turn_answer(&db, &turn), true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    if let Err(r) = exists(&db, id) {
        return *r;
    }
    if let Err(e) = require_unarchived(&db, id) {
        return e.reply();
    }
    let lead = db
        .query_row(
            "SELECT lead FROM conversations WHERE conversation_id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| e.to_string());
    let lead = match lead {
        Ok(Some(v)) => v != 0,
        Ok(None) => {
            return refusal(404, "not_found", "no such conversation", Value::Null);
        }
        Err(e) => return internal(&e),
    };
    if !lead {
        return No::invalid("wake replies are only for hosted-lead conversations").reply();
    }
    let made = (|| -> Result<String, No> {
        let turn = format!(
            "trn_{}",
            util::ulid().map_err(|e| No::internal(&e))?.to_lowercase()
        );
        let now = now_utc();
        let tx = crate::store::write_tx(&mut db).map_err(|e| No::internal(&e))?;
        require_unarchived(&tx, id)?;
        tx.execute(
            "INSERT INTO turns (turn_id, conversation_id, seq, message, state, posted_by, posted_at, reply, finished_at,
                                origin, detail)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE conversation_id = ?2), '',
                     'done', ?3, ?4, ?5, ?4, 'wake', ?6)",
            params![turn, id, client.id, now, reply, detail],
        )
        .map_err(|e| No::internal(&e))?;
        touch(&tx, Record::Turn, &turn).map_err(|e| No::internal(&e))?;
        touch(&tx, Record::Conversation, id).map_err(|e| No::internal(&e))?;
        remember(&tx, &key, &sha, Record::Turn, &turn, client).map_err(|e| No::internal(&e))?;
        tx.commit().map_err(|e| No::internal(&e))?;
        Ok(turn)
    })();
    match made {
        Ok(turn) => reply_with(201, turn_answer(&db, &turn), false),
        Err(e) => e.reply(),
    }
}

/// `POST conversations/{id}/turns`: queue a message. It starts at once when nothing runs and the queue is not held.
fn post_turn(state: &State, call: &Call, client: &Client, id: &str) -> Reply {
    let (key, m) = match (key(call), body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    let Some(message) = m
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= MAX_MESSAGE)
    else {
        return No::invalid("message is a non-empty string of at most 200000 bytes").reply();
    };
    let sha = content("turn", id, &json!({"message": message}));
    let mut db = state.store.db();
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some((_, turn))) => return reply_with(200, turn_answer(&db, &turn), true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    if let Err(r) = exists(&db, id) {
        return *r;
    }
    let made = (|| -> Result<String, No> {
        let turn = format!(
            "trn_{}",
            util::ulid().map_err(|e| No::internal(&e))?.to_lowercase()
        );
        let tx = crate::store::write_tx(&mut db).map_err(|e| No::internal(&e))?;
        require_unarchived(&tx, id)?;
        clear_attention_cols(&tx, id).map_err(|e| No::internal(&e))?;
        tx.execute(
            "INSERT INTO turns (turn_id, conversation_id, seq, message, state, posted_by, posted_at)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE conversation_id = ?2), ?3,
                     'queued', ?4, ?5)",
            params![turn, id, message, client.id, now_utc()],
        )
        .map_err(|e| No::internal(&e))?;
        touch(&tx, Record::Turn, &turn).map_err(|e| No::internal(&e))?;
        touch(&tx, Record::Conversation, id).map_err(|e| No::internal(&e))?;
        crate::turns::advance(&state.store, &tx, id).map_err(|e| No::internal(&e))?;
        remember(&tx, &key, &sha, Record::Turn, &turn, client).map_err(|e| No::internal(&e))?;
        tx.commit().map_err(|e| No::internal(&e))?;
        Ok(turn)
    })();
    match made {
        Ok(turn) => reply_with(201, turn_answer(&db, &turn), false),
        Err(e) => e.reply(),
    }
}

fn clear_attention_cols(db: &Connection, id: &str) -> rusqlite::Result<()> {
    db.execute(
        "UPDATE conversations SET attention_question = NULL, attention_job_id = NULL,
         attention_set_at = NULL, attention_set_by = NULL
         WHERE conversation_id = ?1 AND attention_question IS NOT NULL",
        params![id],
    )?;
    Ok(())
}

/// `PUT conversations/{id}/attention`: the lead or owner asks for a decision; the queue keeps running.
fn set_attention(state: &State, call: &Call, client: &Client, id: &str) -> Reply {
    let m = match body(call) {
        Ok(m) => m,
        Err(r) => return r.reply(),
    };
    let Some(question) = m
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= MAX_ATTENTION)
    else {
        return No::invalid(&format!(
            "question is a non-empty string of at most {MAX_ATTENTION} bytes"
        ))
        .reply();
    };
    let job_id = match m.get("job_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() && s.len() <= MAX_ATTENTION_JOB => Some(s.as_str()),
        _ => {
            return No::invalid(&format!(
                "job_id must be a non-empty string of at most {MAX_ATTENTION_JOB} bytes"
            ))
            .reply();
        }
    };
    let mut db = state.store.db();
    if let Err(r) = exists(&db, id) {
        return *r;
    }
    let done = (|| -> Result<(), String> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        let now = now_utc();
        tx.execute(
            "UPDATE conversations SET attention_question = ?2, attention_job_id = ?3,
             attention_set_at = ?4, attention_set_by = ?5 WHERE conversation_id = ?1",
            params![id, question, job_id, now, client.id],
        )
        .map_err(|e| e.to_string())?;
        touch(&tx, Record::Conversation, id).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    })();
    match done {
        Ok(()) => reply_with(200, answer(&db, id, false), false),
        Err(e) => internal(&e),
    }
}

/// `DELETE conversations/{id}/attention`: answered elsewhere; recorded like any other conversation change.
fn clear_attention(state: &State, client: &Client, id: &str) -> Reply {
    let mut db = state.store.db();
    if let Err(r) = exists(&db, id) {
        return *r;
    }
    let had = db
        .query_row(
            "SELECT attention_question IS NOT NULL FROM conversations WHERE conversation_id = ?1",
            params![id],
            |r| r.get::<_, bool>(0),
        )
        .map_err(|e| e.to_string());
    let had = match had {
        Ok(v) => v,
        Err(e) => return internal(&e),
    };
    let done = (|| -> Result<(), String> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        clear_attention_cols(&tx, id).map_err(|e| e.to_string())?;
        if had {
            touch(&tx, Record::Conversation, id).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    })();
    match done {
        Ok(()) => {
            if had {
                crate::logf!("conversation {id}: attention cleared by {}", client.id);
            }
            reply_with(200, answer(&db, id, false), false)
        }
        Err(e) => internal(&e),
    }
}

/// `POST conversations/{id}/stop` or `resume`.
fn hold(state: &State, call: &Call, client: &Client, id: &str, word: &str) -> Reply {
    let key = match key(call) {
        Ok(k) => k,
        Err(r) => return r.reply(),
    };
    let sha = content(&format!("conversation_{word}"), id, &json!({}));
    let status = if word == "stop" { 202 } else { 200 };
    let mut db = state.store.db();
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some(_)) => return reply_with(200, answer(&db, id, true), true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    if let Err(r) = exists(&db, id) {
        return *r;
    }
    let done = (|| -> Result<(), String> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        if word == "stop" {
            crate::turns::stop(&state.store, &tx, id, &client.id)?;
        } else {
            crate::turns::resume(&state.store, &tx, id)?;
        }
        remember(&tx, &key, &sha, Record::Conversation, id, client).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    })();
    match done {
        Ok(()) => {
            crate::logf!("conversation {id}: {word} by {}", client.id);
            reply_with(status, answer(&db, id, true), false)
        }
        Err(e) => internal(&e),
    }
}

/// `POST conversations/{id}/summaries/{sid}/redo`: make the summary again, by a new job, for the same portion.
fn redo(state: &State, call: &Call, client: &Client, id: &str, sid: &str) -> Reply {
    let key = match key(call) {
        Ok(k) => k,
        Err(r) => return r.reply(),
    };
    let sha = content("summary_redo", sid, &json!({}));
    let mut db = state.store.db();
    match scoped_prior(&db, &key, &sha, client) {
        Ok(Some((_, made))) => {
            let v = records::one(&db, Record::Summary, &made).map(Option::unwrap_or_default);
            return reply_with(200, v.map(|s| json!({"summary": s})), true);
        }
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let old: Option<(i64, Option<String>, String)> = match db
        .query_row(
            "SELECT through_seq, based_on, state FROM summaries WHERE summary_id = ?1 AND conversation_id = ?2",
            params![sid, id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
    {
        Ok(o) => o,
        Err(e) => return internal(&e),
    };
    let Some((through, based_on, was)) = old else {
        return refusal(
            404,
            "not_found",
            "no such summary in this conversation",
            Value::Null,
        );
    };
    if was == "running" {
        return refusal(
            409,
            "summary_running",
            "this summary is still being made",
            Value::Null,
        );
    }
    let made = (|| -> Result<String, String> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        let new = crate::turns::start_summary(
            &state.store,
            &tx,
            id,
            through,
            based_on.as_deref(),
            Some(sid),
            &client.id,
        )?;
        remember(&tx, &key, &sha, Record::Summary, &new, client).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(new)
    })();
    match made {
        Ok(new) => {
            let v = records::one(&db, Record::Summary, &new).map(Option::unwrap_or_default);
            reply_with(202, v.map(|s| json!({"summary": s})), false)
        }
        Err(e) => internal(&e),
    }
}

fn require_unarchived(db: &Connection, id: &str) -> Result<(), No> {
    let archived: bool = db
        .query_row(
            "SELECT archived_at IS NOT NULL FROM conversations WHERE conversation_id = ?1",
            [id],
            |r| r.get(0),
        )
        .map_err(|e| No::internal(&e))?;
    if archived {
        return Err(No(
            409,
            "archived",
            "unarchive the conversation first".into(),
            Value::Null,
        ));
    }
    Ok(())
}

fn archive(state: &State, call: &Call, client: &Client, id: &str, word: &str) -> Reply {
    let key = match key(call) {
        Ok(k) => k,
        Err(e) => return e.reply(),
    };
    let sha = content(word, id, &json!({}));
    let mut db = state.store.db();
    let result = (|| -> Result<bool, No> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| No::internal(&e))?;
        let known: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE conversation_id = ?1)",
                [id],
                |r| r.get(0),
            )
            .map_err(|e| No::internal(&e))?;
        if !known {
            return Err(No(
                404,
                "not_found",
                "no such conversation".into(),
                Value::Null,
            ));
        }
        if word == "archive" {
            crate::chief_of_staff::protect(&tx, id)?;
        }
        let replayed = scoped_prior(&tx, &key, &sha, client)?.is_some();
        if !replayed {
            let at = (word == "archive").then(now_utc);
            let changed = tx
                .execute(
                    "UPDATE conversations SET archived_at = ?2 WHERE conversation_id = ?1
                 AND (archived_at IS NULL) != (?2 IS NULL)",
                    params![id, at],
                )
                .map_err(|e| No::internal(&e))?;
            if changed > 0 {
                touch(&tx, Record::Conversation, id).map_err(|e| No::internal(&e))?;
            }
            remember(&tx, &key, &sha, Record::Conversation, id, client)
                .map_err(|e| No::internal(&e))?;
        }
        tx.commit().map_err(|e| No::internal(&e))?;
        Ok(replayed)
    })();
    match result {
        Ok(replayed) => reply_with(200, answer(&db, id, false), replayed),
        Err(e) => e.reply(),
    }
}

fn delete(state: &State, call: &Call, client: &Client, id: &str) -> Reply {
    if client.kind == Kind::Lead {
        return crate::lead::denied();
    }
    let key = match key(call) {
        Ok(k) => k,
        Err(e) => return e.reply(),
    };
    let sha = content("conversation_delete", id, &json!({}));
    let mut db = state.store.db();
    let result = (|| -> Result<Vec<String>, No> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| No::internal(&e))?;
        crate::chief_of_staff::protect(&tx, id)?;
        let sql = |q: &str| tx.execute(q, [id]).map_err(|e| No::internal(&e));
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE conversation_id = ?1)",
                [id],
                |r| r.get(0),
            )
            .map_err(|e| No::internal(&e))?;
        if !exists {
            return Err(No(
                404,
                "not_found",
                "no such conversation".into(),
                Value::Null,
            ));
        }
        scoped_prior(&tx, &key, &sha, client)?;
        let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM turns WHERE conversation_id = ?1 AND state NOT IN ('done', 'error', 'stopped'))", [id], |r| r.get(0)).map_err(|e| No::internal(&e))?;
        if busy {
            return Err(No(
                409,
                "busy",
                "stop the conversation first".into(),
                Value::Null,
            ));
        }
        let credentials = tx
            .prepare("SELECT id FROM credentials WHERE conversation_id = ?1 AND kind = 'lead'")
            .and_then(|mut s| {
                s.query_map([id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|e| No::internal(&e))?;
        sql(
            "DELETE FROM lead_objects WHERE credential_id IN (SELECT id FROM credentials WHERE conversation_id = ?1 AND kind = 'lead')",
        )?;
        sql(
            "UPDATE credentials SET revoked_at = COALESCE(revoked_at, strftime('%Y-%m-%dT%H:%M:%SZ', 'now')), conversation_id = NULL WHERE conversation_id = ?1",
        )?;
        let jobs = tx.prepare("SELECT job_id FROM turns WHERE conversation_id = ?1 AND job_id IS NOT NULL UNION SELECT job_id FROM summaries WHERE conversation_id = ?1 UNION SELECT job_id FROM jobs WHERE intent_id IN (SELECT intent_id FROM intents WHERE conversation_id = ?1 OR lead_conversation_id = ?1)").and_then(|mut s| s.query_map([id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()).map_err(|e| No::internal(&e))?;
        for job in jobs {
            touch(&tx, Record::Job, &job).map_err(|e| No::internal(&e))?;
        }
        sql("DELETE FROM conversation_files WHERE conversation_id = ?1")?;
        sql("DELETE FROM turns WHERE conversation_id = ?1")?;
        sql("DELETE FROM summaries WHERE conversation_id = ?1")?;
        let intents = tx.prepare("SELECT intent_id FROM intents WHERE conversation_id = ?1 OR lead_conversation_id = ?1").and_then(|mut s| s.query_map([id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()).map_err(|e| No::internal(&e))?;
        sql(
            "UPDATE intents SET conversation_id = CASE WHEN conversation_id = ?1 THEN NULL ELSE conversation_id END, lead_conversation_id = CASE WHEN lead_conversation_id = ?1 THEN NULL ELSE lead_conversation_id END WHERE conversation_id = ?1 OR lead_conversation_id = ?1",
        )?;
        for intent in intents {
            touch(&tx, Record::Intent, &intent).map_err(|e| No::internal(&e))?;
        }
        sql("UPDATE job_budgets SET conversation_id = NULL WHERE conversation_id = ?1")?;
        sql("DELETE FROM budgets WHERE scope = 'conversation' AND scope_id = ?1")?;
        touch(&tx, Record::Conversation, id).map_err(|e| No::internal(&e))?;
        sql("DELETE FROM conversations WHERE conversation_id = ?1")?;
        remember(&tx, &key, &sha, Record::Conversation, id, client)
            .map_err(|e| No::internal(&e))?;
        tx.commit().map_err(|e| No::internal(&e))?;
        Ok(credentials)
    })();
    match result {
        Ok(credentials) => {
            for credential in credentials {
                state.hub.revoked(&credential);
            }
            json(200, &json!({"deleted": id}))
        }
        Err(e) => e.reply(),
    }
}

#[cfg(test)]
mod lead_bundle_tests {
    #[test]
    fn python_and_rust_specs_match() {
        let Some(repo) = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent() else {
            panic!("manifest parent missing");
        };
        let Ok(template) = std::fs::read_to_string(repo.join("cs/lead/spec.yaml")) else {
            panic!("lead spec.yaml missing from repo");
        };
        for model in ["claude-code-sonnet", "claude-code-opus", "dspec-model"] {
            let Ok(output) = std::process::Command::new("python3")
                .arg("-c").arg("import sys; sys.path.insert(0, 'cs/scripts'); import cs_lead; sys.stdout.buffer.write(cs_lead.bundle_files(sys.argv[1])['spec.yaml'])")
                .arg(model).current_dir(repo).output() else {
                panic!("python3 cs_lead bundle_files failed to run");
            };
            assert!(output.status.success());
            assert_eq!(super::lead_spec(&template, model).as_bytes(), output.stdout);
        }
    }
}

#[cfg(test)]
mod chat_bundle_tests {
    #[test]
    fn chat_spec_has_desktop_fields() -> Result<(), Box<dyn std::error::Error>> {
        let spec = super::chat_spec("codex");
        let doc: serde_yaml::Value = serde_yaml::from_str(&spec)?;
        assert_eq!(doc["id"], "chat");
        assert_eq!(doc["model"], "codex");
        assert_eq!(doc["system"], "prompts/system.md");
        assert_eq!(doc["budget"]["turns"], 4);
        assert_eq!(doc["budget"]["seconds"], 300);
        assert_eq!(doc["budget"]["tool_calls"], 0);
        assert_eq!(doc["budget"]["usd"], 0);
        assert_eq!(doc["trials"], 1);
        assert_eq!(doc["tools"].as_sequence().map(Vec::len), Some(0));
        assert_eq!(doc["mcp"].as_sequence().map(Vec::len), Some(0));
        let facts = crate::thread::read_spec(spec.as_bytes()).map_err(|e| e.clone())?;
        assert_eq!(facts.prompts, ["prompts/system.md"]);
        assert!(!crate::thread::is_lead_spec(spec.as_bytes()));
        Ok(())
    }

    #[test]
    fn chat_prompt_and_model_substitution() -> Result<(), String> {
        assert_eq!(
            super::CHAT_SYSTEM,
            "You are a helpful assistant in a conversation. The thread so far is in your context.\n"
        );
        for model in ["codex", "provider/model:variant", "local-coder"] {
            let facts = crate::thread::read_spec(super::chat_spec(model).as_bytes())?;
            assert_eq!(facts.model, model);
        }
        Ok(())
    }
}
