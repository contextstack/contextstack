//! Live work (NODE-PROTOCOL.md §9.2, §9.3): the held-open request for work, and a run's numbered live events,
//! recorded without gaps and passed to clients on the event stream.

use crate::control;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::ScopedConnection;
use crate::store::{Record, touch};
use crate::util::{self, now_utc, sha256_hex};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The longest a request for work is held open: part of the contract (§9.2).
pub const HOLD: Duration = Duration::from_secs(30);
/// Events per call, and the largest payload, serialised.
const MAX_EVENTS: usize = 500;
const MAX_PAYLOAD: usize = 64 * 1024;
/// Events per client read.
const PAGE: i64 = 1000;

/// Nodes holding a request for work open right now, by node row id.
static LISTENING: Mutex<Option<HashMap<i64, usize>>> = Mutex::new(None);

fn listening_change(node_id: i64, up: bool) {
    let mut held = LISTENING.lock().unwrap_or_else(PoisonError::into_inner);
    let map = held.get_or_insert_with(HashMap::new);
    let n = map.entry(node_id).or_insert(0);
    *n = if up { *n + 1 } else { n.saturating_sub(1) };
    if *n == 0 {
        map.remove(&node_id);
    }
}

/// Whether a node holds a request for work open.
pub fn listening(node_id: i64) -> bool {
    LISTENING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|m| m.contains_key(&node_id))
}

/// Counts a held request while it lives, including when the node drops it.
struct Held(i64);

impl Drop for Held {
    fn drop(&mut self) {
        listening_change(self.0, false);
    }
}

/// What `GET work` answers now, and its cursor.
fn snapshot(db: &ScopedConnection, node_id: i64) -> rusqlite::Result<(Value, Value, String)> {
    let jobs = crate::jobs::for_node(db, node_id)?;
    let control = control::open_for(db, node_id)?;
    let mut stmt = db.prepare_cached(
        "SELECT job_id, revision FROM jobs WHERE +org_id = :org_id AND node_id = ?1 AND state IN ('offered', 'accepted') ORDER BY rowid",
    )?;
    let mut seen = String::new();
    for row in stmt.query_map(params![node_id], |r| {
        Ok(format!(
            "j:{}:{}\n",
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?
        ))
    })? {
        seen.push_str(&row?);
    }
    for c in &control {
        let _ = writeln!(seen, "c:{}:{}", c["control_id"], c["state"]);
    }
    let cursor = sha256_hex(seen.as_bytes())[..32].to_string();
    Ok((Value::Array(jobs), Value::Array(control), cursor))
}

fn work_answer(jobs: &Value, control: &Value, cursor: &str, changed: bool) -> Reply {
    json(
        200,
        &json!({"jobs": jobs, "control": control, "cursor": cursor, "changed": changed}),
    )
}

/// `GET work?after=&wait=`: answer at once when the node's jobs or control messages differ from `after`;
/// otherwise when they come to differ, or after the hold (at most 30 s) with `changed: false`.
pub async fn work(state: &Arc<State>, call: &Call, node_id: i64) -> Reply {
    let wait = match call.query("wait") {
        None => HOLD,
        Some(w) => match w.parse::<u64>() {
            Ok(s) => Duration::from_secs(s).min(HOLD),
            Err(_) => {
                return refusal(
                    422,
                    "invalid_request",
                    "wait is a whole number of seconds from 0 to 30",
                    Value::Null,
                );
            }
        },
    };
    let after = call.query("after").map(String::from);
    let deadline = tokio::time::Instant::now() + wait;
    listening_change(node_id, true);
    let _held = Held(node_id);
    loop {
        let notified = state.hub.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let now = snapshot(&state.store.db(), node_id);
        let (jobs, control, cursor) = match now {
            Ok(s) => s,
            Err(e) => return internal(&e),
        };
        if after.as_deref() != Some(cursor.as_str()) {
            return work_answer(&jobs, &control, &cursor, true);
        }
        tokio::select! {
            () = notified => {}
            () = tokio::time::sleep_until(deadline) => return work_answer(&jobs, &control, &cursor, false),
            () = state.hub.stop.cancelled() => return work_answer(&jobs, &control, &cursor, false),
        }
    }
}

