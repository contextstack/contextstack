//! Hosted-lead authorization. All routes are denied unless explicitly allowed here.
use crate::access::{Client, Kind};
use crate::server::{Call, Reply, State, internal, refusal};
use crate::store::Record;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::fmt::Write as _;

pub fn denied() -> Reply {
    refusal(
        403,
        "owner_only",
        "only an owner credential may do this",
        Value::Null,
    )
}

/// Whether a record belongs to the lead's node and optional intent scope.
pub fn visible(db: &Connection, c: &Client, kind: Record, id: &str) -> rusqlite::Result<bool> {
    if c.kind != Kind::Lead {
        return Ok(true);
    }
    let node = c.node.as_deref();
    let intent = c.intent_id.as_deref();
    let exists = |sql: &str| {
        db.query_row(sql, params![id, node, intent, c.id], |_| Ok(()))
            .optional()
            .map(|v| v.is_some())
    };
    match kind {
        Record::BacklogItem => db
            .prepare(
                "SELECT 1 FROM backlog_items WHERE item_id = ?1 AND project_id =
             (SELECT project_id FROM conversations WHERE conversation_id = ?2)",
            )?
            .exists(params![id, c.conversation_id]),
        Record::DecisionLog => db
            .prepare(
                "SELECT 1 FROM decision_log WHERE entry_id = ?1 AND project_id =
                      (SELECT project_id FROM conversations WHERE conversation_id = ?2)",
            )?
            .exists(params![id, c.conversation_id]),
        Record::Node => Ok(node.is_some() && node == Some(id)),
        Record::Conversation => Ok(c.conversation_id.as_deref() == Some(id)),
        Record::Turn | Record::Summary => {
            let (table, key) = if kind == Record::Turn {
                ("turns", "turn_id")
            } else {
                ("summaries", "summary_id")
            };
            db.query_row(
                &format!("SELECT conversation_id FROM {table} WHERE {key} = ?1"),
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map(|v| v.as_deref() == c.conversation_id.as_deref())
        }
        Record::Intent => exists(
            "SELECT 1 FROM intents WHERE intent_id = ?1 AND node = ?2 AND (?3 IS NULL OR intent_id = ?3 OR created_by = ?4 OR conversation_id = (SELECT conversation_id FROM credentials WHERE id = ?4) OR lead_conversation_id = (SELECT conversation_id FROM credentials WHERE id = ?4))",
        ),
        Record::Job => exists(
            "SELECT 1 FROM jobs j JOIN nodes n ON n.id = j.node_id LEFT JOIN intents i ON i.intent_id = j.intent_id WHERE j.job_id = ?1 AND n.name = ?2 AND (?3 IS NULL OR j.intent_id = ?3 OR i.created_by = ?4 OR i.conversation_id = (SELECT conversation_id FROM credentials WHERE id = ?4) OR i.lead_conversation_id = (SELECT conversation_id FROM credentials WHERE id = ?4))",
        ),
        Record::Run => {
            let job: Option<String> = db.query_row("SELECT COALESCE((SELECT job_id FROM runs WHERE run_id = ?1), (SELECT job_id FROM jobs WHERE run_id = ?1), (SELECT job_id FROM live_runs WHERE run_id = ?1))",params![id], |r| r.get(0))?;
            if let Some(j) = job {
                return visible(db, c, Record::Job, &j);
            }
            if intent.is_some() {
                return Ok(false);
            }
            db.query_row("SELECT 1 FROM runs r JOIN nodes n ON n.id = r.node_id WHERE r.run_id = ?1 AND n.name = ?2",params![id,node], |_| Ok(())).optional().map(|v| v.is_some())
        }
        Record::Decision | Record::Land | Record::Cancel | Record::ActionApproval => {
            let (table, key, target, kind) = match kind {
                Record::Decision => ("decisions", "decision_id", "run_id", Record::Run),
                Record::Land => ("lands", "land_id", "run_id", Record::Run),
                Record::Cancel => ("cancels", "cancel_id", "job_id", Record::Job),
                Record::ActionApproval => ("action_approvals", "id", "job_id", Record::Job),
                _ => unreachable!(),
            };
            let target: Option<String> = db
                .query_row(
                    &format!("SELECT {target} FROM {table} WHERE {key} = ?1"),
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            target.map_or(Ok(false), |id| visible(db, c, kind, &id))
        }
        _ => Ok(false),
    }
}

fn may_control_intent(db: &Connection, c: &Client, intent_id: &str) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT 1 FROM intents i
         WHERE i.intent_id = ?1 AND i.node = ?2
           AND i.lead_conversation_id = ?3",
        params![intent_id, c.node, c.conversation_id],
        |_| Ok(()),
    )
    .optional()
    .map(|v| v.is_some())
}

