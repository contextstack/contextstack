//! The fleet (NODE-PROTOCOL.md §9.4, §9.5; CLIENT-API.md §4.3, §4.4, §6.3): the protocol each node speaks, its
//! status reports, the schedules it holds and their firings, and clients' requests to change a schedule, which
//! travel as control messages.

use crate::access::Client;
use crate::control;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::{Record, touch};
use crate::util::{self, now_utc};
use crate::work::{self, No};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

/// The most schedules, firings or active jobs in one report.
const MAX_ITEMS: usize = 1000;
/// Firing states a node may report.
const FIRING_STATES: [&str; 6] = [
    "recorded",
    "started",
    "done",
    "error",
    "missed",
    "uncertain",
];

/// Record the protocol version a node spoke, when it changed.
pub fn note_protocol(
    db: &Connection,
    node_id: i64,
    name: &str,
    version: u32,
) -> rusqlite::Result<()> {
    let changed = db.execute(
        "UPDATE nodes SET protocol = ?1 WHERE id = ?2 AND protocol IS NOT ?1",
        params![version, node_id],
    )?;
    if changed > 0 {
        touch(db, Record::Node, name)?;
    }
    Ok(())
}

fn invalid(detail: &str) -> Reply {
    refusal(422, "invalid_request", detail, Value::Null)
}

fn object(body: &[u8]) -> Result<Map<String, Value>, String> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err("the body must be a JSON object".into()),
    }
}

/// An optional string field of at most `max` bytes, kept as JSON.
fn opt_text(m: &Map<String, Value>, field: &str, max: usize) -> Result<Value, String> {
    match m.get(field) {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::String(s)) if s.len() <= max => Ok(json!(s)),
        Some(_) => Err(format!("{field} must be a string of at most {max} bytes")),
    }
}

fn opt_u64(v: Option<&Value>) -> Result<Value, String> {
    match v {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::Number(n)) if n.is_u64() => Ok(json!(n)),
        Some(Value::Number(n)) if n.as_i64().is_some_and(|i| i >= 0) => Ok(json!(n)),
        Some(_) => Err("vitals fields must be null or a non-negative integer".into()),
    }
}

fn vitals_body(v: &Value) -> Result<Value, String> {
    let m = v
        .as_object()
        .ok_or_else(|| "vitals must be an object".to_string())?;
    let mut out = Map::new();
    for field in [
        "memory_total_bytes",
        "memory_available_bytes",
        "swap_total_bytes",
        "swap_used_bytes",
        "disk_free_bytes",
    ] {
        if m.contains_key(field) {
            out.insert(field.into(), opt_u64(m.get(field))?);
        }
    }
    match m.get("memory_pressure") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s.len() <= 40 => {
            out.insert("memory_pressure".into(), json!(s));
        }
        Some(_) => {
            return Err("vitals.memory_pressure must be a string of at most 40 bytes".into());
        }
    }
    Ok(Value::Object(out))
}

fn held_body(v: &Value) -> Result<Value, String> {
    let m = v
        .as_object()
        .ok_or_else(|| "held must be an object".to_string())?;
    let reason = m
        .get("reason")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 80)
        .ok_or_else(|| "held.reason is required".to_string())?;
    Ok(json!({"reason": reason}))
}

/// `provider_limits`: the node's usage-limited templates, `[{template, message?, resets_at?, recorded_at?}]`.
fn provider_limits_body(m: &Map<String, Value>) -> Result<Value, String> {
    let mut out = Vec::new();
    for (i, row) in list(m, "provider_limits")?.iter().enumerate() {
        let r = row
            .as_object()
            .ok_or(format!("provider_limits[{i}] must be an object"))?;
        let template = r
            .get("template")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty() && t.len() <= 200)
            .ok_or(format!("provider_limits[{i}].template is required"))?;
        out.push(json!({"template": template, "message": opt_text(r, "message", 500)?,
                        "resets_at": opt_text(r, "resets_at", 40)?, "recorded_at": opt_text(r, "recorded_at", 40)?}));
    }
    Ok(json!(out))
}

fn list<'a>(m: &'a Map<String, Value>, field: &str) -> Result<&'a [Value], String> {
    match m.get(field) {
        None => Ok(&[]),
        Some(Value::Array(a)) if a.len() <= MAX_ITEMS => Ok(a),
        Some(_) => Err(format!("{field} must be a list of at most {MAX_ITEMS}")),
    }
}