/// One event as sent, checked.
struct Event {
    seq: i64,
    kind: String,
    partial: bool,
    payload: Value,
    at: Option<String>,
}

fn kind_ok(k: &str) -> bool {
    let mut c = k.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && k.len() <= 64
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || matches!(x, '_' | '.'))
}

fn parse_events(body: &[u8]) -> Result<(String, Vec<Event>), String> {
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body) else {
        return Err("the body must be a JSON object".into());
    };
    let job = m
        .get("job_id")
        .and_then(Value::as_str)
        .filter(|j| !j.is_empty())
        .ok_or("job_id names the run's job")?;
    let list = m
        .get("events")
        .and_then(Value::as_array)
        .ok_or("events is a list")?;
    if list.len() > MAX_EVENTS {
        return Err(format!("at most {MAX_EVENTS} events per call"));
    }
    let mut out: Vec<Event> = Vec::with_capacity(list.len());
    for (i, e) in list.iter().enumerate() {
        let bad = |what: &str| format!("events[{i}]: {what}");
        let seq = e
            .get("seq")
            .and_then(Value::as_i64)
            .filter(|s| *s >= 1)
            .ok_or_else(|| bad("seq is an integer >= 1"))?;
        if out.last().is_some_and(|p| p.seq >= seq) {
            return Err(bad("events are sorted by seq, each once"));
        }
        let kind = e
            .get("kind")
            .and_then(Value::as_str)
            .filter(|k| kind_ok(k))
            .ok_or_else(|| bad("kind matches [a-z][a-z0-9_.]{0,63}"))?;
        let partial = e
            .get("partial")
            .and_then(Value::as_bool)
            .ok_or_else(|| bad("partial is a boolean"))?;
        let payload = e
            .get("payload")
            .filter(|p| p.is_object() && p.to_string().len() <= MAX_PAYLOAD)
            .ok_or_else(|| bad("payload is an object of at most 64 KiB"))?;
        let at = match e.get("at") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.len() <= 40 => Some(s.clone()),
            Some(_) => return Err(bad("at is a timestamp string")),
        };
        out.push(Event {
            seq,
            kind: kind.into(),
            partial,
            payload: payload.clone(),
            at,
        });
    }
    Ok((job.to_string(), out))
}

