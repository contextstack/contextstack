//! Work trace for a task (intent): origin, job reasons, decisions, needs-you and owner-message marks.
//! Shape matches tracing/CONTRACT.md rule 5w; `GET intents/{id}/work-trace` serves it.

#![allow(
    clippy::too_many_lines,
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::unnecessary_wraps,
    clippy::type_complexity,
    clippy::implicit_clone
)]

use crate::access::{Client, Kind};
use crate::jobs::Plan;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::{Record, touch};
use crate::util::{self, now_utc};
use crate::work::{No, body};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

const MARKS: [&str; 3] = ["instruction", "answer", "intervention"];

/// Parsed task origin for storage on `intents`.
#[derive(Debug, Clone)]
pub struct Origin {
    pub kind: String,
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
}

/// Validate an optional `origin` object; default `{kind: api}`.
pub fn parse_origin(m: &Map<String, Value>) -> Result<Origin, No> {
    let Some(raw) = m.get("origin") else {
        return Ok(Origin {
            kind: "api".into(),
            conversation_id: None,
            turn_id: None,
        });
    };
    let obj = raw
        .as_object()
        .ok_or_else(|| No::invalid("origin must be an object"))?;
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| No::invalid("origin.kind is required"))?;
    match kind {
        "conversation_turn" => {
            let conversation_id = obj
                .get("conversation_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    No::invalid("origin.conversation_id is required for conversation_turn")
                })?;
            let turn_id = obj
                .get("turn_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| No::invalid("origin.turn_id is required for conversation_turn"))?;
            Ok(Origin {
                kind: kind.to_string(),
                conversation_id: Some(conversation_id),
                turn_id: Some(turn_id),
            })
        }
        "schedule" | "cs_cli" | "api" => Ok(Origin {
            kind: kind.to_string(),
            conversation_id: None,
            turn_id: None,
        }),
        _ => Err(No::invalid(
            "origin.kind must be conversation_turn, schedule, cs_cli or api",
        )),
    }
}

/// Infer conversation-turn origin from a source job or an active lead turn when still `api`.
pub fn resolve_origin(
    db: &Connection,
    mut origin: Origin,
    client: &Client,
    from_job: Option<&str>,
) -> Result<Origin, No> {
    if origin.kind == "api" {
        if let Some(job) = from_job {
            if let Ok(Some((conversation_id, turn_id))) = turn_for_job(db, job) {
                origin = Origin {
                    kind: "conversation_turn".into(),
                    conversation_id: Some(conversation_id),
                    turn_id: Some(turn_id),
                };
            }
        } else if client.kind == Kind::Lead
            && let Some(conv) = &client.conversation_id
            && let Ok(Some(turn_id)) = active_turn(db, conv)
        {
            origin = Origin {
                kind: "conversation_turn".into(),
                conversation_id: Some(conv.clone()),
                turn_id: Some(turn_id),
            };
        }
    }
    Ok(origin)
}