/// Public checkout inventory; each status report replaces the previous inventory.
fn repos_body(m: &Map<String, Value>) -> Result<Value, String> {
    let mut out = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for row in list(m, "repos")? {
        let row = row.as_object().ok_or("repos entries must be objects")?;
        let mut repo = Map::new();
        for (field, max) in [("name", 63), ("path", 4096), ("branch", 1024)] {
            let value = row
                .get(field)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= max)
                .ok_or(format!(
                    "repos.{field} must be a nonempty string of at most {max} bytes"
                ))?;
            repo.insert(field.into(), json!(value));
        }
        if !names.insert(repo["name"].clone().to_string()) {
            return Err("duplicate repo name".into());
        }
        out.push(Value::Object(repo));
    }
    Ok(json!(out))
}

fn status_body(body: &[u8]) -> Result<Value, String> {
    let m = object(body)?;
    let mut active = Vec::new();
    for (i, a) in list(&m, "active")?.iter().enumerate() {
        let a = a
            .as_object()
            .ok_or(format!("active[{i}] must be an object"))?;
        let job = a
            .get("job_id")
            .and_then(Value::as_str)
            .filter(|j| !j.is_empty() && j.len() <= 200)
            .ok_or(format!("active[{i}].job_id is required"))?;
        let state = a
            .get("state")
            .and_then(Value::as_str)
            .filter(|s| matches!(*s, "queued" | "running"))
            .ok_or(format!("active[{i}].state is queued or running"))?;
        active.push(
            json!({"job_id": job, "state": state, "run_id": opt_text(a, "run_id", 200)?,
                           "started_at": opt_text(a, "started_at", 40)?}),
        );
    }
    let mut out = Map::from_iter([
        ("active".into(), json!(active)),
        ("repos".into(), repos_body(&m)?),
        ("node_version".into(), opt_text(&m, "node_version", 200)?),
    ]);
    if let Some(v) = m.get("vitals") {
        out.insert("vitals".into(), vitals_body(v)?);
    }
    if let Some(h) = m.get("held") {
        out.insert("held".into(), held_body(h)?);
    }
    if let Some(caps) = m.get("agent_capabilities") {
        let caps = caps
            .as_object()
            .ok_or("agent_capabilities must be an object")?;
        let templates = list(caps, "templates")?;
        if templates
            .iter()
            .any(|v| v.as_str().is_none_or(|s| s.is_empty() || s.len() > 100))
        {
            return Err("agent_capabilities.templates must contain template names".into());
        }
        out.insert(
            "agent_capabilities".into(),
            json!({"templates": templates, "preferred": opt_text(caps, "preferred", 100)?}),
        );
    }
    if m.contains_key("provider_limits") {
        out.insert("provider_limits".into(), provider_limits_body(&m)?);
    }
    Ok(Value::Object(out))
}

/// `POST status`: the node's active jobs; each report replaces the last.
pub fn post_status(state: &State, call: &Call, node_id: i64, name: &str) -> Reply {
    let status = match status_body(&call.body) {
        Ok(s) => s,
        Err(e) => return invalid(&e),
    };
    let stamp = now_utc();
    let mut db = state.store.db();
    let done = (|| -> rusqlite::Result<()> {
        let tx = crate::store::write_tx(&mut db)?;
        tx.execute(
            "UPDATE nodes SET status = ?1, status_at = ?2 WHERE id = ?3",
            params![status.to_string(), stamp, node_id],
        )?;
        touch(&tx, Record::Node, name)?;
        tx.commit()
    })();
    match done {
        Ok(()) => json(200, &json!({"recorded_at": stamp})),
        Err(e) => internal(&e),
    }
}

fn firing_id_ok(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphanumeric())
        && s.len() <= 200
        && c.all(|x| x.is_ascii_alphanumeric() || matches!(x, '.' | '_' | ':' | '@' | '-'))
}

