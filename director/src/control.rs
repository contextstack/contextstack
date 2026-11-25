//! Control messages (NODE-PROTOCOL.md §9.1): what the Director wants a node to do, picked up by the
//! node. Each has a stable id and moves requested → received → applied or refused. A cancel and a schedule change
//! each own one; a change of state is a new revision of its owner.

use crate::store::{Record, touch};
use crate::util::{self, now_utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

/// The longest `detail` a node may report.
const MAX_DETAIL: usize = 4000;

/// A refused report: status, code, detail, extra fields.
pub struct Refused(pub u16, pub &'static str, pub String, pub Value);

fn invalid(detail: &str) -> Refused {
    Refused(422, "invalid_request", detail.into(), Value::Null)
}

fn db_err(e: &rusqlite::Error) -> Refused {
    crate::logf!("internal error: {e}");
    Refused(
        500,
        "internal",
        "the Director could not complete the request".into(),
        Value::Null,
    )
}

/// The outcomes a kind allows, for `applied` and for `refused`.
fn outcomes(kind: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match kind {
        "cancel" => (&["stopped", "already_finished"], &["refused"]),
        "schedule_change" => (
            &["applied"],
            &["revision_mismatch", "unknown_schedule", "refused"],
        ),
        _ => (&[], &["refused"]),
    }
}

/// Record a new control message for a node, inside the caller's transaction. Returns its id.
pub fn issue(tx: &Connection, node_id: i64, kind: &str, body: &Value) -> Result<String, String> {
    let id = format!("ctl_{}", util::ulid()?.to_lowercase());
    tx.execute(
        "INSERT INTO controls (control_id, node_id, kind, body, state, issued_at)
         VALUES (?1, ?2, ?3, ?4, 'requested', ?5)",
        params![id, node_id, kind, body.to_string(), now_utc()],
    )
    .map_err(|e| e.to_string())?;
    Ok(id)
}

const SELECT: &str = "SELECT control_id, kind, state, issued_at, body, outcome, detail, result, received_at, settled_at
                      FROM controls";

fn shape(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok());
    Ok(
        json!({"control_id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?,
              "state": r.get::<_, String>(2)?, "issued_at": r.get::<_, String>(3)?,
              "body": parse(r.get(4)?).unwrap_or_else(|| json!({})), "outcome": r.get::<_, Option<String>>(5)?,
              "detail": r.get::<_, Option<String>>(6)?, "result": parse(r.get(7)?),
              "received_at": r.get::<_, Option<String>>(8)?, "settled_at": r.get::<_, Option<String>>(9)?}),
    )
}

/// `GET control`: this node's messages that are requested or received, oldest first.
pub fn open_for(db: &Connection, node_id: i64) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = db.prepare_cached(&format!(
        "{SELECT} WHERE node_id = ?1 AND state IN ('requested', 'received') ORDER BY rowid"
    ))?;
    stmt.query_map(params![node_id], shape)?.collect()
}

fn one(db: &Connection, node_id: i64, id: &str) -> rusqlite::Result<Option<Value>> {
    db.query_row(
        &format!("{SELECT} WHERE control_id = ?1 AND node_id = ?2"),
        params![id, node_id],
        shape,
    )
    .optional()
}