fn turn_for_job(db: &Connection, job_id: &str) -> rusqlite::Result<Option<(String, String)>> {
    db.query_row(
        "SELECT conversation_id, turn_id FROM turns WHERE job_id = ?1",
        params![job_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

fn active_turn(db: &Connection, conversation_id: &str) -> rusqlite::Result<Option<String>> {
    db.query_row(
        "SELECT turn_id FROM turns WHERE conversation_id = ?1
         AND state IN ('queued', 'summarising', 'running') ORDER BY seq DESC LIMIT 1",
        params![conversation_id],
        |r| r.get(0),
    )
    .optional()
}

/// Reason JSON for a job after the task's first, stored on `jobs.work_reason`.
pub fn reason_for_job(
    db: &Connection,
    intent_id: &str,
    meta: &Map<String, Value>,
    client: &Client,
) -> Result<Option<Value>, String> {
    let prior: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM jobs WHERE intent_id = ?1",
            params![intent_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if prior == 0 {
        return Ok(None);
    }
    if let Some(retry_of) = meta
        .get("retry_of")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let new_model = meta
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| meta.get("agent").and_then(Value::as_str));
        if let Some(prior_model) = job_model(db, retry_of)? {
            if new_model.is_some_and(|m| m != prior_model) {
                return Ok(Some(json!({"kind": "fallback", "template": prior_model})));
            }
        }
        if let Some(template) = meta
            .get("fallback_from")
            .or(meta.get("fallback_template"))
            .and_then(Value::as_str)
        {
            return Ok(Some(json!({"kind": "fallback", "template": template})));
        }
        return Ok(Some(json!({"kind": "retry"})));
    }
    if let Some(template) = meta
        .get("fallback_from")
        .or(meta.get("fallback_template"))
        .and_then(Value::as_str)
    {
        return Ok(Some(json!({"kind": "fallback", "template": template})));
    }
    if client.kind == Kind::Lead {
        Ok(Some(json!({"kind": "retry"})))
    } else {
        Ok(Some(json!({"kind": "owner_request"})))
    }
}

fn job_model(db: &Connection, job_id: &str) -> Result<Option<String>, String> {
    let meta: String = db
        .query_row(
            "SELECT meta FROM jobs WHERE job_id = ?1",
            params![job_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&meta).unwrap_or(Value::Null);
    Ok(v.get("model")
        .or(v.get("agent"))
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// Record needs-you when attention is raised on a conversation for one of its jobs.
pub fn on_attention_set(
    tx: &Connection,
    conversation_id: &str,
    job_id: Option<&str>,
    raised_at: &str,
    raised_by: &str,
) -> Result<(), String> {
    let Some(job_id) = job_id else {
        return Ok(());
    };
    let intent: Option<String> = tx
        .query_row(
            "SELECT intent_id FROM jobs WHERE job_id = ?1",
            params![job_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .flatten();
    let Some(intent_id) = intent else {
        return Ok(());
    };
    tx.execute(
        "UPDATE task_needs_you SET status = 'cleared', cleared_at = ?2, cleared_by = ?3
         WHERE intent_id = ?1 AND status = 'open'",
        params![intent_id, raised_at, raised_by],
    )
    .map_err(|e| e.to_string())?;
    let id = util::ulid().map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO task_needs_you (needs_you_id, intent_id, conversation_id, job_id,
                                     raised_at, raised_by, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open')",
        params![id, intent_id, conversation_id, job_id, raised_at, raised_by],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Clear open needs-you rows for intents tied to this conversation's jobs.
pub fn on_attention_clear(
    tx: &Connection,
    conversation_id: &str,
    cleared_by: &str,
) -> Result<(), String> {
    let now = now_utc();
    tx.execute(
        "UPDATE task_needs_you SET status = 'cleared', cleared_at = ?2, cleared_by = ?3
         WHERE conversation_id = ?1 AND status = 'open'",
        params![conversation_id, now, cleared_by],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// `GET intents/{id}/work-trace`.
pub fn get(state: &State, intent_id: &str) -> Reply {
    match assemble(&state.store.db(), intent_id) {
        Ok(Some(trace)) => json(200, &json!({"work_trace": trace})),
        Ok(None) => refusal(404, "not_found", "no such intent", Value::Null),
        Err(e) => internal(&e),
    }
}

/// `PUT intents/{id}/owner-messages/{message_id}/mark` with `{mark}`.
pub fn set_mark(
    state: &State,
    call: &Call,
    client: &Client,
    intent_id: &str,
    message_id: &str,
) -> Reply {
    let m = match body(call) {
        Ok(m) => m,
        Err(r) => return r.reply(),
    };
    let Some(mark) = m.get("mark").and_then(Value::as_str) else {
        return refusal(422, "invalid_request", "mark is required", Value::Null);
    };
    if !MARKS.contains(&mark) {
        return refusal(
            422,
            "invalid_request",
            "mark must be instruction, answer or intervention",
            Value::Null,
        );
    }
    let mut db = state.store.db();
    let exists = db
        .query_row(
            "SELECT 1 FROM intents WHERE intent_id = ?1",
            params![intent_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| e.to_string());
    match exists {
        Ok(None) => return refusal(404, "not_found", "no such intent", Value::Null),
        Err(e) => return internal(&e),
        Ok(Some(())) => {}
    }
    if !owner_message_on_task(&db, intent_id, message_id).unwrap_or(false) {
        return refusal(
            404,
            "not_found",
            "no such owner message on this task",
            Value::Null,
        );
    }
    let now = now_utc();
    let done = (|| -> Result<(), String> {
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        let owner = client.is_owner();
        let existing: Option<String> = tx
            .query_row(
                "SELECT actor FROM owner_message_marks WHERE message_id = ?1 AND intent_id = ?2",
                params![message_id, intent_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(actor) = existing {
            let prior_owner: bool = tx
                .query_row(
                    "SELECT kind = 'local' FROM credentials WHERE id = ?1",
                    params![actor],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if prior_owner && !owner {
                return Err("owner_correction".into());
            }
        }
        tx.execute(
            "INSERT INTO owner_message_marks (message_id, intent_id, mark, actor, marked_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(message_id) DO UPDATE SET mark = excluded.mark, actor = excluded.actor,
                                                  marked_at = excluded.marked_at
             WHERE intent_id = excluded.intent_id",
            params![message_id, intent_id, mark, client.id, now],
        )
        .map_err(|e| e.to_string())?;
        touch(&tx, Record::Intent, intent_id).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    })();
    match done {
        Ok(()) => match assemble(&db, intent_id) {
            Ok(Some(trace)) => json(200, &json!({"work_trace": trace})),
            Ok(None) => refusal(404, "not_found", "no such intent", Value::Null),
            Err(e) => internal(&e),
        },
        Err(e) if e == "owner_correction" => refusal(
            403,
            "owner_only",
            "an owner mark overrides a prior mark",
            Value::Null,
        ),
        Err(e) => internal(&e),
    }
}

fn owner_message_on_task(
    db: &Connection,
    intent_id: &str,
    message_id: &str,
) -> rusqlite::Result<bool> {
    let conversations = task_conversation_ids(db, intent_id)?;
    if conversations.is_empty() {
        return Ok(false);
    }
    let placeholders = conversations
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT 1 FROM turns t JOIN credentials c ON c.id = t.posted_by
         WHERE t.turn_id = ?1 AND t.conversation_id IN ({placeholders})
           AND c.kind IN ('local', 'device')"
    );
    let mut sql_params: Vec<&dyn rusqlite::ToSql> = vec![&message_id];
    for c in &conversations {
        sql_params.push(c);
    }
    db.query_row(&sql, sql_params.as_slice(), |_| Ok(()))
        .optional()
        .map(|v| v.is_some())
}

fn task_conversation_ids(db: &Connection, intent_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut ids = Vec::new();
    if let Some(c) = db
        .query_row(
            "SELECT origin_conversation_id FROM intents WHERE intent_id = ?1",
            params![intent_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
    {
        ids.push(c);
    }
    if let Some(c) = db
        .query_row(
            "SELECT lead_conversation_id FROM intents WHERE intent_id = ?1",
            params![intent_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
    {
        if !ids.contains(&c) {
            ids.push(c);
        }
    }
    let mut stmt =
        db.prepare_cached("SELECT conversation_id FROM conversations WHERE intent_id = ?1")?;
    for row in stmt.query_map(params![intent_id], |r| r.get::<_, String>(0))? {
        let c = row?;
        if !ids.contains(&c) {
            ids.push(c);
        }
    }
    Ok(ids)
}

pub fn assemble(db: &Connection, intent_id: &str) -> Result<Option<Value>, String> {
    let row: Option<(String, Option<String>, String, Option<String>, Option<String>)> = db
        .query_row(
            "SELECT intent_id, title, COALESCE(origin_kind, 'api'), origin_conversation_id, origin_turn_id
             FROM intents WHERE intent_id = ?1",
            params![intent_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((task_id, title, kind, conversation_id, turn_id)) = row else {
        return Ok(None);
    };
    let origin = match kind.as_str() {
        "conversation_turn" => json!({
            "kind": kind,
            "conversation_id": conversation_id,
            "turn_id": turn_id,
        }),
        _ => json!({"kind": kind}),
    };
    let mut jobs = Vec::new();
    let mut stmt = db
        .prepare_cached(
            "SELECT job_id, state, finished_at, run_id, work_reason FROM jobs
             WHERE intent_id = ?1 ORDER BY rowid",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![intent_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let job_count = rows.len();
    for (index, (job_id, state, _finished_at, terminal_run, work_reason)) in rows.iter().enumerate()
    {
        let finished = matches!(state.as_str(), "done" | "error");
        let run_ids = runs_for_job(db, job_id)?;
        let mut job = json!({
            "job_id": job_id,
            "finished": finished,
            "run_ids": run_ids,
        });
        if let Some(raw) = work_reason {
            if let Ok(reason) = serde_json::from_str::<Value>(raw) {
                job["reason"] = reason;
            }
        }
        let superseded = index + 1 < job_count;
        let (decisions, none_yet) =
            job_decisions(db, job_id, terminal_run.as_deref(), finished, superseded);
        if let Some(status) = none_yet {
            job["decision_status"] = json!(status);
        }
        if !decisions.is_empty() {
            job["decisions"] = json!(decisions);
        }
        jobs.push(job);
    }
    let mut needs = needs_you_for(db, intent_id)?;
    needs.extend(crate::action_approvals::needs_you(db, intent_id).map_err(|e| e.to_string())?);
    needs.sort_by(|a, b| a["raised_at"].as_str().cmp(&b["raised_at"].as_str()));
    let raised_needs_you_ids: Vec<_> = needs.iter().map(|n| n["id"].clone()).collect();
    let (owner_message_ids, owner_messages) = owner_messages_for(db, intent_id)?;
    Ok(Some(json!({
        "task_id": task_id,
        "title": title.unwrap_or_default(),
        "origin": origin,
        "jobs": jobs,
        "raised_needs_you_ids": raised_needs_you_ids,
        "needs_you": needs,
        "owner_message_ids": owner_message_ids,
        "owner_messages": owner_messages,
    })))
}

fn runs_for_job(db: &Connection, job_id: &str) -> Result<Vec<String>, String> {
    let mut runs = Vec::new();
    if let Ok(term) = db.query_row(
        "SELECT run_id FROM jobs WHERE job_id = ?1",
        params![job_id],
        |r| r.get::<_, Option<String>>(0),
    ) {
        if let Some(r) = term {
            runs.push(r);
        }
    }
    let mut stmt = db
        .prepare_cached("SELECT run_id FROM runs WHERE job_id = ?1 ORDER BY rowid")
        .map_err(|e| e.to_string())?;
    for row in stmt
        .query_map(params![job_id], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
    {
        let r = row.map_err(|e| e.to_string())?;
        if !runs.contains(&r) {
            runs.push(r);
        }
    }
    Ok(runs)
}

fn job_decisions(
    db: &Connection,
    job_id: &str,
    terminal_run: Option<&str>,
    finished: bool,
    superseded: bool,
) -> (Vec<Value>, Option<&'static str>) {
    let mut decisions = Vec::new();
    if let Some(run_id) = terminal_run {
        if let Ok(Some((decision, actor, at))) = db
            .query_row(
                "SELECT d.decision, d.decided_by, d.decided_at FROM decisions d
                 WHERE d.run_id = ?1",
                params![run_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
        {
            decisions.push(json!({"kind": decision, "actor": actor, "at": at}));
        }
        if let Ok(Some((landed_by, landed_at))) = db
            .query_row(
                "SELECT landed_by, landed_at FROM lands WHERE run_id = ?1 AND current = 1 AND state = 'landed'",
                params![run_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
        {
            decisions.push(json!({"kind": "land", "actor": landed_by, "at": landed_at}));
        }
    }
    if let Ok(Some((requested_by, requested_at))) = db
        .query_row(
            "SELECT requested_by, requested_at FROM cancels WHERE job_id = ?1",
            params![job_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
    {
        decisions.push(json!({"kind": "cancel", "actor": requested_by, "at": requested_at}));
    }
    // Superseded finished jobs may never get a decision; the current job must decide or read incomplete.
    let none_yet = if finished && decisions.is_empty() && superseded {
        Some("none_yet")
    } else {
        None
    };
    (decisions, none_yet)
}

fn needs_you_for(db: &Connection, intent_id: &str) -> Result<Vec<Value>, String> {
    let mut stmt = db
        .prepare_cached(
            "SELECT needs_you_id, raised_at, status, cleared_at, cleared_by FROM task_needs_you
             WHERE intent_id = ?1 ORDER BY raised_at",
        )
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in stmt
        .query_map(params![intent_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?
    {
        let (id, raised_at, status, cleared_at, cleared_by) = row.map_err(|e| e.to_string())?;
        let mut item = json!({"id": id, "raised_at": raised_at, "status": status});
        if status == "cleared" {
            item["cleared_at"] = json!(cleared_at);
            item["cleared_by"] = json!(cleared_by);
        }
        out.push(item);
    }
    Ok(out)
}

fn owner_messages_for(
    db: &Connection,
    intent_id: &str,
) -> Result<(Vec<String>, Vec<Value>), String> {
    let conversations = task_conversation_ids(db, intent_id).map_err(|e| e.to_string())?;
    if conversations.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let placeholders = conversations
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT t.turn_id, m.mark, m.actor, m.marked_at
         FROM turns t
         JOIN credentials c ON c.id = t.posted_by
         LEFT JOIN owner_message_marks m ON m.message_id = t.turn_id AND m.intent_id = ?1
         WHERE t.conversation_id IN ({placeholders}) AND c.kind IN ('local', 'device')
         ORDER BY t.posted_at"
    );
    let mut sql_args: Vec<String> = vec![intent_id.to_string()];
    sql_args.extend(conversations);
    let mut stmt = db.prepare(&sql).map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    let mut marks = Vec::new();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(sql_args.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (turn_id, mark, actor, marked_at) = row.map_err(|e| e.to_string())?;
        ids.push(turn_id.clone());
        if let (Some(mark), Some(actor), Some(at)) = (mark, actor, marked_at) {
            marks.push(json!({
                "message_id": turn_id,
                "actor": actor,
                "at": at,
                "mark": mark,
            }));
        }
    }
    Ok((ids, marks))
}

/// Persist `work_reason` when a follow-on job is planned.
pub fn stamp_job_reason(
    plan: &mut Plan,
    db: &Connection,
    intent_id: &str,
    client: &Client,
) -> Result<(), String> {
    if let Some(reason) = reason_for_job(db, intent_id, plan.meta(), client)? {
        plan.set_work_reason(Some(reason.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{Client, Kind};
    use crate::store::Store;

    #[test]
    fn origin_defaults_to_api() -> Result<(), String> {
        let origin = parse_origin(&Map::new()).map_err(|e| e.2)?;
        assert_eq!(origin.kind, "api");
        Ok(())
    }

    #[test]
    fn conversation_origin_requires_ids() {
        let mut m = Map::new();
        m.insert("origin".into(), json!({"kind": "conversation_turn"}));
        assert!(parse_origin(&m).is_err());
    }

    #[test]
    fn retry_reason_from_meta() -> Result<(), String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        seed_task(&db)?;
        let client = Client {
            id: "owner".into(),
            kind: Kind::Local,
            conversation_id: None,
            intent_id: None,
            node: Some("n".into()),
        };
        let mut meta = Map::new();
        meta.insert("retry_of".into(), "job-1".into());
        let reason = reason_for_job(&db, "task-1", &meta, &client)?;
        assert_eq!(reason, Some(json!({"kind": "retry"})));
        Ok(())
    }

    fn seed_task(db: &Connection) -> Result<(), String> {
        db.execute_batch(
            "INSERT INTO nodes (id, name, token_hash, created_at) VALUES (1, 'n', 'h', 't');
             INSERT INTO intents (intent_id, node, spec, task, trials, meta, bundle_sha256, created_by, created_at,
                                  origin_kind)
             VALUES ('task-1', 'n', 's', 'tasks/t', 1, '{}', 'b', 'owner', 't', 'api');
             INSERT INTO jobs (job_id, node_id, spec, task, trials, bundle_sha256, meta, state, created_at, intent_id)
             VALUES ('job-1', 1, 's', 'tasks/t', 1, 'b', '{}', 'done', 't', 'task-1');",
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}