/// A reported schedule, normalised: `(schedule_id, revision, body)`.
fn schedule(i: usize, s: &Value) -> Result<(String, i64, Value), String> {
    let s = s
        .as_object()
        .ok_or(format!("schedules[{i}] must be an object"))?;
    let bad = |what: &str| format!("schedules[{i}]: {what}");
    let id = s
        .get("schedule_id")
        .and_then(Value::as_str)
        .filter(|id| util::plain_name(id, 128))
        .ok_or_else(|| bad("schedule_id matches [A-Za-z0-9][A-Za-z0-9._-]{0,127}"))?;
    let revision = s
        .get("revision")
        .and_then(Value::as_i64)
        .filter(|r| *r >= 1)
        .ok_or_else(|| bad("revision is an integer >= 1"))?;
    let enabled = s
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| bad("enabled is a boolean"))?;
    let missed = match s.get("missed") {
        None | Some(Value::Null) => "skip",
        Some(Value::String(m)) if matches!(m.as_str(), "skip" | "catch_up") => m.as_str(),
        Some(_) => return Err(bad("missed is skip or catch_up")),
    };
    let definition = match s.get("definition") {
        None | Some(Value::Null) => json!({}),
        Some(d @ Value::Object(_)) => d.clone(),
        Some(_) => return Err(bad("definition is an object")),
    };
    let mut body = json!({"enabled": enabled, "missed": missed, "definition": definition});
    for f in ["cron", "timezone", "spec", "task", "next_at"] {
        body[f] = opt_text(s, f, 500).map_err(|e| bad(&e))?;
    }
    Ok((id.to_string(), revision, body))
}

/// A reported firing, normalised: `(firing_id, schedule_id, recorded_at, body)`.
fn firing(i: usize, f: &Value) -> Result<(String, String, String, Value), String> {
    let f = f
        .as_object()
        .ok_or(format!("firings[{i}] must be an object"))?;
    let bad = |what: &str| format!("firings[{i}]: {what}");
    let s = |k: &str| f.get(k).and_then(Value::as_str);
    let id = s("firing_id")
        .filter(|id| firing_id_ok(id))
        .ok_or_else(|| bad("firing_id matches [A-Za-z0-9][A-Za-z0-9._:@-]{0,199}"))?;
    let schedule = s("schedule_id")
        .filter(|id| util::plain_name(id, 128))
        .ok_or_else(|| bad("schedule_id is a schedule's id"))?;
    let slot = s("slot")
        .filter(|v| !v.is_empty() && v.len() <= 200)
        .ok_or_else(|| bad("slot is a string"))?;
    let state = s("state")
        .filter(|v| FIRING_STATES.contains(v))
        .ok_or_else(|| bad(&format!("state is one of {}", FIRING_STATES.join(" | "))))?;
    let recorded_at = s("recorded_at")
        .filter(|v| !v.is_empty() && v.len() <= 40)
        .ok_or_else(|| bad("recorded_at is a timestamp string"))?;
    let revision = match f.get("schedule_revision") {
        None | Some(Value::Null) => Value::Null,
        Some(v) if v.as_i64().is_some_and(|r| r >= 1) => v.clone(),
        Some(_) => return Err(bad("schedule_revision is an integer >= 1")),
    };
    let catch_up = match f.get("catch_up") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad("catch_up is a boolean")),
    };
    let body = json!({"firing_id": id, "schedule_id": schedule, "slot": slot, "state": state,
                      "schedule_revision": revision, "recorded_at": recorded_at, "catch_up": catch_up,
                      "job_id": opt_text(f, "job_id", 200).map_err(|e| bad(&e))?,
                      "run_id": opt_text(f, "run_id", 200).map_err(|e| bad(&e))?,
                      "detail": opt_text(f, "detail", 4000).map_err(|e| bad(&e))?});
    Ok((id.into(), schedule.into(), recorded_at.into(), body))
}

type Reported = (
    Vec<(String, i64, Value)>,
    Vec<(String, String, String, Value)>,
);