/// Touch the record that owns a control message (and what it shows on), so clients see the new state.
fn touch_owner(tx: &Connection, control_id: &str) -> rusqlite::Result<()> {
    let cancel: Option<(String, String)> = tx
        .query_row(
            "SELECT cancel_id, job_id FROM cancels WHERE control_id = ?1",
            params![control_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((cancel_id, job_id)) = cancel {
        touch(tx, Record::Cancel, &cancel_id)?;
        touch(tx, Record::Job, &job_id)?;
    }
    let change: Option<(String, String)> = tx
        .query_row(
            "SELECT change_id, schedule_key FROM schedule_changes WHERE control_id = ?1",
            params![control_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((change_id, key)) = change {
        touch(tx, Record::ScheduleChange, &change_id)?;
        touch(tx, Record::Schedule, &key)?;
    }
    Ok(())
}

/// A report body, checked against the message's kind: `(state, outcome, detail, result)`.
type Report = (String, Option<String>, Option<String>, Option<Value>);

fn parse_report(kind: &str, body: &[u8]) -> Result<Report, Refused> {
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body) else {
        return Err(invalid("the body must be a JSON object"));
    };
    let state = m.get("state").and_then(Value::as_str).unwrap_or_default();
    let detail = match m.get("detail") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= MAX_DETAIL => Some(s.clone()),
        Some(_) => return Err(invalid("detail must be a string of at most 4000 bytes")),
    };
    let result = match m.get("result") {
        None | Some(Value::Null) => None,
        Some(Value::Object(o)) => Some(Value::Object(o.clone())),
        Some(_) => return Err(invalid("result must be an object")),
    };
    let outcome = m.get("outcome").and_then(Value::as_str).map(String::from);
    let (applied, refused) = outcomes(kind);
    match state {
        "received" => return Ok((state.into(), None, None, None)),
        "applied" | "refused" => {}
        _ => return Err(invalid("state must be received, applied or refused")),
    }
    let allowed = if state == "applied" { applied } else { refused };
    let Some(word) = outcome.filter(|o| allowed.contains(&o.as_str())) else {
        return Err(invalid(&format!(
            "a {kind} that is {state} has outcome {}",
            allowed.join(" | ")
        )));
    };
    if kind == "schedule_change"
        && state == "applied"
        && result
            .as_ref()
            .and_then(|r| r.get("revision"))
            .and_then(Value::as_i64)
            .is_none_or(|n| n < 1)
    {
        return Err(invalid(
            "an applied schedule_change reports result.revision, the schedule's new revision",
        ));
    }
    Ok((state.into(), Some(word), detail, result))
}

/// `POST control/{id}`: the node reports a message received, applied or refused. A repeat of the recorded
/// outcome answers the same record; a different outcome on a settled message is a conflict.
pub fn report(
    store: &crate::store::Store,
    db: &mut Connection,
    node_id: i64,
    control_id: &str,
    body: &[u8],
) -> Result<Value, Refused> {
    let Some(now) = one(db, node_id, control_id).map_err(|e| db_err(&e))? else {
        return Err(Refused(
            404,
            "not_found",
            "no such control message for this node".into(),
            Value::Null,
        ));
    };
    let kind = now["kind"].as_str().unwrap_or_default().to_string();
    let (state, outcome, detail, result) = parse_report(&kind, body)?;
    let was = now["state"].as_str().unwrap_or_default();
    let settled = matches!(was, "applied" | "refused");
    if state == "received" && was != "requested" {
        return Ok(json!({"control": now}));
    }
    if settled {
        if now["state"] == state.as_str() && now["outcome"].as_str() == outcome.as_deref() {
            return Ok(json!({"control": now}));
        }
        return Err(Refused(
            409,
            "conflict",
            "this control message is already settled with another outcome".into(),
            json!({"control": now}),
        ));
    }
    let tx = crate::store::write_tx(db).map_err(|e| db_err(&e))?;
    let stamp = now_utc();
    let changed = if state == "received" {
        tx.execute(
            "UPDATE controls SET state = 'received', received_at = ?1 WHERE control_id = ?2",
            params![stamp, control_id],
        )
    } else {
        tx.execute(
            "UPDATE controls SET state = ?1, outcome = ?2, detail = ?3, result = ?4, settled_at = ?5,
                                 received_at = COALESCE(received_at, ?5)
             WHERE control_id = ?6",
            params![
                state,
                outcome,
                detail,
                result.map(|r| r.to_string()),
                stamp,
                control_id
            ],
        )
    };
    changed
        .and_then(|_| touch_owner(&tx, control_id))
        .map_err(|e| db_err(&e))?;
    // An applied cancel closes the job's reservation (CLIENT-API.md §12.3). A job with no live run that the
    // node stops (offered, or accepted with nothing to stop) ends here as cancelled; a running job waits for
    // job_terminal. Log line: `control {id} (cancel): applied stopped`.
    if kind == "cancel"
        && state == "applied"
        && let Some(job) = tx
            .query_row(
                "SELECT job_id FROM cancels WHERE control_id = ?1",
                params![control_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| db_err(&e))?
    {
        let outcome = outcome.as_deref().unwrap_or_default();
        crate::reservations::on_cancel_applied(store, &tx, &job, outcome);
    }
    tx.commit().map_err(|e| db_err(&e))?;
    crate::logf!(
        "control {control_id} ({kind}): {state} {}",
        outcome.unwrap_or_default()
    );
    let after = one(db, node_id, control_id).map_err(|e| db_err(&e))?;
    Ok(json!({"control": after}))
}

/// A job's `job_terminal` was acked: a cancel the node never picked up is settled here, as already finished.
pub fn settle_on_terminal(tx: &Connection, job_id: &str) -> rusqlite::Result<()> {
    let open: Option<String> = tx
        .query_row(
            "SELECT k.control_id FROM cancels c JOIN controls k ON k.control_id = c.control_id
             WHERE c.job_id = ?1 AND k.state = 'requested'",
            params![job_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(control_id) = open else {
        return Ok(());
    };
    tx.execute(
        "UPDATE controls SET state = 'applied', outcome = 'already_finished', settled_at = ?1,
                             detail = 'the node reported the job over before it picked the cancel up'
         WHERE control_id = ?2",
        params![now_utc(), control_id],
    )?;
    tx.execute(
        "UPDATE cancels SET state = 'applied' WHERE control_id = ?1",
        params![control_id],
    )?;
    touch_owner(tx, &control_id)
}

/// A control message's state as its owner record shows it: `{state, outcome, pending, delivered, detail, …}`.
pub fn view(
    db: &Connection,
    control_id: Option<&str>,
    node_protocol: Option<i64>,
) -> rusqlite::Result<Map<String, Value>> {
    let found = match control_id {
        Some(id) => db
            .query_row(
                &format!("{SELECT} WHERE control_id = ?1"),
                params![id],
                shape,
            )
            .optional()?,
        None => None,
    };
    let mut out = Map::new();
    let Some(c) = found else {
        // A cancel recorded before control messages existed: it can never be delivered.
        out.extend([
            ("control_id".into(), Value::Null),
            ("state".into(), json!("requested")),
            ("outcome".into(), Value::Null),
            ("pending".into(), json!(true)),
            ("delivered".into(), json!(false)),
            ("received_at".into(), Value::Null),
            ("settled_at".into(), Value::Null),
            (
                "detail".into(),
                json!("recorded before control messages existed; it cannot be delivered"),
            ),
        ]);
        return Ok(out);
    };
    let state = c["state"].as_str().unwrap_or_default();
    let detail = match (state, c["detail"].as_str()) {
        ("applied" | "refused", Some(d)) => d.to_string(),
        ("applied" | "refused", None) => format!("the node reported {state}"),
        ("received", _) => "the node has it and has not reported an outcome yet".into(),
        _ if node_protocol.is_some_and(|p| p < 2) => {
            "the node speaks protocol 1, which cannot receive control messages; this stays requested until it \
             speaks protocol 2"
                .into()
        }
        _ => "waiting for the node to pick it up".into(),
    };
    out.extend([
        ("control_id".into(), c["control_id"].clone()),
        ("state".into(), c["state"].clone()),
        ("outcome".into(), c["outcome"].clone()),
        (
            "pending".into(),
            json!(matches!(state, "requested" | "received")),
        ),
        ("delivered".into(), json!(!c["received_at"].is_null())),
        ("received_at".into(), c["received_at"].clone()),
        ("settled_at".into(), c["settled_at"].clone()),
        ("result".into(), c["result"].clone()),
        ("detail".into(), json!(detail)),
    ]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_checked_against_the_kind() {
        let ok = |kind: &str, body: &str| parse_report(kind, body.as_bytes()).is_ok();
        assert!(ok("cancel", r#"{"state":"received"}"#));
        assert!(ok("cancel", r#"{"state":"applied","outcome":"stopped"}"#));
        assert!(ok(
            "cancel",
            r#"{"state":"applied","outcome":"already_finished"}"#
        ));
        assert!(ok(
            "cancel",
            r#"{"state":"refused","outcome":"refused","detail":"no"}"#
        ));
        assert!(!ok("cancel", r#"{"state":"applied","outcome":"refused"}"#));
        assert!(!ok("cancel", r#"{"state":"applied"}"#));
        assert!(!ok("cancel", r#"{"state":"done","outcome":"stopped"}"#));
        assert!(!ok(
            "cancel",
            r#"{"state":"refused","outcome":"refused","detail":7}"#
        ));
        assert!(!ok(
            "schedule_change",
            r#"{"state":"applied","outcome":"applied"}"#
        ));
        assert!(ok(
            "schedule_change",
            r#"{"state":"applied","outcome":"applied","result":{"revision":5}}"#
        ));
        assert!(ok(
            "schedule_change",
            r#"{"state":"refused","outcome":"revision_mismatch"}"#
        ));
        assert!(ok("mystery", r#"{"state":"refused","outcome":"refused"}"#));
        assert!(!ok("mystery", r#"{"state":"applied","outcome":"applied"}"#));
    }
}