/// Write authority is narrower than visibility: a node or attached intent is not a grant.
fn may_control(db: &Connection, c: &Client, kind: Record, id: &str) -> rusqlite::Result<bool> {
    let job = match kind {
        Record::Job => Some(id.to_string()),
        Record::Run => db.query_row(
            "SELECT COALESCE((SELECT job_id FROM runs WHERE run_id = ?1),
                             (SELECT job_id FROM jobs WHERE run_id = ?1),
                             (SELECT job_id FROM live_runs WHERE run_id = ?1))",
            params![id],
            |r| r.get::<_, Option<String>>(0),
        )?,
        _ => None,
    };
    let Some(job) = job else {
        return Ok(false);
    };
    db.query_row(
        "SELECT 1 FROM jobs j JOIN nodes n ON n.id = j.node_id
         JOIN intents i ON i.intent_id = j.intent_id
         WHERE j.job_id = ?1 AND n.name = ?2
           AND (i.created_by = ?3 OR i.conversation_id = ?4 OR i.lead_conversation_id = ?4)",
        params![job, c.node, c.id, c.conversation_id],
        |_| Ok(()),
    )
    .optional()
    .map(|v| v.is_some())
}

fn may_submit(db: &Connection, c: &Client, call: &Call) -> rusqlite::Result<bool> {
    let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
    if body["node"].as_str() != c.node.as_deref() {
        return Ok(false);
    }
    match &body["project_id"] {
        Value::Null => {}
        Value::String(project) if crate::decision_log::project_visible(db, c, project)? => {}
        _ => return Ok(false),
    }
    if let Some(job) = body["from_job"].as_str() {
        return visible(db, c, Record::Job, job);
    }
    if let Some(files) = body["files"].as_array() {
        for sha in files.iter().filter_map(|f| f["sha256"].as_str()) {
            let owned = db
                .prepare_cached(
                    "SELECT 1 FROM lead_objects WHERE credential_id = ?1 AND sha256 = ?2",
                )?
                .exists(params![c.id, sha])?;
            if !owned && !object_visible(db, c, sha)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Authorize before parsing a write or opening an evidence/event response.
pub fn guard(state: &State, call: &Call, c: &Client, parts: &[&str]) -> Option<Reply> {
    if c.kind != Kind::Lead {
        return None;
    }
    let check = || -> rusqlite::Result<bool> {
        let db = state.store.db();
        match (call.method.as_str(), parts) {
            (
                "GET",
                ["me" | "nodes" | "usage" | "agents" | "repos" | "action-approvals"]
                | ["nodes" | "agents" | "action-approvals", _],
            )
            | ("PUT", ["objects", _]) => Ok(true),
            ("GET" | "POST", ["projects", project, "decision_log" | "backlog", ..]) => {
                crate::decision_log::project_visible(&db, c, project)
            }
            ("GET", ["events"]) => Ok(call
                .query("conversation")
                .is_none_or(|id| c.conversation_id.as_deref() == Some(id))),
            ("GET", ["intents" | "jobs" | "runs"]) => {
                let node_ok = call
                    .query("node")
                    .is_none_or(|n| Some(n) == c.node.as_deref());
                let conversation_ok = call
                    .query("conversation")
                    .is_none_or(|id| c.conversation_id.as_deref() == Some(id));
                Ok(node_ok && conversation_ok)
            }
            ("GET", ["conversations", id] | ["conversations", id, "thread"])
            | ("PUT" | "DELETE", ["conversations", id, "attention"])
            | ("POST", ["conversations", id, "wake-turns"]) => {
                visible(&db, c, Record::Conversation, id)
            }
            ("GET", [kind, id, ..])
                if matches!(
                    *kind,
                    "intents" | "jobs" | "runs" | "decisions" | "cancels" | "turns" | "summaries"
                ) =>
            {
                let kind = match *kind {
                    "intents" => Record::Intent,
                    "jobs" => Record::Job,
                    "runs" => Record::Run,
                    "decisions" => Record::Decision,
                    "cancels" => Record::Cancel,
                    "turns" => Record::Turn,
                    _ => Record::Summary,
                };
                visible(&db, c, kind, id)
            }
            ("POST", ["jobs", id, "cancel"]) => may_control(&db, c, Record::Job, id),
            ("POST", ["runs", id, "decision"]) => may_control(&db, c, Record::Run, id),
            ("POST", ["intents", id, "jobs"]) | ("PUT", ["intents", id, "owner-messages", ..]) => {
                may_control_intent(&db, c, id)
            }
            ("GET", ["objects", sha]) => object_visible(&db, c, sha),
            ("POST", ["intents"]) => may_submit(&db, c, call),
            _ => Ok(false),
        }
    };
    match check() {
        Ok(true) => None,
        Ok(false)
            if matches!(
                (call.method.as_str(), parts),
                (
                    "POST",
                    ["jobs", _, "cancel"] | ["runs", _, "decision"] | ["intents", _, "jobs"]
                )
            ) =>
        {
            Some(refusal(
                403,
                "owner_only",
                "hosted leads may decide or cancel only their own intents or intents descended from their conversation turns",
                Value::Null,
            ))
        }
        Ok(false) => Some(denied()),
        Err(e) => Some(internal(&e)),
    }
}

// Common intent association check for work lists and usage.
fn push_association(
    c: &Client,
    intent_id: &str,
    intent: &str,
    sql: &mut String,
    bind: &mut Vec<rusqlite::types::Value>,
) {
    let _ = write!(
        sql,
        " AND (? IS NULL OR {intent_id} = ? OR {intent}.created_by = ?
               OR {intent}.conversation_id = (SELECT conversation_id FROM credentials WHERE id = ?)
               OR {intent}.lead_conversation_id = (SELECT conversation_id FROM credentials WHERE id = ?))"
    );
    bind.extend([
        c.intent_id.clone().into(),
        c.intent_id.clone().into(),
        c.id.clone().into(),
        c.id.clone().into(),
        c.id.clone().into(),
    ]);
}

pub fn push_intent_scope(c: &Client, sql: &mut String, bind: &mut Vec<rusqlite::types::Value>) {
    if c.kind == Kind::Lead {
        sql.push_str(" AND i.node = ?");
        bind.push(c.node.clone().into());
        push_association(c, "i.intent_id", "i", sql, bind);
    }
}

fn push_job_exists(
    c: &Client,
    job: &str,
    sql: &mut String,
    bind: &mut Vec<rusqlite::types::Value>,
) {
    let _ = write!(
        sql,
        "EXISTS (SELECT 1 FROM jobs jv
                 JOIN nodes nv ON nv.id = jv.node_id
                 LEFT JOIN intents iv ON iv.intent_id = jv.intent_id
                 WHERE jv.job_id = {job} AND nv.name = ?"
    );
    bind.push(c.node.clone().into());
    push_association(c, "jv.intent_id", "iv", sql, bind);
    sql.push(')');
}

pub fn push_job_scope(
    c: &Client,
    job: &str,
    sql: &mut String,
    bind: &mut Vec<rusqlite::types::Value>,
) {
    if c.kind == Kind::Lead {
        sql.push_str(" AND ");
        push_job_exists(c, job, sql, bind);
    }
}

/// Restrict run lists and usage roll-ups to runs a lead may see.
pub fn push_run_scope(c: &Client, sql: &mut String, bind: &mut Vec<rusqlite::types::Value>) {
    if c.kind != Kind::Lead {
        return;
    }
    let job = "COALESCE(r.job_id, (SELECT x.job_id FROM jobs x WHERE x.run_id = r.run_id LIMIT 1), (SELECT l.job_id FROM live_runs l WHERE l.run_id = r.run_id LIMIT 1))";
    sql.push_str(" AND (");
    push_job_exists(c, job, sql, bind);
    let _ = write!(sql, " OR (? IS NULL AND {job} IS NULL AND n.name = ?))");
    bind.extend([c.intent_id.clone().into(), c.node.clone().into()]);
}

/// Filter list records before returning them to a lead.
pub fn filter(
    db: &Connection,
    c: &Client,
    kind: Record,
    rows: Vec<Value>,
) -> Result<Vec<Value>, String> {
    let key = match kind {
        Record::Intent => "intent_id",
        Record::Job => "job_id",
        Record::Run => "run_id",
        _ => return Ok(rows),
    };
    rows.into_iter()
        .filter_map(
            |r| match visible(db, c, kind, r[key].as_str().unwrap_or_default()) {
                Ok(true) => Some(Ok(r)),
                Ok(false) => None,
                Err(e) => Some(Err(e.to_string())),
            },
        )
        .collect()
}

/// An object is readable only through a job or run in scope.
fn object_visible(db: &Connection, c: &Client, sha: &str) -> rusqlite::Result<bool> {
    let mut stmt = db.prepare("SELECT 'job',job_id FROM job_files WHERE sha256 = ?1 UNION ALL SELECT 'run',run_id FROM run_files WHERE sha256 = ?1")?;
    let rows = stmt
        .query_map(params![sha], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (kind, id) in rows {
        if visible(
            db,
            c,
            if kind == "job" {
                Record::Job
            } else {
                Record::Run
            },
            &id,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}