/// Who a run id belongs to: `Ok(None)` for a new run, `Ok(Some(confirmed))` for this node's run of this job;
/// a 409 when another job or node holds it.
fn owner(
    db: &ScopedConnection,
    node_id: i64,
    job_id: &str,
    run_id: &str,
) -> Result<Option<i64>, Box<Reply>> {
    let known: Result<Option<(i64, String, i64)>, _> = db
        .query_row(
            "SELECT node_id, job_id, confirmed FROM live_runs WHERE +org_id = :org_id AND run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional();
    let other_node: Result<Option<i64>, _> = db
        .query_row(
            "SELECT node_id FROM runs WHERE +org_id = :org_id AND run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )
        .optional();
    let (known, other_node) = match (known, other_node) {
        (Ok(k), Ok(o)) => (k, o),
        (Err(e), _) | (_, Err(e)) => return Err(Box::new(internal(&e))),
    };
    if known
        .as_ref()
        .is_some_and(|(n, j, _)| *n != node_id || *j != job_id)
        || other_node.is_some_and(|n| n != node_id)
    {
        return Err(Box::new(refusal(
            409,
            "conflict",
            "this run_id belongs to another job or node",
            Value::Null,
        )));
    }
    Ok(known.map(|k| k.2))
}

/// `POST runs/{run_id}/events`: record events in order without gaps; duplicates are dropped; past a gap
/// nothing is recorded and the answer names the next number wanted. Recorded events go to the clients.
pub fn post_events(state: &State, call: &Call, node_id: i64, node: &str, run_id: &str) -> Reply {
    let invalid = |d: &str| refusal(422, "invalid_request", d, Value::Null);
    if !util::plain_name(run_id, 200) || run_id.contains("..") {
        return invalid("run_id must be a plain name");
    }
    let (job_id, events) = match parse_events(&call.body) {
        Ok(p) => p,
        Err(e) => return invalid(&e),
    };
    let mut db = state.store.db();
    let ours = db
        .query_row(
            "SELECT 1 FROM jobs WHERE +org_id = :org_id AND job_id = ?1 AND node_id = ?2",
            params![job_id, node_id],
            |_| Ok(()),
        )
        .optional();
    match ours {
        Ok(Some(())) => {}
        Ok(None) => return refusal(404, "not_found", "no such job for this node", Value::Null),
        Err(e) => return internal(&e),
    }
    let known = match owner(&db, node_id, &job_id, run_id) {
        Ok(k) => k,
        Err(reply) => return *reply,
    };
    let mut confirmed = known.unwrap_or(0);
    let (mut accepted, mut duplicates, mut gap) = (Vec::new(), 0, None);
    for e in &events {
        if e.seq <= confirmed {
            duplicates += 1;
        } else if e.seq == confirmed + 1 {
            confirmed += 1;
            accepted.push(e);
        } else {
            gap = Some(json!({"expected": confirmed + 1}));
            break;
        }
    }
    let answer = json!({"run_id": run_id, "confirmed": confirmed, "accepted": accepted.len(), "duplicates": duplicates,
                         "gap": gap});
    if accepted.is_empty() {
        return json(200, &answer);
    }
    let stamp = now_utc();
    let recorded = (|| -> rusqlite::Result<()> {
        let tx = crate::store::write_tx(&mut db)?;
        tx.execute(
            "INSERT INTO live_runs (org_id, run_id, node_id, job_id, confirmed, first_at, last_at) VALUES (:org_id, ?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(run_id) DO UPDATE SET confirmed = excluded.confirmed, last_at = excluded.last_at WHERE +live_runs.org_id = :org_id",
            params![run_id, node_id, job_id, confirmed, stamp],
        )?;
        for e in &accepted {
            tx.execute(
                "INSERT INTO run_events (org_id, run_id, seq, kind, partial, payload, at, received_at)
                 VALUES (:org_id, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    run_id,
                    e.seq,
                    e.kind,
                    e.partial,
                    e.payload.to_string(),
                    e.at,
                    stamp
                ],
            )?;
        }
        if known.is_none() {
            // The job now lists this run as live.
            touch(&tx, Record::Job, &job_id)?;
        }
        tx.commit()
    })();
    drop(db);
    if let Err(e) = recorded {
        return internal(&e);
    }
    let tag = conversation_of(&state.store.db(), &job_id);
    let published: Vec<Value> = accepted
        .iter()
        .map(|e| {
            let mut ev = json!({"run_id": run_id, "job_id": job_id, "node": node, "seq": e.seq, "kind": e.kind,
                                "partial": e.partial, "payload": e.payload, "at": e.at});
            if let Some((conversation, field, id)) = &tag {
                ev["conversation_id"] = json!(conversation);
                ev[*field] = json!(id);
            }
            ev
        })
        .collect();
    state.hub.run_events(published);
    json(200, &answer)
}

/// The conversation a job serves, as `(conversation_id, "turn_id" | "summary_id", id)`.
pub fn conversation_tag(
    db: &ScopedConnection,
    job_id: &str,
) -> Option<(String, &'static str, String)> {
    conversation_of(db, job_id)
}

fn conversation_of(db: &ScopedConnection, job_id: &str) -> Option<(String, &'static str, String)> {
    db.query_row(
        "SELECT conversation_id, 'turn_id', turn_id FROM turns WHERE +org_id = :org_id AND job_id = ?1
         UNION ALL SELECT conversation_id, 'summary_id', summary_id FROM summaries WHERE +org_id = :org_id AND job_id = ?1 LIMIT 1",
        params![job_id],
        |r| {
            let field: String = r.get(1)?;
            Ok((r.get(0)?, if field == "turn_id" { "turn_id" } else { "summary_id" }, r.get(2)?))
        },
    )
    .ok()
}

/// `GET runs/{id}/events?after=`: a run's live events, for clients. `None` when the run is unknown.
pub fn read_events(
    db: &ScopedConnection,
    run_id: &str,
    after: i64,
) -> rusqlite::Result<Option<Value>> {
    let live: Option<(String, String, i64)> = db
        .query_row(
            "SELECT l.job_id, n.name, l.confirmed FROM (SELECT * FROM live_runs WHERE +org_id = :org_id) l JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.id = l.node_id
             WHERE l.run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let finished: Option<(Option<String>, String)> = db
        .query_row(
            "SELECT (SELECT j.job_id FROM jobs j WHERE +org_id = :org_id AND j.run_id = r.run_id LIMIT 1), n.name
             FROM (SELECT * FROM runs WHERE +org_id = :org_id) r JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.id = r.node_id WHERE r.run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (job, node, confirmed) = match (&live, &finished) {
        (Some((j, n, c)), _) => (Some(j.clone()), n.clone(), *c),
        (None, Some((j, n))) => (j.clone(), n.clone(), 0),
        (None, None) => return Ok(None),
    };
    let mut stmt = db.prepare_cached(
        "SELECT seq, kind, partial, payload, at, received_at FROM run_events WHERE +org_id = :org_id AND run_id = ?1 AND seq > ?2
         ORDER BY seq LIMIT ?3",
    )?;
    let events = stmt
        .query_map(params![run_id, after, PAGE], |r| {
            Ok(json!({"seq": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?,
                      "partial": r.get::<_, bool>(2)?,
                      "payload": serde_json::from_str::<Value>(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
                      "at": r.get::<_, Option<String>>(4)?, "received_at": r.get::<_, String>(5)?}))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(
        json!({"run_id": run_id, "job_id": job, "node": node, "confirmed": confirmed,
                    "finished": finished.is_some(), "events": events}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_checked() {
        let p = |s: &str| parse_events(s.as_bytes());
        let good = r#"{"job_id":"j","events":[{"seq":1,"kind":"delta","partial":true,"payload":{"text":"a"}},
                       {"seq":2,"kind":"stop","partial":true,"payload":{}}]}"#;
        assert_eq!(p(good).map(|(_, e)| e.len()), Ok(2));
        assert!(p(r#"{"job_id":"j","events":[]}"#).is_ok());
        for bad in [
            r#"{"events":[]}"#,
            r#"{"job_id":"j"}"#,
            r#"{"job_id":"j","events":[{"seq":0,"kind":"d","partial":true,"payload":{}}]}"#,
            r#"{"job_id":"j","events":[{"seq":1,"kind":"Delta","partial":true,"payload":{}}]}"#,
            r#"{"job_id":"j","events":[{"seq":1,"kind":"d","payload":{}}]}"#,
            r#"{"job_id":"j","events":[{"seq":1,"kind":"d","partial":true,"payload":"x"}]}"#,
            r#"{"job_id":"j","events":[{"seq":2,"kind":"d","partial":true,"payload":{}},
                                       {"seq":1,"kind":"d","partial":true,"payload":{}}]}"#,
        ] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn listening_counts_holds() {
        assert!(!listening(-7));
        listening_change(-7, true);
        {
            let _a = Held(-7);
            listening_change(-7, true);
            assert!(listening(-7));
        }
        assert!(listening(-7), "one hold still open");
        listening_change(-7, false);
        assert!(!listening(-7));
    }
}