fn schedules_body(body: &[u8]) -> Result<Reported, String> {
    let m = object(body)?;
    if !m.get("schedules").is_some_and(Value::is_array) {
        return Err("schedules is the node's complete list".into());
    }
    let schedules = list(&m, "schedules")?
        .iter()
        .enumerate()
        .map(|(i, s)| schedule(i, s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut ids: Vec<&str> = schedules.iter().map(|s| s.0.as_str()).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err("each schedule_id once".into());
    }
    let firings = list(&m, "firings")?
        .iter()
        .enumerate()
        .map(|(i, f)| firing(i, f))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((schedules, firings))
}

/// Store one report: schedules replace the node's set (missing ones are marked removed); firings are kept by id.
/// A schedule's record changes only when what it shows changed.
fn record_schedules(
    tx: &Connection,
    node_id: i64,
    node: &str,
    report: &Reported,
    stamp: &str,
) -> rusqlite::Result<()> {
    let key = |id: &str| format!("{node}/{id}");
    for (id, revision, body) in &report.0 {
        let k = key(id);
        let before: Option<(i64, String, Option<String>)> = tx
            .query_row(
                "SELECT schedule_revision, body, removed_at FROM schedules WHERE schedule_key = ?1",
                params![k],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let text = body.to_string();
        if before.is_some_and(|(r, b, gone)| r == *revision && b == text && gone.is_none()) {
            continue;
        }
        tx.execute(
            "INSERT INTO schedules (schedule_key, node_id, schedule_id, schedule_revision, body, reported_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(schedule_key) DO UPDATE SET schedule_revision = excluded.schedule_revision,
               body = excluded.body, reported_at = excluded.reported_at, removed_at = NULL",
            params![k, node_id, id, revision, text, stamp],
        )?;
        touch(tx, Record::Schedule, &k)?;
    }
    let listed: Vec<String> = report.0.iter().map(|s| key(&s.0)).collect();
    let mut stmt =
        tx.prepare("SELECT schedule_key FROM schedules WHERE node_id = ?1 AND removed_at IS NULL")?;
    let held: Vec<String> = stmt
        .query_map(params![node_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for k in held.iter().filter(|k| !listed.contains(k)) {
        tx.execute(
            "UPDATE schedules SET removed_at = ?1, reported_at = ?1 WHERE schedule_key = ?2",
            params![stamp, k],
        )?;
        touch(tx, Record::Schedule, k)?;
    }
    for (id, schedule_id, recorded_at, body) in &report.1 {
        let text = body.to_string();
        let same = tx
            .query_row(
                "SELECT 1 FROM firings WHERE node_id = ?1 AND firing_id = ?2 AND body = ?3",
                params![node_id, id, text],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if same {
            continue;
        }
        tx.execute(
            "INSERT INTO firings (node_id, firing_id, schedule_id, body, recorded_at, reported_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(node_id, firing_id) DO UPDATE SET schedule_id = excluded.schedule_id, body = excluded.body,
               recorded_at = excluded.recorded_at, reported_at = excluded.reported_at",
            params![node_id, id, schedule_id, text, recorded_at, stamp],
        )?;
        let k = key(schedule_id);
        if tx
            .query_row(
                "SELECT 1 FROM schedules WHERE schedule_key = ?1",
                params![k],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            touch(tx, Record::Schedule, &k)?;
        }
    }
    Ok(())
}

/// `POST schedules`: the node's complete set of schedules, and its new or changed firings.
pub fn post_schedules(state: &State, call: &Call, node_id: i64, node: &str) -> Reply {
    let report = match schedules_body(&call.body) {
        Ok(r) => r,
        Err(e) => return invalid(&e),
    };
    let stamp = now_utc();
    let mut db = state.store.db();
    let done = (|| -> rusqlite::Result<()> {
        let tx = crate::store::write_tx(&mut db)?;
        record_schedules(&tx, node_id, node, &report, &stamp)?;
        tx.commit()
    })();
    match done {
        Ok(()) => json(
            200,
            &json!({"schedules": report.0.len(), "firings": report.1.len(), "recorded_at": stamp}),
        ),
        Err(e) => internal(&e),
    }
}

/// A schedule change's body, normalised: `{expect_revision, set}`.
fn change_body(m: &Map<String, Value>) -> Result<Value, No> {
    let expect = m
        .get("expect_revision")
        .and_then(Value::as_i64)
        .filter(|r| *r >= 1)
        .ok_or_else(|| No::invalid("expect_revision is the schedule_revision you saw"))?;
    let Some(Value::Object(set)) = m.get("set") else {
        return Err(No::invalid("set holds enabled and/or missed"));
    };
    if set.is_empty() {
        return Err(No::invalid("set changes something: enabled and/or missed"));
    }
    for (k, v) in set {
        let ok = match k.as_str() {
            "enabled" => v.is_boolean(),
            "missed" => matches!(v.as_str(), Some("skip" | "catch_up")),
            _ => false,
        };
        if !ok {
            return Err(No::invalid(&format!(
                "set.{k}: only enabled (boolean) and missed (skip | catch_up) can be changed"
            )));
        }
    }
    Ok(json!({"expect_revision": expect, "set": set}))
}

/// `POST schedules/{node}/{schedule_id}/changes`: a change request, sent to the node as a control message
/// naming the revision it expects.
pub fn request_change(
    state: &State,
    call: &Call,
    client: &Client,
    node: &str,
    schedule_id: &str,
) -> Reply {
    let (key, m) = match (work::key(call), work::body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    let want = match change_body(&m) {
        Ok(w) => w,
        Err(r) => return r.reply(),
    };
    let schedule_key = format!("{node}/{schedule_id}");
    let sha = work::content("schedule_change", &schedule_key, &want);
    let mut db = state.store.db();
    match work::prior(&db, &key, &sha) {
        Ok(Some((kind, id))) => return work::render(&db, 200, kind, &id, true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let found = db
        .query_row(
            "SELECT node_id, schedule_revision FROM schedules WHERE schedule_key = ?1 AND removed_at IS NULL",
            params![schedule_key],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional();
    let (node_id, current) = match found {
        Ok(Some(f)) => f,
        Ok(None) => return refusal(404, "not_found", "no such schedule", Value::Null),
        Err(e) => return internal(&e),
    };
    if Some(current) != want["expect_revision"].as_i64() {
        let now = crate::records::one(&db, Record::Schedule, &schedule_key)
            .ok()
            .flatten();
        return refusal(
            409,
            "schedule_revision_mismatch",
            &format!("the schedule is at revision {current}; look again and name that revision"),
            json!({"schedule": now}),
        );
    }
    let made = (|| -> Result<String, String> {
        let change_id = format!("sch_{}", util::ulid()?.to_lowercase());
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        let message = json!({"schedule_id": schedule_id, "expect_revision": want["expect_revision"],
                             "set": want["set"]});
        let control_id = control::issue(&tx, node_id, "schedule_change", &message)?;
        tx.execute(
            "INSERT INTO schedule_changes (change_id, schedule_key, control_id, expect_revision, change,
                                           requested_by, requested_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![change_id, schedule_key, control_id, want["expect_revision"].as_i64(),
                    want["set"].to_string(), client.id, now_utc()],
        )
        .map_err(|e| e.to_string())?;
        touch(&tx, Record::ScheduleChange, &change_id).map_err(|e| e.to_string())?;
        touch(&tx, Record::Schedule, &schedule_key).map_err(|e| e.to_string())?;
        work::remember(&tx, &key, &sha, Record::ScheduleChange, &change_id, client)
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(change_id)
    })();
    match made {
        Ok(id) => work::render(&db, 202, Record::ScheduleChange, &id, false),
        Err(e) => internal(&e),
    }
}

/// `GET schedules/{node}/{schedule_id}`: the schedule, its latest 100 firings and its changes.
pub fn schedule_detail(db: &Connection, schedule_key: &str) -> rusqlite::Result<Value> {
    let Some(schedule) = crate::records::one(db, Record::Schedule, schedule_key)? else {
        return Ok(Value::Null);
    };
    let mut stmt = db.prepare(
        "SELECT f.body, f.reported_at FROM firings f JOIN schedules s
           ON s.node_id = f.node_id AND s.schedule_id = f.schedule_id
         WHERE s.schedule_key = ?1 ORDER BY f.recorded_at DESC, f.rowid DESC LIMIT 100",
    )?;
    let firings = stmt
        .query_map(params![schedule_key], |r| {
            let mut f = serde_json::from_str::<Value>(&r.get::<_, String>(0)?).unwrap_or_default();
            f["reported_at"] = json!(r.get::<_, String>(1)?);
            Ok(f)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let changes = crate::records::select(
        db,
        Record::ScheduleChange,
        "c.schedule_key = ?1",
        &schedule_key,
    )?;
    Ok(json!({"schedule": schedule, "firings": firings, "changes": changes}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_reports_are_checked_and_missed_defaults_to_skip() -> Result<(), String> {
        let r = schedules_body(
            br#"{"schedules":[{"schedule_id":"nightly","revision":3,"enabled":true,"cron":"0 2 * * *"}],
                 "firings":[{"firing_id":"nightly@2026-11-10T02:00","schedule_id":"nightly",
                             "slot":"2026-11-10T02:00","state":"recorded","recorded_at":"2026-11-10T02:00:01Z"}]}"#,
        );
        let (s, f) = r?;
        assert_eq!(s[0].2["missed"], "skip");
        assert_eq!(f[0].3["catch_up"], false);
        for bad in [
            r#"{"firings":[]}"#,
            r#"{"schedules":[{"schedule_id":"a","revision":0,"enabled":true}]}"#,
            r#"{"schedules":[{"schedule_id":"a","revision":1,"enabled":true,"missed":"later"}]}"#,
            r#"{"schedules":[{"schedule_id":"a","revision":1,"enabled":true},{"schedule_id":"a","revision":2,"enabled":true}]}"#,
            r#"{"schedules":[],"firings":[{"firing_id":"x","schedule_id":"a","slot":"s","state":"ran","recorded_at":"t"}]}"#,
            r#"{"schedules":[],"firings":[{"firing_id":"/x","schedule_id":"a","slot":"s","state":"done","recorded_at":"t"}]}"#,
        ] {
            assert!(schedules_body(bad.as_bytes()).is_err(), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn changes_name_a_revision_and_known_fields() {
        let m = |s: &str| serde_json::from_str::<Map<String, Value>>(s).unwrap_or_default();
        assert!(change_body(&m(r#"{"expect_revision":2,"set":{"enabled":false}}"#)).is_ok());
        assert!(
            change_body(&m(
                r#"{"expect_revision":2,"set":{"missed":"catch_up","enabled":true}}"#
            ))
            .is_ok()
        );
        for bad in [
            r#"{"set":{"enabled":false}}"#,
            r#"{"expect_revision":2,"set":{}}"#,
            r#"{"expect_revision":2,"set":{"cron":"* * * * *"}}"#,
            r#"{"expect_revision":2,"set":{"missed":"later"}}"#,
        ] {
            assert!(change_body(&m(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_reports_are_checked() {
        assert!(
            status_body(br#"{"active":[{"job_id":"j","state":"running","run_id":"r"}]}"#).is_ok()
        );
        assert!(status_body(br#"{"active":[]}"#).is_ok());
        assert!(status_body(br#"{"active":[{"job_id":"j","state":"stopped"}]}"#).is_err());
        assert!(status_body(br#"{"active":[{"state":"queued"}]}"#).is_err());
        let vitals = match status_body(
            br#"{"active":[],"vitals":{"memory_total_bytes":100,"memory_available_bytes":5,
            "memory_pressure":"warn","disk_free_bytes":null},"held":{"reason":"memory_low"}}"#,
        ) {
            Ok(v) => v,
            Err(e) => panic!("vitals status: {e}"),
        };
        assert_eq!(vitals["held"]["reason"], "memory_low");
        assert_eq!(vitals["vitals"]["memory_pressure"], "warn");
        assert!(status_body(br#"{"active":[],"held":{}}"#).is_err());
        assert!(status_body(br#"{"active":[],"vitals":"x"}"#).is_err());
    }

    #[test]
    fn status_carries_agent_capabilities() -> Result<(), String> {
        let status = status_body(br#"{"active":[],"agent_capabilities":{"templates":["claude-code"],"preferred":"claude-code"}}"#)?;
        assert_eq!(
            status["agent_capabilities"]["templates"],
            json!(["claude-code"])
        );
        assert_eq!(status["agent_capabilities"]["preferred"], "claude-code");
        for body in [
            br#"{"active":[],"agent_capabilities":[]}"#.as_slice(),
            br#"{"active":[],"agent_capabilities":{"templates":[3]}}"#.as_slice(),
            br#"{"active":[],"agent_capabilities":{"templates":"claude-code"}}"#.as_slice(),
        ] {
            assert!(status_body(body).is_err());
        }
        Ok(())
    }

    #[test]
    fn status_carries_provider_limits() {
        let limited = match status_body(
            br#"{"active":[],"provider_limits":[{"template":"cursor-sol","message":"usage limit",
            "resets_at":"2099-01-01T00:00:00Z","recorded_at":"2026-11-14T00:00:00Z","extra":"dropped"}]}"#,
        ) {
            Ok(v) => v,
            Err(e) => panic!("limits status: {e}"),
        };
        assert_eq!(limited["provider_limits"][0]["template"], "cursor-sol");
        assert_eq!(
            limited["provider_limits"][0]["resets_at"],
            "2099-01-01T00:00:00Z"
        );
        assert!(limited["provider_limits"][0].get("extra").is_none());
        let none = status_body(br#"{"active":[]}"#).unwrap_or_default();
        assert!(none.get("provider_limits").is_none());
        assert!(status_body(br#"{"active":[],"provider_limits":[{"message":"x"}]}"#).is_err());
        assert!(status_body(br#"{"active":[],"provider_limits":"x"}"#).is_err());
    }
}
