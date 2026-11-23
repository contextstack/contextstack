//! Director-owned schedules for hosted lead turns. Fleet schedules run on nodes.

use crate::access::Client;
use crate::server::{Call, Reply, State, json};
use crate::store::{Store, write_tx};
use crate::work::{self, No};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const MESSAGE: &str = "Backlog heartbeat: use backlog to take the next unblocked item in project {project}.     Dispatch or advance it as appropriate and update the backlog with backlog_update.     If nothing is ready, report that and finish.";

fn schedule(db: &Connection, project: &str) -> rusqlite::Result<Value> {
    db.query_row(
        "SELECT conversation_id, interval_seconds FROM project_heartbeats WHERE project_id = ?1",
        [project],
        |r| {
            Ok(
                json!({"project_id": project, "conversation_id": r.get::<_, String>(0)?,
                                 "interval_seconds": r.get::<_, i64>(1)?}),
            )
        },
    )
    .optional()
    .map(|value| {
        value.unwrap_or_else(|| {
            json!({
                "project_id": project, "conversation_id": null, "interval_seconds": null
            })
        })
    })
}

fn configure(state: &State, call: &Call, client: &Client, project: &str) -> Result<Value, No> {
    let input = work::body(call)?;
    let mut db = state.store.db();
    let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
    crate::backlog::check_project(&tx, client, project)?;
    match input.get("interval_seconds") {
        Some(Value::Null) if input.len() == 1 => {
            tx.execute(
                "DELETE FROM project_heartbeats WHERE project_id = ?1",
                [project],
            )
            .map_err(|e| No::internal(&e))?;
        }
        Some(interval) if input.len() == 2 => {
            let seconds = interval
                .as_i64()
                .filter(|s| *s > 0 && *s <= i64::from(u32::MAX))
                .ok_or_else(|| {
                    No::invalid("interval_seconds must be a positive 32-bit integer or null")
                })?;
            let conversation = input
                .get("conversation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    No::invalid("conversation_id is required to enable the heartbeat")
                })?;
            let valid = tx
                .prepare(
                    "SELECT 1 FROM conversations WHERE conversation_id = ?1
                AND project_id = ?2 AND lead = 1 AND archived_at IS NULL",
                )
                .and_then(|mut stmt| stmt.exists(params![conversation, project]))
                .map_err(|e| No::internal(&e))?;
            if !valid {
                return Err(No::invalid(
                    "conversation_id must name an unarchived hosted lead in this project",
                ));
            }
            tx.execute("INSERT INTO project_heartbeats (project_id, conversation_id, interval_seconds, due_at)
                        VALUES (?1, ?2, ?3, ?4 + ?3 * 1000)
                        ON CONFLICT(project_id) DO UPDATE SET conversation_id = excluded.conversation_id,
                            interval_seconds = excluded.interval_seconds, due_at = excluded.due_at
                        WHERE conversation_id != excluded.conversation_id OR interval_seconds != excluded.interval_seconds",
                        params![project, conversation, seconds, Utc::now().timestamp_millis()])
                .map_err(|e| No::internal(&e))?;
        }
        _ => {
            return Err(No::invalid(
                "supply interval_seconds and conversation_id, or only interval_seconds: null",
            ));
        }
    }
    let answer = schedule(&tx, project).map_err(|e| No::internal(&e))?;
    tx.commit().map_err(|e| No::internal(&e))?;
    Ok(answer)
}

pub(crate) fn handle(state: &State, call: &Call, client: &Client, project: &str) -> Reply {
    let result = match call.method.as_str() {
        "GET" => {
            let db = state.store.db();
            crate::backlog::check_project(&db, client, project)
                .and_then(|()| schedule(&db, project).map_err(|e| No::internal(&e)))
        }
        "PUT" => configure(state, call, client, project),
        _ => Err(No(404, "not_found", "no such route".into(), Value::Null)),
    };
    result.map_or_else(No::reply, |answer| json(200, &answer))
}

pub(crate) fn tick(store: &Store) -> Result<(), String> {
    let mut db = store.db();
    let now = Utc::now().timestamp_millis();
    let projects = db
        .prepare("SELECT project_id FROM project_heartbeats WHERE due_at <= ?1")
        .and_then(|mut stmt| {
            stmt.query_map([now], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| e.to_string())?;
    for project in projects {
        if let Err(e) = fire(store, &mut db, &project, now) {
            crate::logf!("heartbeat for {project}: {e}");
        }
    }
    Ok(())
}

fn fire(store: &Store, db: &mut Connection, project: &str, now: i64) -> Result<(), String> {
    let tx = write_tx(db).map_err(|e| e.to_string())?;
    // Recheck every condition under the same write lock that records and advances the turn.
    let target = tx.query_row(&format!(
        "SELECT h.conversation_id FROM project_heartbeats h
         JOIN conversations c ON c.conversation_id = h.conversation_id
         WHERE h.project_id = ?1 AND h.due_at <= ?2 AND c.held = 0 AND c.archived_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM summaries s WHERE s.conversation_id = h.conversation_id
                 AND s.state = 'running')
           AND NOT EXISTS (SELECT 1 FROM turns t JOIN conversations tc ON tc.conversation_id = t.conversation_id
               WHERE t.state IN ('queued','summarising','running')
                 AND (t.conversation_id = h.conversation_id OR (t.origin = 'heartbeat' AND tc.project_id = ?1)))
           AND NOT EXISTS (SELECT 1 FROM backlog_items WHERE project_id = ?1
               AND {owner_blocked} AND json_extract(data, '$.stop') = 1)",
        owner_blocked = crate::backlog::OWNER_BLOCKED,
    ), params![project, now], |r| r.get::<_, String>(0)).optional().map_err(|e| e.to_string())?;
    let Some(conversation) = target else {
        return Ok(());
    };
    let message = MESSAGE.replace("{project}", project);
    crate::turns::enqueue(store, &tx, &conversation, &message, "director", "heartbeat")?;
    tx.execute(
        "UPDATE project_heartbeats SET due_at = ?2 + interval_seconds * 1000 WHERE project_id = ?1",
        params![project, now],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}
