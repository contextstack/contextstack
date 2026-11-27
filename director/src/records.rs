//! What a client sees: intents, jobs, runs, decision receipts, cancel requests, nodes, schedules and conversations
//! with their turns and summaries, each a flat JSON record
//! with its own `revision` (CLIENT-API.md §3). One place renders them, for the API and the event stream alike.

use crate::store::Record;
use crate::store::ScopedConnection;
use rusqlite::{OptionalExtension, Row, ToSql, params};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, Ordering};

/// A node not heard from for this long is shown as stale (`serve --stale-after`).
static STALE_AFTER: AtomicI64 = AtomicI64::new(300);

/// Set how long a node may be silent before it is shown as stale.
pub fn set_stale_after(seconds: i64) {
    STALE_AFTER.store(seconds, Ordering::Relaxed);
}

/// How long a node may be silent before it is shown as stale.
pub fn stale_after() -> i64 {
    STALE_AFTER.load(Ordering::Relaxed)
}

/// An offered job its node has not pulled for this long shows `waiting` (`serve --offer-grace`).
static OFFER_GRACE: AtomicI64 = AtomicI64::new(30);

/// Set how long an offered job may go unpulled before it shows `waiting`.
pub fn set_offer_grace(seconds: i64) {
    OFFER_GRACE.store(seconds, Ordering::Relaxed);
}

/// How long an offered job may go unpulled before it shows `waiting`.
pub fn offer_grace() -> i64 {
    OFFER_GRACE.load(Ordering::Relaxed)
}

/// What a job's `waiting` field is derived from.
#[derive(Debug, Default)]
pub(crate) struct Wait<'a> {
    /// The job's state.
    pub state: &'a str,
    /// Its node's name.
    pub node: &'a str,
    /// When it was offered (or created, for a job offered before `offered_at` was kept).
    pub since: &'a str,
    /// Seconds since `since`; `None` when unparseable.
    pub since_age: Option<i64>,
    /// When its node was last heard from.
    pub node_last_seen: Option<&'a str>,
    /// Seconds since `node_last_seen`; `None` when never seen.
    pub seen_age: Option<i64>,
    /// The hold's reason (`job_budgets.reason`), for a held job.
    pub hold_reason: Option<&'a str>,
    /// The hold's text (`job_budgets.detail`), for a held job.
    pub hold_detail: Option<&'a str>,
}

/// Why a job is not yet running, or null (CLIENT-API.md §11, "waiting"). Derived at read time; it does not by
/// itself change a record's `revision`.
pub(crate) fn waiting(w: &Wait, grace: i64, stale: i64) -> Value {
    match w.state {
        "offered" if w.since_age.is_none_or(|a| a > grace) => {
            let mut out = json!({"reason": "node_not_pulled", "node": w.node, "since": w.since});
            if w.seen_age.is_none_or(|a| a > stale) {
                out["node_last_seen"] = json!(w.node_last_seen);
            }
            out
        }
        "waiting" => json!({"reason": "held", "detail": w.hold_reason.unwrap_or("waiting"),
                            "message": w.hold_detail, "node": w.node}),
        _ => Value::Null,
    }
}

/// A job's `waiting` field, read from the store; null for an unknown job or one that is not waiting.
pub(crate) fn job_waiting(db: &ScopedConnection, job_id: &str) -> rusqlite::Result<Value> {
    let mut stmt = db.prepare_cached(
        "SELECT j.state, n.name, COALESCE(j.offered_at, j.created_at), n.last_seen_at, b.reason, b.detail
         FROM (SELECT rowid AS rowid, * FROM jobs WHERE +org_id = :org_id) j JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.id = j.node_id LEFT JOIN (SELECT * FROM job_budgets WHERE +org_id = :org_id) b ON b.job_id = j.job_id
         WHERE j.job_id = ?1 AND j.state IN ('offered', 'waiting')",
    )?;
    let row = stmt
        .query_row(params![job_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .optional()?;
    let Some((state, node, since, seen, reason, detail)) = row else {
        return Ok(Value::Null);
    };
    let w = Wait {
        state: &state,
        node: &node,
        since: &since,
        since_age: crate::util::seconds_since(&since),
        node_last_seen: seen.as_deref(),
        seen_age: seen.as_deref().and_then(crate::util::seconds_since),
        hold_reason: reason.as_deref(),
        hold_detail: detail.as_deref(),
    };
    Ok(waiting(&w, offer_grace(), stale_after()))
}

/// A record kind: its select, the column it is found by, and how a row becomes JSON.
pub(crate) struct View {
    pub(crate) select: &'static str,
    pub(crate) key: &'static str,
    pub(crate) order: &'static str,
    pub(crate) shape: fn(&ScopedConnection, &Row) -> rusqlite::Result<Value>,
}

fn parse(s: Option<String>) -> Option<Value> {
    s.and_then(|s| serde_json::from_str::<Value>(&s).ok())
}

/// SQL boolean for a task lane: same rule as [`is_lane_meta`] (`lanes=1` and derived `lane`).
pub(crate) const INTENT_IS_LANE_SQL: &str = "CASE WHEN json_valid(i.meta) THEN
               json_extract(i.meta, '$.source') IN ('client-web-lane', 'client-mobile-lane', 'cs-mcp')
               OR json_extract(i.meta, '$.lane') IS NOT NULL
             ELSE 0 END";

/// Whether intent `meta` marks a task lane (same rule as `lanes=1` and the derived `lane` field).
pub fn is_lane_meta(meta: &Value) -> bool {
    let Value::Object(map) = meta else {
        return false;
    };
    matches!(
        map.get("source").and_then(Value::as_str),
        Some("client-web-lane" | "client-mobile-lane" | "cs-mcp")
    ) || map.get("lane").is_some_and(|v| !v.is_null())
}

const INTENTS: View = View {
    select: "SELECT i.intent_id, i.title, i.node, i.spec, i.task, i.trials, i.grant_id, i.meta, i.bundle_sha256,
                    i.created_by, i.created_at, i.revision, i.project_id, i.lead_conversation_id, i.responsible_member_id
             FROM (SELECT rowid AS rowid, * FROM intents WHERE +org_id = :org_id) i",
    key: "i.intent_id",
    order: "i.rowid DESC",
    shape: |db, r| {
        let id: String = r.get(0)?;
        let mut stmt = db.prepare_cached("SELECT job_id FROM jobs WHERE +org_id = :org_id AND intent_id = ?1 ORDER BY rowid")?;
        let jobs = stmt
            .query_map(params![id], |j| j.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut stmt = db.prepare_cached("SELECT conversation_id FROM conversations WHERE +org_id = :org_id AND intent_id = ?1 ORDER BY rowid")?;
        let conversations = stmt
            .query_map(params![id], |c| c.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let meta = parse(r.get(7)?).unwrap_or(Value::Null);
        Ok(json!({"intent_id": id, "conversation_ids": conversations, "title": r.get::<_, Option<String>>(1)?, "node": r.get::<_, String>(2)?,
                  "spec": r.get::<_, String>(3)?, "task": r.get::<_, String>(4)?, "trials": r.get::<_, i64>(5)?,
                  "grant": r.get::<_, Option<String>>(6)?, "meta": meta,
                  "bundle_sha256": r.get::<_, String>(8)?, "created_by": r.get::<_, String>(9)?,
                  "created_at": r.get::<_, String>(10)?, "revision": r.get::<_, i64>(11)?, "job_ids": jobs,
                  "project_id": r.get::<_, Option<String>>(12)?,
                  "lead_conversation_id": r.get::<_, Option<String>>(13)?,
                  "responsible_member_id": r.get::<_, Option<String>>(14)?.unwrap_or_else(|| db.scope().member_id.clone()),
                  "lane": is_lane_meta(&meta)}))
    },
};

const JOBS: View = View {
    select: "SELECT j.job_id, n.name, j.state, j.spec, j.task, j.trials, j.bundle_sha256, j.grant_id, j.meta,
                    j.created_at, j.accepted_at, j.finished_at, j.run_id, j.terminal_row, j.revision, j.intent_id,
                    c.cancel_id, COALESCE(k.state, c.state), (SELECT r.run_id FROM runs r WHERE +org_id = :org_id AND r.run_id = j.run_id),
                    k.outcome,
                    COALESCE((SELECT t.conversation_id FROM turns t WHERE +org_id = :org_id AND t.job_id = j.job_id),
                             (SELECT s.conversation_id FROM summaries s WHERE +org_id = :org_id AND s.job_id = j.job_id)),
                    li.lead_conversation_id,
                    j.error,
                    ld.land_id, ld.state, ld.repo, ld.lane, ld.main, ld.\"commit\", ld.reason, ld.landed_by, ld.landed_at,
                    ld.attempts
             FROM (SELECT rowid AS rowid, * FROM jobs WHERE +org_id = :org_id) j JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.id = j.node_id LEFT JOIN (SELECT * FROM cancels WHERE +org_id = :org_id) c ON c.job_id = j.job_id
               LEFT JOIN (SELECT * FROM controls WHERE +org_id = :org_id) k ON k.control_id = c.control_id
               LEFT JOIN (SELECT * FROM intents WHERE +org_id = :org_id) li ON li.intent_id = j.intent_id
               LEFT JOIN (SELECT * FROM lands WHERE +org_id = :org_id) ld ON ld.run_id = j.run_id AND ld.current = 1",
    key: "j.job_id",
    order: "j.rowid DESC",
    shape: |db, r| {
        let id: String = r.get(0)?;
        let cancel = r
            .get::<_, Option<String>>(16)?
            .map(|c| json!({"cancel_id": c, "state": r.get::<_, Option<String>>(17).ok().flatten(),
                            "outcome": r.get::<_, Option<String>>(19).ok().flatten()}));
        // The run its end names, then each run whose report names this job (every trial's run).
        let mut runs: Vec<String> = r.get::<_, Option<String>>(18)?.into_iter().collect();
        let mut named = db.prepare_cached("SELECT run_id FROM runs WHERE +org_id = :org_id AND job_id = ?1 ORDER BY rowid")?;
        for run in named.query_map(params![id], |l| l.get::<_, String>(0))? {
            let run = run?;
            if !runs.contains(&run) {
                runs.push(run);
            }
        }
        let mut live = db.prepare_cached("SELECT run_id FROM live_runs WHERE +org_id = :org_id AND job_id = ?1 ORDER BY rowid")?;
        let live: Vec<String> = live
            .query_map(params![id], |l| l.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(json!({"job_id": id, "node": r.get::<_, String>(1)?, "state": r.get::<_, String>(2)?,
                  "spec": r.get::<_, String>(3)?, "task": r.get::<_, String>(4)?, "trials": r.get::<_, i64>(5)?,
                  "bundle_sha256": r.get::<_, String>(6)?, "grant": r.get::<_, Option<String>>(7)?,
                  "meta": parse(r.get(8)?), "created_at": r.get::<_, String>(9)?,
                  "accepted_at": r.get::<_, Option<String>>(10)?, "finished_at": r.get::<_, Option<String>>(11)?,
                  "run_id": r.get::<_, Option<String>>(12)?, "terminal_row": parse(r.get(13)?),
                  "revision": r.get::<_, i64>(14)?, "intent_id": r.get::<_, Option<String>>(15)?,
                  "cancel": cancel, "run_ids": runs, "live_run_ids": live,
                  "conversation_id": r.get::<_, Option<String>>(20)?,
                  "lead_conversation_id": r.get::<_, Option<String>>(21)?,
                  "error": r.get::<_, Option<String>>(22)?,
                  "failure": crate::failure::classify(&r.get::<_, String>(2)?, &parse(r.get(13)?).unwrap_or(Value::Null),
                      r.get::<_, Option<String>>(22)?.as_deref(), None),
                  "land": land_summary(r, 23, 32)?,
                  "land_state": r.get::<_, Option<String>>(24)?,
                  "budget": crate::scopes::job_view(db, &id)?,
                  "waiting": job_waiting(db, &id)?,
                  "files": crate::jobs::files(db, &id)?}))
    },
};

const RUNS: View = View {
    select: "SELECT r.run_id, n.name, r.usage, r.verdict, r.transcript, r.row_line, r.received_at, r.revision,
                    COALESCE(r.job_id, (SELECT j.job_id FROM jobs j WHERE +org_id = :org_id AND j.run_id = r.run_id LIMIT 1)),
                    COALESCE((SELECT j.intent_id FROM jobs j WHERE +org_id = :org_id AND j.job_id = r.job_id),
                             (SELECT j.intent_id FROM jobs j WHERE +org_id = :org_id AND j.run_id = r.run_id LIMIT 1)),
                    d.decision_id, d.decision, d.decided_at,
                    ld.land_id, ld.state, ld.repo, ld.lane, ld.main, ld.\"commit\", ld.reason, ld.landed_by, ld.landed_at,
                    (SELECT l.confirmed FROM live_runs l WHERE +org_id = :org_id AND l.run_id = r.run_id),
                    COALESCE((SELECT t.conversation_id FROM (SELECT * FROM turns WHERE +org_id = :org_id) t JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id = t.job_id
                              WHERE j.run_id = r.run_id OR j.job_id = r.job_id),
                             (SELECT s.conversation_id FROM (SELECT rowid AS rowid, * FROM summaries WHERE +org_id = :org_id) s JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id = s.job_id
                              WHERE j.run_id = r.run_id OR j.job_id = r.job_id)),
                    ld.attempts
             FROM (SELECT rowid AS rowid, * FROM runs WHERE +org_id = :org_id) r JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.id = r.node_id LEFT JOIN (SELECT * FROM decisions WHERE +org_id = :org_id) d ON d.run_id = r.run_id
               LEFT JOIN (SELECT * FROM lands WHERE +org_id = :org_id) ld ON ld.run_id = r.run_id AND ld.current = 1",
    key: "r.run_id",
    order: "r.rowid DESC",
    shape: |db, r| {
        let id: String = r.get(0)?;
        let mut stmt = db
            .prepare("SELECT path, sha256, bytes FROM run_files WHERE +org_id = :org_id AND run_id = ?1 ORDER BY path")?;
        let files = stmt
            .query_map(params![id], |f| {
                Ok(
                    json!({"path": f.get::<_, String>(0)?, "sha256": f.get::<_, String>(1)?,
                          "bytes": f.get::<_, i64>(2)?}),
                )
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let decision = r.get::<_, Option<String>>(10)?.map(|d| {
            json!({"decision_id": d, "decision": r.get::<_, String>(11).ok(),
                   "decided_at": r.get::<_, String>(12).ok()})
        });
        Ok(
            json!({"run_id": id, "node": r.get::<_, String>(1)?, "usage": parse(r.get(2)?),
                  "verdict": r.get::<_, Option<String>>(3)?, "transcript": r.get::<_, Option<String>>(4)?,
                  "row": parse(r.get(5)?), "received_at": r.get::<_, String>(6)?,
                  "revision": r.get::<_, i64>(7)?, "job_id": r.get::<_, Option<String>>(8)?,
                  "intent_id": r.get::<_, Option<String>>(9)?, "decision": decision,
                  "land": land_summary(r, 13, 24)?, "files": files,
                  "events": r.get::<_, Option<i64>>(22)?.unwrap_or(0),
                  "conversation_id": r.get::<_, Option<String>>(23)?}),
        )
    },
};

const DECISIONS: View = View {
    select:
        "SELECT d.decision_id, d.run_id, d.decision, d.note, d.decided_by, d.decided_at, d.revision,
                    (SELECT j.job_id FROM jobs j WHERE +org_id = :org_id AND j.run_id = d.run_id LIMIT 1),
                    (SELECT j.intent_id FROM jobs j WHERE +org_id = :org_id AND j.run_id = d.run_id LIMIT 1),
                    (SELECT c.name FROM credentials c WHERE +org_id = :org_id AND c.id = d.decided_by)
             FROM (SELECT rowid AS rowid, * FROM decisions WHERE +org_id = :org_id) d",
    key: "d.decision_id",
    order: "d.rowid DESC",
    shape: |_, r| {
        Ok(
            json!({"decision_id": r.get::<_, String>(0)?, "run_id": r.get::<_, String>(1)?,
                  "decision": r.get::<_, String>(2)?, "note": r.get::<_, Option<String>>(3)?,
                  "decided_by": r.get::<_, String>(4)?, "decided_by_name": r.get::<_, Option<String>>(9)?,
                  "decided_at": r.get::<_, String>(5)?, "revision": r.get::<_, i64>(6)?,
                  "job_id": r.get::<_, Option<String>>(7)?, "intent_id": r.get::<_, Option<String>>(8)?,
                  "scope": "run_result", "authorises": []}),
        )
    },
};

/// The land embedded on a job or run. `attempts_at` sits apart from the contiguous land columns so
/// later fields on the same row keep their indexes.
fn land_summary(r: &Row, base: usize, attempts_at: usize) -> rusqlite::Result<Value> {
    let id: Option<String> = r.get(base)?;
    Ok(match id {
        None => Value::Null,
        Some(land_id) => with_land_attempts(
            json!({
                "land_id": land_id,
                "state": r.get::<_, String>(base + 1)?,
                "repo": r.get::<_, String>(base + 2)?,
                "lane": r.get::<_, String>(base + 3)?,
                "main": r.get::<_, String>(base + 4)?,
                "commit": r.get::<_, Option<String>>(base + 5)?,
                "reason": r.get::<_, Option<String>>(base + 6)?,
                "landed_by": r.get::<_, String>(base + 7)?,
                "landed_at": r.get::<_, String>(base + 8)?,
            }),
            &r.get::<_, String>(attempts_at)?,
        ),
    })
}

fn with_land_attempts(mut land: Value, raw: &str) -> Value {
    let (attempts, coverage) = crate::land::decode_attempts(raw);
    land["attempts"] = attempts;
    if let Some(coverage) = coverage {
        land["host_probe_coverage"] = coverage;
    }
    land
}

const LANDS: View = View {
    select:
        "SELECT l.land_id, l.run_id, l.state, l.repo, l.lane, l.main, l.\"commit\", l.reason, l.landed_by, l.landed_at,
                    l.revision,
                    (SELECT j.job_id FROM jobs j WHERE +org_id = :org_id AND j.run_id = l.run_id LIMIT 1),
                    (SELECT j.intent_id FROM jobs j WHERE +org_id = :org_id AND j.run_id = l.run_id LIMIT 1),
                    l.attempts
             FROM (SELECT rowid AS rowid, * FROM lands WHERE +org_id = :org_id) l",
    key: "l.land_id",
    order: "l.rowid DESC",
    shape: |_, r| {
        Ok(with_land_attempts(json!({"land_id": r.get::<_, String>(0)?, "run_id": r.get::<_, String>(1)?,
                  "state": r.get::<_, String>(2)?, "repo": r.get::<_, String>(3)?, "lane": r.get::<_, String>(4)?,
                  "main": r.get::<_, String>(5)?, "commit": r.get::<_, Option<String>>(6)?,
                  "reason": r.get::<_, Option<String>>(7)?, "landed_by": r.get::<_, String>(8)?,
                  "landed_at": r.get::<_, String>(9)?, "revision": r.get::<_, i64>(10)?,
                  "job_id": r.get::<_, Option<String>>(11)?, "intent_id": r.get::<_, Option<String>>(12)?}),
                  &r.get::<_, String>(13)?))
    },
};

const CANCELS: View = View {
    select: "SELECT c.cancel_id, c.job_id, c.control_id, c.reason, c.requested_by, c.requested_at, c.revision,
                    j.intent_id, (SELECT k.name FROM credentials k WHERE +org_id = :org_id AND k.id = c.requested_by), n.name, n.protocol
             FROM (SELECT rowid AS rowid, * FROM cancels WHERE +org_id = :org_id) c JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id = c.job_id JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.id = j.node_id",
    key: "c.cancel_id",
    order: "c.rowid DESC",
    shape: |db, r| {
        let control: Option<String> = r.get(2)?;
        let mut out = crate::control::view(db, control.as_deref(), r.get(10)?)?;
        out.remove("result");
        out.extend([
            ("cancel_id".into(), json!(r.get::<_, String>(0)?)),
            ("job_id".into(), json!(r.get::<_, String>(1)?)),
            ("node".into(), json!(r.get::<_, String>(9)?)),
            ("reason".into(), json!(r.get::<_, Option<String>>(3)?)),
            ("requested_by".into(), json!(r.get::<_, String>(4)?)),
            ("requested_by_name".into(), json!(r.get::<_, Option<String>>(8)?)),
            ("requested_at".into(), json!(r.get::<_, String>(5)?)),
            ("revision".into(), json!(r.get::<_, i64>(6)?)),
            ("intent_id".into(), json!(r.get::<_, Option<String>>(7)?)),
        ]);
        Ok(Value::Object(out))
    },
};

const NODES: View = View {
    select: "SELECT n.id, n.name, n.created_at, n.last_seen_at, n.revoked_at, n.protocol, n.status, n.status_at,
                    n.revision,
                    (SELECT COUNT(*) FROM jobs j WHERE +org_id = :org_id AND j.node_id = n.id AND j.state IN ('offered', 'accepted'))
             FROM (SELECT * FROM nodes WHERE +org_id = :org_id) n",
    key: "n.name",
    order: "n.name",
    shape: |_, r| {
        let seen: Option<String> = r.get(3)?;
        let age = seen.as_deref().and_then(crate::util::seconds_since);
        let status = parse(r.get(6)?).map(|mut s| {
            let at: Option<String> = r.get(7).ok().flatten();
            s["seconds_since_report"] = json!(at.as_deref().and_then(crate::util::seconds_since));
            s["reported_at"] = json!(at);
            s
        });
        Ok(json!({"name": r.get::<_, String>(1)?, "created_at": r.get::<_, String>(2)?, "last_seen_at": seen,
                  "seconds_since_seen": age, "stale": age.is_none_or(|a| a > stale_after()),
                  "revoked_at": r.get::<_, Option<String>>(4)?, "open_jobs": r.get::<_, i64>(9)?,
                  "protocol": r.get::<_, Option<i64>>(5)?, "listening": crate::live::listening(r.get(0)?),
                  "status": status, "revision": r.get::<_, i64>(8)?}))
    },
};

const SCHEDULES: View = View {
    select: "SELECT s.schedule_key, n.name, s.schedule_id, s.schedule_revision, s.body, s.reported_at, s.removed_at,
                    s.revision, s.node_id, s.responsible_member_id
             FROM (SELECT rowid AS rowid, * FROM schedules WHERE +org_id = :org_id) s JOIN (SELECT rowid AS rowid, * FROM nodes WHERE +org_id = :org_id) n ON n.id = s.node_id",
    key: "s.schedule_key",
    order: "n.name, s.schedule_id",
    shape: |db, r| {
        let key: String = r.get(0)?;
        let (node_id, schedule_id): (i64, String) = (r.get(8)?, r.get(2)?);
        let last = db
            .query_row(
                "SELECT body FROM firings WHERE +org_id = :org_id AND node_id = ?1 AND schedule_id = ?2
                 ORDER BY recorded_at DESC, rowid DESC LIMIT 1",
                params![node_id, schedule_id],
                |f| f.get::<_, String>(0),
            )
            .ok();
        let mut stmt = db.prepare_cached(
            "SELECT c.change_id FROM (SELECT rowid AS rowid, * FROM schedule_changes WHERE +org_id = :org_id) c JOIN (SELECT rowid AS rowid, * FROM controls WHERE +org_id = :org_id) k ON k.control_id = c.control_id
             WHERE c.schedule_key = ?1 AND k.state IN ('requested', 'received') ORDER BY c.rowid",
        )?;
        let pending: Vec<String> = stmt.query_map(params![key], |c| c.get(0))?.collect::<Result<_, _>>()?;
        let mut out = parse(r.get(4)?).unwrap_or_else(|| json!({}));
        for (k, v) in [
            ("schedule_key", json!(key)),
            ("responsible_member_id", json!(r.get::<_, Option<String>>(9)?.unwrap_or_else(|| db.scope().member_id.clone()))),
            ("node", json!(r.get::<_, String>(1)?)),
            ("schedule_id", json!(schedule_id)),
            ("schedule_revision", json!(r.get::<_, i64>(3)?)),
            ("reported_at", json!(r.get::<_, String>(5)?)),
            ("removed_at", json!(r.get::<_, Option<String>>(6)?)),
            ("last_firing", parse(last).unwrap_or(Value::Null)),
            ("pending_change_ids", json!(pending)),
            ("revision", json!(r.get::<_, i64>(7)?)),
        ] {
            out[k] = v;
        }
        if out["definition"]["director_managed"] == true
            && let Some(name) = out["definition"]["name"].as_str()
        {
            out["name"] = json!(name);
        }
        if crate::scopes::budget_of(db, "schedule", &key)?.is_some() {
            out["budget"] = crate::scopes::scope_view(db, "schedule", &key)?;
        }
        Ok(out)
    },
};

const SCHEDULE_CHANGES: View = View {
    select: "SELECT c.change_id, c.schedule_key, s.schedule_id, n.name, c.control_id, c.expect_revision, c.change,
                    c.requested_by, c.requested_at, c.revision, n.protocol,
                    (SELECT k.name FROM credentials k WHERE +org_id = :org_id AND k.id = c.requested_by),
                    (SELECT json_extract(k.body, '$.kind') FROM controls k WHERE +org_id = :org_id AND k.control_id = c.control_id)
             FROM (SELECT rowid AS rowid, * FROM schedule_changes WHERE +org_id = :org_id) c JOIN (SELECT * FROM schedules WHERE +org_id = :org_id) s ON s.schedule_key = c.schedule_key
               JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.id = s.node_id",
    key: "c.change_id",
    order: "c.rowid DESC",
    shape: |db, r| {
        let control: String = r.get(4)?;
        let mut out = crate::control::view(db, Some(&control), r.get(10)?)?;
        let result = out.remove("result").unwrap_or(Value::Null);
        out.extend([
            ("change_id".into(), json!(r.get::<_, String>(0)?)),
            ("schedule_key".into(), json!(r.get::<_, String>(1)?)),
            ("schedule_id".into(), json!(r.get::<_, String>(2)?)),
            ("node".into(), json!(r.get::<_, String>(3)?)),
            ("expect_revision".into(), json!(r.get::<_, i64>(5)?)),
            ("set".into(), parse(r.get(6)?).unwrap_or_else(|| json!({}))),
            ("result_revision".into(), result.get("revision").cloned().unwrap_or(Value::Null)),
            ("requested_by".into(), json!(r.get::<_, String>(7)?)),
            ("requested_by_name".into(), json!(r.get::<_, Option<String>>(11)?)),
            ("requested_at".into(), json!(r.get::<_, String>(8)?)),
            ("revision".into(), json!(r.get::<_, i64>(9)?)),
        ]);
        if let Some(kind) = r.get::<_, Option<String>>(12)? {
            out.insert("kind".into(), json!(kind));
        }
        Ok(Value::Object(out))
    },
};

fn conversation_attention(r: &Row) -> rusqlite::Result<Value> {
    let question: Option<String> = r.get(14)?;
    Ok(match question {
        None => Value::Null,
        Some(q) => json!({"question": q, "job_id": r.get::<_, Option<String>>(15)?,
                          "set_at": r.get::<_, String>(16)?, "set_by": r.get::<_, String>(17)?,
                          "source_kind": r.get::<_, Option<String>>(18)?, "assignee_member_id": r.get::<_, Option<String>>(23)?}),
    })
}

fn turn_waiting(
    db: &ScopedConnection,
    conversation: &str,
    state: &str,
    job: Option<String>,
) -> rusqlite::Result<Value> {
    if !matches!(state, "queued" | "summarising" | "running") {
        return Ok(Value::Null);
    }
    let job = match job {
        None if state == "summarising" => db
            .query_row(
                "SELECT job_id FROM summaries WHERE +org_id = :org_id AND conversation_id = ?1 AND state = 'running'",
                params![conversation],
                |row| row.get(0),
            )
            .optional()?,
        job => job,
    };
    job.map_or(Ok(Value::Null), |job| job_waiting(db, &job))
}

const CONVERSATIONS: View = View {
    select: "SELECT c.conversation_id, c.title, c.node, c.spec, c.spec_version, c.model, c.intent_id, c.held,
                    c.created_by, c.created_at, c.revision, c.instructions_tokens, c.tools, c.project_id,
                    c.attention_question, c.attention_job_id, c.attention_set_at, c.attention_set_by,
                    (SELECT kind FROM credentials WHERE +org_id = :org_id AND id = c.attention_set_by), c.lead, c.archived_at, c.responsible_member_id,
                    COALESCE(c.sponsor_member_id,c.responsible_member_id), c.attention_assignee_member_id
             FROM (SELECT rowid AS rowid, * FROM conversations WHERE +org_id = :org_id) c",
    key: "c.conversation_id",
    order: "c.rowid DESC",
    shape: |db, r| {
        let id: String = r.get(0)?;
        let mut stmt = db.prepare_cached(
            "SELECT turn_id, state, job_id FROM turns
             WHERE +org_id = :org_id AND conversation_id = ?1 AND state IN ('queued', 'summarising', 'running') ORDER BY seq",
        )?;
        let open: Vec<(String, String, Option<String>)> = stmt
            .query_map(params![id], |t| Ok((t.get(0)?, t.get(1)?, t.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let active = open.iter().find(|(_, s, _)| s != "queued");
        let queued: Vec<&String> = open.iter().filter(|(_, s, _)| s == "queued").map(|(t, _, _)| t).collect();
        let waiting = match active {
            Some((_, state, job)) => turn_waiting(db, &id, state, job.clone())?,
            _ => Value::Null,
        };
        let (turns, summaries): (i64, i64) = db.query_row(
            "SELECT (SELECT COUNT(*) FROM turns WHERE +org_id = :org_id AND conversation_id = ?1),
                    (SELECT COUNT(*) FROM summaries WHERE +org_id = :org_id AND conversation_id = ?1)",
            params![id],
            |c| Ok((c.get(0)?, c.get(1)?)),
        )?;
        let held: bool = r.get(7)?;
        let state = match (held, active) {
            (true, _) => "held",
            (false, Some((_, s, _))) => s.as_str(),
            (false, None) => "idle",
        };
        let model: String = r.get(5)?;
        let budget = crate::thread::budget(&model, r.get(11)?, r.get(12)?);
        let assignee: Option<String> = r.get(23)?;
        let raised_by: Option<String> = r.get(17)?;
        let attention = if crate::policy::attention_visible(assignee.as_deref(), raised_by.as_deref()) {
            conversation_attention(r)?
        } else {
            Value::Null
        };
        let attention = if attention.is_null() {
            crate::action_approvals::attention(db, &id)?
        } else {
            attention
        };
        let attention = if attention.is_null() && r.get::<_, bool>(19)? {
            crate::backlog::attention(db, &id)?
        } else {
            attention
        };
        Ok(json!({"conversation_id": id, "title": r.get::<_, Option<String>>(1)?, "node": r.get::<_, String>(2)?,
                  "spec": r.get::<_, String>(3)?, "spec_version": r.get::<_, String>(4)?, "model": model,
                  "intent_id": r.get::<_, Option<String>>(6)?, "held": held, "state": state,
                  "active_turn_id": active.map(|(t, _, _)| t), "waiting": waiting,
                  "queued_turn_ids": queued, "turns": turns,
                  "summaries": summaries,
                  "compaction": {"context_window": budget.window, "window_source": budget.source,
                                 "threshold_tokens": budget.threshold, "keep_tokens": budget.keep},
                  "created_by": r.get::<_, String>(8)?, "created_at": r.get::<_, String>(9)?,
                  "project_id": r.get::<_, Option<String>>(13)?,
                  "attention": attention,
                  "budget": crate::scopes::scope_view(db, "conversation", &id)?,
                  "lead": r.get::<_, i64>(19)? != 0,
                  "responsible_member_id": r.get::<_, Option<String>>(21)?.unwrap_or_else(|| db.scope().member_id.clone()),
                  "sponsor_member_id": r.get::<_, Option<String>>(22)?.unwrap_or_else(|| db.scope().member_id.clone()),
                  "archived_at": r.get::<_, Option<String>>(20)?,
                  "revision": r.get::<_, i64>(10)?}))
    },
};

const PROJECTS: View = View {
    select: "SELECT p.project_id, p.name, p.created_by, p.created_at, p.revision FROM (SELECT rowid AS rowid, * FROM projects WHERE +org_id = :org_id) p",
    key: "p.project_id",
    order: "p.rowid DESC",
    shape: |db, r| {
        let id: String = r.get(0)?;
        Ok(
            json!({"project_id": id, "name": r.get::<_, String>(1)?, "created_by": r.get::<_, String>(2)?,
                  "created_at": r.get::<_, String>(3)?,
                  "budget": crate::scopes::scope_view(db, "project", &id)?,
                  "revision": r.get::<_, i64>(4)?}),
        )
    },
};

const TURNS: View = View {
    select: "SELECT t.turn_id, t.conversation_id, t.seq, t.message, t.state, t.posted_by, t.posted_at, t.job_id,
                    t.reply, t.detail, t.context, t.started_at, t.finished_at, t.revision,
                    (SELECT k.name FROM credentials k WHERE +org_id = :org_id AND k.id = t.posted_by), j.run_id, c.cancel_id,
                    COALESCE(x.state, c.state), x.outcome, t.origin, j.terminal_row, j.error
             FROM (SELECT * FROM turns WHERE +org_id = :org_id) t LEFT JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id = t.job_id LEFT JOIN (SELECT * FROM cancels WHERE +org_id = :org_id) c ON c.job_id = t.job_id
               LEFT JOIN (SELECT * FROM controls WHERE +org_id = :org_id) x ON x.control_id = c.control_id",
    key: "t.turn_id",
    order: "t.seq",
    shape: |db, r| {
        let cancel = r.get::<_, Option<String>>(16)?.map(|c| {
            json!({"cancel_id": c, "state": r.get::<_, Option<String>>(17).ok().flatten(),
                   "outcome": r.get::<_, Option<String>>(18).ok().flatten()})
        });
        // Wake turns can carry structured detail; other turns keep the error text.
        let origin = r.get::<_, String>(19)?;
        let detail = match (origin.as_str(), r.get::<_, Option<String>>(9)?) {
            ("wake" | "browser_data", Some(d)) => serde_json::from_str(&d).unwrap_or(Value::String(d)),
            (_, d) => d.map_or(Value::Null, Value::String),
        };
        let waiting = turn_waiting(db, r.get_ref(1)?.as_str()?, r.get_ref(4)?.as_str()?, r.get(7)?)?;
        let stored_message: String = r.get(3)?;
        let peer = crate::peer_messages::attribution(&origin, &stored_message);
        let message = peer.as_ref().and_then(|peer| peer["text"].as_str()).unwrap_or(&stored_message);
        let mut turn = json!({"turn_id": r.get::<_, String>(0)?, "conversation_id": r.get::<_, String>(1)?,
                  "seq": r.get::<_, i64>(2)?, "message": message, "state": r.get::<_, String>(4)?,
                  "posted_by": r.get::<_, String>(5)?, "posted_by_name": if origin == "browser_data" {
                      Some("Google Tasks".to_owned())
                  } else { r.get::<_, Option<String>>(14)? },
                  "posted_at": r.get::<_, String>(6)?, "job_id": r.get::<_, Option<String>>(7)?,
                  "run_id": r.get::<_, Option<String>>(15)?, "reply": r.get::<_, Option<String>>(8)?,
                  "failure": crate::failure::classify(&r.get::<_, String>(4)?, &parse(r.get(20)?).unwrap_or(Value::Null),
                      r.get::<_, Option<String>>(21)?.as_deref(), detail.as_str().or_else(|| detail["error"].as_str())),
                  "detail": detail, "context": parse(r.get(10)?), "cancel": cancel,
                  "started_at": r.get::<_, Option<String>>(11)?, "finished_at": r.get::<_, Option<String>>(12)?,
                  "revision": r.get::<_, i64>(13)?, "origin": origin, "waiting": waiting});
        if let Some(mut peer) = peer {
            if let Some(fields) = peer.as_object_mut() {
                fields.remove("text");
            }
            turn["peer"] = peer;
        }
        Ok(turn)
    },
};

const SUMMARIES: View = View {
    select: "SELECT s.summary_id, s.conversation_id, s.through_seq, s.based_on, s.redoes, s.job_id, s.state, s.text,
                    s.detail, s.requested_by, s.created_at, s.finished_at, s.revision, j.run_id
             FROM (SELECT rowid AS rowid, * FROM summaries WHERE +org_id = :org_id) s LEFT JOIN (SELECT rowid AS rowid, * FROM jobs WHERE +org_id = :org_id) j ON j.job_id = s.job_id",
    key: "s.summary_id",
    order: "s.rowid",
    shape: |_, r| {
        Ok(json!({"summary_id": r.get::<_, String>(0)?, "conversation_id": r.get::<_, String>(1)?,
                  "through_seq": r.get::<_, i64>(2)?, "based_on": r.get::<_, Option<String>>(3)?,
                  "redoes": r.get::<_, Option<String>>(4)?, "job_id": r.get::<_, String>(5)?,
                  "run_id": r.get::<_, Option<String>>(13)?, "state": r.get::<_, String>(6)?,
                  "text": r.get::<_, Option<String>>(7)?, "detail": r.get::<_, Option<String>>(8)?,
                  "requested_by": r.get::<_, String>(9)?, "created_at": r.get::<_, String>(10)?,
                  "finished_at": r.get::<_, Option<String>>(11)?, "revision": r.get::<_, i64>(12)?}))
    },
};

fn view(kind: Record) -> &'static View {
    match kind {
        Record::Intent => &INTENTS,
        Record::Job => &JOBS,
        Record::Run => &RUNS,
        Record::Decision => &DECISIONS,
        Record::Land => &LANDS,
        Record::Cancel => &CANCELS,
        Record::Node => &NODES,
        Record::Schedule => &SCHEDULES,
        Record::ScheduleChange => &SCHEDULE_CHANGES,
        Record::Conversation => &CONVERSATIONS,
        Record::Turn => &TURNS,
        Record::Summary => &SUMMARIES,
        Record::Project => &PROJECTS,
        Record::BacklogItem => &crate::backlog::VIEW,
        Record::DecisionLog => &crate::decision_log::VIEW,
        Record::ActionApproval => &crate::action_approvals::VIEW,
    }
}

fn project_row(
    db: &ScopedConnection,
    kind: Record,
    view: &View,
    row: &Row,
) -> rusqlite::Result<Value> {
    let mut value = (view.shape)(db, row)?;
    crate::projections::record(db, kind, &mut value)?;
    Ok(value)
}

/// Records of a kind matching `filter` (an SQL condition on `?1`), newest first, at most 500.
pub fn select(
    db: &ScopedConnection,
    kind: Record,
    filter: &str,
    arg: &dyn ToSql,
) -> rusqlite::Result<Vec<Value>> {
    let v = view(kind);
    let sql = format!("{} WHERE {filter} ORDER BY {} LIMIT 500", v.select, v.order);
    let mut stmt = db.prepare(&sql)?;
    stmt.query_map([arg], |r| project_row(db, kind, v, r))?
        .collect()
}

/// The complete conversation thread, including turns covered by summaries.
pub fn thread_rows(db: &ScopedConnection, kind: Record, id: &str) -> rusqlite::Result<Vec<Value>> {
    let (v, alias) = match kind {
        Record::Turn => (&TURNS, "t"),
        Record::Summary => (&SUMMARIES, "s"),
        _ => unreachable!("threads contain turns and summaries"),
    };
    let sql = format!(
        "{} WHERE {alias}.conversation_id = ?1 ORDER BY {}",
        v.select, v.order
    );
    db.prepare(&sql)?
        .query_map([id], |r| project_row(db, kind, v, r))?
        .collect()
}

/// Jobs and runs visible to this client, with the scope applied before the cap.
pub fn list_work(
    db: &ScopedConnection,
    kind: Record,
    node: Option<&str>,
    client: &crate::access::Client,
) -> rusqlite::Result<Vec<Value>> {
    let v = view(kind);
    let mut sql = format!("{} WHERE (?1 IS NULL OR n.name = ?1)", v.select);
    let mut bind = vec![node.map(String::from).into()];
    match kind {
        Record::Job => crate::lead::push_job_scope(client, "j.job_id", &mut sql, &mut bind),
        Record::Run => crate::lead::push_run_scope(client, &mut sql, &mut bind),
        _ => unreachable!("work list requires jobs or runs"),
    }
    let _ = write!(sql, " ORDER BY {} LIMIT {MAX_INTENTS}", v.order);
    db.prepare(&sql)?
        .query_map(bind, |r| project_row(db, kind, v, r))?
        .collect()
}

/// One record by its id.
pub fn one(db: &ScopedConnection, kind: Record, id: &str) -> rusqlite::Result<Option<Value>> {
    let v = view(kind);
    Ok(select(db, kind, &format!("{} = ?1", v.key), &id)?
        .into_iter()
        .next())
}

/// The most records one list returns, and the default `limit`.
pub const MAX_INTENTS: u32 = 500;

/// Intents, optionally filtered by node, lead conversation, and/or project, newest first, at most `limit`
/// (clamped to 1..=[`MAX_INTENTS`]). With `lanes`, only task lanes ([`is_lane_meta`] / [`INTENT_IS_LANE_SQL`]).
pub fn list_intents(
    db: &ScopedConnection,
    node: Option<&str>,
    conversation: Option<&str>,
    lanes: bool,
    project: Option<&str>,
    limit: u32,
    client: Option<&crate::access::Client>,
) -> rusqlite::Result<Vec<Value>> {
    let v = view(Record::Intent);
    let mut sql = format!(
        "{} WHERE (?1 IS NULL OR i.node = ?1) AND (?2 IS NULL OR i.lead_conversation_id = ?2)
        AND (?3 = 0 OR {}) AND (?4 IS NULL OR i.project_id = ?4)",
        v.select, INTENT_IS_LANE_SQL
    );
    let mut bind = vec![
        node.map(String::from).into(),
        conversation.map(String::from).into(),
        lanes.into(),
        project.map(String::from).into(),
    ];
    if let Some(client) = client {
        crate::lead::push_intent_scope(client, &mut sql, &mut bind);
    }
    let _ = write!(sql, " ORDER BY {} LIMIT ?", v.order);
    bind.push(limit.clamp(1, MAX_INTENTS).into());
    db.prepare(&sql)?
        .query_map(bind, |r| project_row(db, Record::Intent, v, r))?
        .collect()
}

/// Conversations, optionally filtered by node, intent, hosted-lead marker, and/or project, newest first by
/// `created_at`, at most `limit` (clamped to 1..=[`MAX_INTENTS`]).
pub fn list_conversations(
    db: &ScopedConnection,
    node: Option<&str>,
    intent: Option<&str>,
    lead_only: bool,
    project: Option<&str>,
    limit: u32,
    archived: Option<bool>,
) -> rusqlite::Result<Vec<Value>> {
    const FILTER: &str = "(?1 IS NULL OR c.node = ?1) AND (?2 IS NULL OR c.intent_id = ?2)
        AND (?3 = 0 OR c.lead = 1) AND (?4 IS NULL OR c.project_id = ?4)
        AND (?6 IS NULL OR (c.archived_at IS NOT NULL) = ?6)";
    let v = view(Record::Conversation);
    let sql = format!(
        "{} WHERE {FILTER} ORDER BY c.created_at DESC, c.rowid DESC LIMIT ?5",
        v.select
    );
    let mut stmt = db.prepare(&sql)?;
    let limit = limit.clamp(1, MAX_INTENTS);
    stmt.query_map(
        params![node, intent, lead_only, project, limit, archived],
        |r| project_row(db, Record::Conversation, v, r),
    )?
    .collect()
}

/// The jobs of the given intents, optionally only those of one node, newest first, at most 500.
pub fn jobs_of_intents(
    db: &ScopedConnection,
    intent_ids: &[&str],
    node: Option<&str>,
) -> rusqlite::Result<Vec<Value>> {
    if intent_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(intent_ids).unwrap_or_else(|_| "[]".into());
    let v = view(Record::Job);
    let sql = format!(
        "{} WHERE j.intent_id IN (SELECT value FROM json_each(?1)) AND (?2 IS NULL OR n.name = ?2)
         ORDER BY {} LIMIT 500",
        v.select, v.order
    );
    let mut stmt = db.prepare(&sql)?;
    stmt.query_map(params![ids, node], |r| project_row(db, Record::Job, v, r))?
        .collect()
}

/// Records of a kind, optionally only those of one node, newest first.
pub fn list(
    db: &ScopedConnection,
    kind: Record,
    node: Option<&str>,
) -> rusqlite::Result<Vec<Value>> {
    let filter = match kind {
        Record::Intent => "(?1 IS NULL OR i.node = ?1)",
        Record::Job | Record::Run | Record::Node | Record::Schedule | Record::ScheduleChange => {
            "(?1 IS NULL OR n.name = ?1)"
        }
        Record::Conversation => "(?1 IS NULL OR c.node = ?1)",
        Record::Turn => "(?1 IS NULL OR t.conversation_id = ?1)",
        Record::Summary => "(?1 IS NULL OR s.conversation_id = ?1)",
        Record::Decision
        | Record::Land
        | Record::Cancel
        | Record::Project
        | Record::ActionApproval
        | Record::BacklogItem
        | Record::DecisionLog => "(?1 IS NULL OR ?1 IS NOT NULL)",
    };
    select(db, kind, filter, &node)
}

/// An intent with its jobs and their runs: `{intent, jobs, runs}`; `None` when there is no such intent.
pub fn intent_bundle(db: &ScopedConnection, id: &str) -> rusqlite::Result<Option<Value>> {
    let Some(intent) = one(db, Record::Intent, id)? else {
        return Ok(None);
    };
    let jobs = select(db, Record::Job, "j.intent_id = ?1", &id)?;
    let runs = select(
        db,
        Record::Run,
        "r.run_id IN (SELECT run_id FROM jobs WHERE +org_id = :org_id AND intent_id = ?1)",
        &id,
    )?;
    Ok(Some(json!({"intent": intent, "jobs": jobs, "runs": runs})))
}

/// Resolve a finalized or live run to its job, including older terminal reports.
pub fn job_for_run(db: &ScopedConnection, id: &str) -> rusqlite::Result<Option<String>> {
    db.query_row(
        "SELECT COALESCE((SELECT job_id FROM runs WHERE +org_id = :org_id AND run_id = ?1),
                         (SELECT job_id FROM jobs WHERE +org_id = :org_id AND run_id = ?1),
                         (SELECT job_id FROM live_runs WHERE +org_id = :org_id AND run_id = ?1))",
        params![id],
        |row| row.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    type Res = Result<(), Box<dyn std::error::Error>>;

    fn seed_intent(db: &ScopedConnection, id: &str, lead: Option<&str>) -> rusqlite::Result<()> {
        seed_intent_meta(db, id, lead, "{}")
    }

    fn seed_intent_meta(
        db: &ScopedConnection,
        id: &str,
        lead: Option<&str>,
        meta: &str,
    ) -> rusqlite::Result<()> {
        db.execute(
            "INSERT INTO intents (org_id, intent_id, node, spec, task, trials, meta, bundle_sha256,
             created_by, created_at, lead_conversation_id)
             VALUES (:org_id, ?1, 'estate', 's', 't', 1, ?3, 'sha', 'owner', 'now', ?2)",
            params![id, lead, meta],
        )
        .map(|_| ())
    }

    fn seed_job(db: &ScopedConnection, job: &str, intent: &str) -> rusqlite::Result<()> {
        db.execute(
            "INSERT OR IGNORE INTO nodes (org_id, id, name, token_hash, created_at) VALUES (:org_id, 1, 'estate', 'th', 'now')",
            params![],
        )?;
        db.execute(
            "INSERT INTO jobs (org_id, job_id, node_id, spec, task, trials, bundle_sha256, meta, state, created_at, intent_id)
             VALUES (:org_id, ?1, 1, 's', 't', 1, 'sha', '{}', 'offered', 'now', ?2)",
            params![job, intent],
        )
        .map(|_| ())
    }

    fn ids(rows: &[Value], key: &str) -> Vec<String> {
        rows.iter()
            .map(|r| r[key].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn list_intents_limit_caps_newest_first_and_defaults_unchanged() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        for i in 1..=5 {
            seed_intent(&db, &format!("i{i}"), None)?;
        }
        let two = list_intents(&db, None, None, false, None, 2, None)?;
        assert_eq!(ids(&two, "intent_id"), ["i5", "i4"]);
        let all = list_intents(&db, None, None, false, None, MAX_INTENTS, None)?;
        assert_eq!(ids(&all, "intent_id"), ["i5", "i4", "i3", "i2", "i1"]);
        // Out-of-range limits are clamped rather than failing.
        assert_eq!(
            list_intents(&db, None, None, false, None, 0, None)?.len(),
            1
        );
        assert_eq!(
            list_intents(&db, None, None, false, None, 10_000, None)?.len(),
            5
        );
        assert_eq!(
            list_intents(&db, Some("elsewhere"), None, false, None, MAX_INTENTS, None)?.len(),
            0
        );
        Ok(())
    }

    #[test]
    fn list_intents_lanes_keeps_only_task_lanes() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        seed_intent_meta(&db, "mobile", None, r#"{"source":"client-mobile-lane"}"#)?;
        seed_intent_meta(&db, "web", None, r#"{"source":"client-web-lane"}"#)?;
        seed_intent_meta(&db, "mcp", None, r#"{"source":"cs-mcp"}"#)?;
        seed_intent_meta(&db, "laned", None, r#"{"source":"other","lane":"lane/x"}"#)?;
        seed_intent_meta(&db, "lane_false", None, r#"{"lane":false}"#)?;
        seed_intent_meta(&db, "lane_null", None, r#"{"lane":null}"#)?;
        seed_intent_meta(&db, "other", None, r#"{"source":"cs-cli"}"#)?;
        seed_intent_meta(&db, "plain", None, "{}")?;
        seed_intent_meta(&db, "not_object", None, "[1]")?;
        let lanes = list_intents(&db, None, None, true, None, MAX_INTENTS, None)?;
        assert_eq!(
            ids(&lanes, "intent_id"),
            ["lane_false", "laned", "mcp", "web", "mobile"]
        );
        // The limit applies after the lane filter.
        let one = list_intents(&db, None, None, true, None, 1, None)?;
        assert_eq!(ids(&one, "intent_id"), ["lane_false"]);
        let all = list_intents(&db, None, None, false, None, MAX_INTENTS, None)?;
        assert_eq!(all.len(), 9);
        // Predicate and SQL filter agree; derived `lane` matches the predicate.
        let lane_ids: std::collections::HashSet<_> = ids(&lanes, "intent_id").into_iter().collect();
        for intent in &all {
            let id = intent["intent_id"].as_str().unwrap_or_default();
            let predicted = is_lane_meta(&intent["meta"]);
            assert_eq!(
                predicted,
                lane_ids.contains(id),
                "predicate vs SQL for {id}: meta={}",
                intent["meta"]
            );
            assert_eq!(
                intent["lane"].as_bool(),
                Some(predicted),
                "derived lane field for {id}"
            );
        }
        Ok(())
    }

    #[test]
    fn jobs_of_intents_returns_only_listed_intents_jobs() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        for i in ["a", "b", "c"] {
            seed_intent(&db, i, None)?;
        }
        seed_job(&db, "ja1", "a")?;
        seed_job(&db, "jb1", "b")?;
        seed_job(&db, "ja2", "a")?;
        seed_job(&db, "jc1", "c")?;
        let listed = list_intents(&db, None, None, false, None, 2, None)?;
        let listed_ids: Vec<&str> = listed
            .iter()
            .filter_map(|i| i["intent_id"].as_str())
            .collect();
        assert_eq!(listed_ids, ["c", "b"]);
        let jobs = jobs_of_intents(&db, &listed_ids, None)?;
        assert_eq!(ids(&jobs, "job_id"), ["jc1", "jb1"]);
        assert_eq!(
            ids(&jobs_of_intents(&db, &["a"], None)?, "job_id"),
            ["ja2", "ja1"]
        );
        assert_eq!(jobs_of_intents(&db, &["a"], Some("elsewhere"))?.len(), 0);
        assert_eq!(jobs_of_intents(&db, &[], None)?.len(), 0);
        Ok(())
    }

    #[test]
    fn intents_expose_lead_conversation_id_and_filter_lists() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        seed_intent(&db, "lead_lane", Some("conv_a"))?;
        seed_intent(&db, "owner_lane", None)?;
        seed_intent(&db, "other_conv", Some("conv_b"))?;
        let Some(stamped) = one(&db, Record::Intent, "lead_lane")? else {
            panic!("lead_lane intent missing");
        };
        assert_eq!(stamped["lead_conversation_id"].as_str(), Some("conv_a"));
        let Some(owner) = one(&db, Record::Intent, "owner_lane")? else {
            panic!("owner_lane intent missing");
        };
        assert!(owner["lead_conversation_id"].is_null());
        let filtered = list_intents(&db, None, Some("conv_a"), false, None, MAX_INTENTS, None)?;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["intent_id"].as_str(), Some("lead_lane"));
        Ok(())
    }

    fn seed_intent_project(db: &ScopedConnection, id: &str, project: &str) -> rusqlite::Result<()> {
        db.execute(
            "INSERT INTO intents (org_id, intent_id, node, spec, task, trials, meta, bundle_sha256,
             created_by, created_at, project_id)
             VALUES (:org_id, ?1, 'estate', 's', 't', 1, '{}', 'sha', 'owner', ?2, ?3)",
            params![id, format!("at_{id}"), project],
        )
        .map(|_| ())
    }

    fn seed_conversation(
        db: &ScopedConnection,
        id: &str,
        at: &str,
        lead: bool,
        project: Option<&str>,
    ) -> rusqlite::Result<()> {
        db.execute(
            "INSERT INTO conversations (org_id, conversation_id, node, spec, spec_version, model, instructions_tokens,
             tools, created_by, created_at, lead, project_id)
             VALUES (:org_id, ?1, 'estate', 'spec.yaml', 'sha', 'm', 0, 0, 'owner', ?2, ?3, ?4)",
            params![id, at, i64::from(lead), project],
        )
        .map(|_| ())
    }

    #[test]
    fn list_intents_project_and_list_conversations_filters() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        db.execute(
            "INSERT INTO projects (org_id, project_id, name, created_by, created_at) VALUES (:org_id, 'p1', 'one', 'o', 'now'), (:org_id, 'p2', 'two', 'o', 'now')",
            params![],
        )?;
        seed_intent_project(&db, "i_a", "p1")?;
        seed_intent_project(&db, "i_b", "p2")?;
        seed_intent_project(&db, "i_c", "p1")?;
        let p1 = list_intents(&db, None, None, false, Some("p1"), MAX_INTENTS, None)?;
        assert_eq!(ids(&p1, "intent_id"), ["i_c", "i_a"]);
        seed_conversation(&db, "c1", "2020-01-01T00:00:00Z", false, Some("p1"))?;
        seed_conversation(&db, "c2", "2021-01-01T00:00:00Z", true, None)?;
        seed_conversation(&db, "c3", "2022-01-01T00:00:00Z", false, Some("p2"))?;
        let all = list_conversations(&db, None, None, false, None, MAX_INTENTS, Some(false))?;
        assert_eq!(ids(&all, "conversation_id"), ["c3", "c2", "c1"]);
        let two = list_conversations(&db, None, None, false, None, 2, Some(false))?;
        assert_eq!(ids(&two, "conversation_id"), ["c3", "c2"]);
        let leads = list_conversations(&db, None, None, true, None, MAX_INTENTS, Some(false))?;
        assert_eq!(ids(&leads, "conversation_id"), ["c2"]);
        let proj =
            list_conversations(&db, None, None, false, Some("p1"), MAX_INTENTS, Some(false))?;
        assert_eq!(ids(&proj, "conversation_id"), ["c1"]);
        Ok(())
    }

    fn wait(state: &str, since_age: Option<i64>, seen_age: Option<i64>) -> Wait<'_> {
        Wait {
            state,
            node: "estate",
            since: "2026-11-24T08:00:00Z",
            since_age,
            node_last_seen: seen_age.map(|_| "2026-11-24T07:59:00Z"),
            seen_age,
            ..Wait::default()
        }
    }

    #[test]
    fn waiting_names_an_offered_job_unpulled_past_the_grace() {
        assert!(
            waiting(&wait("offered", Some(10), Some(1)), 30, 300).is_null(),
            "within the grace"
        );
        let late = waiting(&wait("offered", Some(31), Some(1)), 30, 300);
        assert_eq!(
            late,
            json!({"reason": "node_not_pulled", "node": "estate", "since": "2026-11-24T08:00:00Z"})
        );
        let stale = waiting(&wait("offered", Some(400), Some(400)), 30, 300);
        assert_eq!(stale["node_last_seen"], "2026-11-24T07:59:00Z");
        let never = waiting(&wait("offered", Some(400), None), 30, 300);
        assert!(
            never
                .as_object()
                .is_some_and(|o| o["node_last_seen"].is_null())
        );
        for state in ["accepted", "done", "error"] {
            assert!(
                waiting(&wait(state, Some(400), None), 30, 300).is_null(),
                "{state}"
            );
        }
    }

    #[test]
    fn waiting_names_a_held_job_and_its_reason() {
        let held = Wait {
            hold_reason: Some("executor_unavailable"),
            hold_detail: Some("estate cannot run claude"),
            ..wait("waiting", Some(1), Some(1))
        };
        assert_eq!(
            waiting(&held, 30, 300),
            json!({"reason": "held", "detail": "executor_unavailable",
                   "message": "estate cannot run claude", "node": "estate"})
        );
    }

    #[test]
    fn turn_conversation_and_job_show_waiting_until_the_node_pulls() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        seed_conversation(&db, "c1", "now", false, None)?;
        seed_job(&db, "j1", "i1")?;
        db.execute(
            "UPDATE jobs SET offered_at = '2026-01-01T00:00:00Z' WHERE job_id = 'j1'",
            params![],
        )?;
        db.execute(
            "UPDATE nodes SET last_seen_at = ?1 WHERE id = 1",
            params![crate::util::now_utc()],
        )?;
        db.execute(
            "INSERT INTO turns (org_id, turn_id, conversation_id, seq, message, state, posted_by, posted_at, job_id)
             VALUES (:org_id, 't1', 'c1', 1, 'hi', 'running', 'owner', 'now', 'j1')",
            params![],
        )?;
        let want =
            json!({"reason": "node_not_pulled", "node": "estate", "since": "2026-01-01T00:00:00Z"});
        let turn = one(&db, Record::Turn, "t1")?.unwrap_or_default();
        assert_eq!(turn["state"], "running");
        assert_eq!(turn["waiting"], want);
        let conversation = one(&db, Record::Conversation, "c1")?.unwrap_or_default();
        assert_eq!(conversation["active_turn_id"], "t1");
        assert_eq!(conversation["waiting"], want);
        assert_eq!(
            one(&db, Record::Job, "j1")?.unwrap_or_default()["waiting"],
            want
        );
        assert_eq!(
            thread_rows(&db, Record::Turn, "c1")?[0]["waiting"],
            want,
            "the thread view"
        );

        db.execute(
            "UPDATE jobs SET state = 'accepted' WHERE job_id = 'j1'",
            params![],
        )?;
        assert!(one(&db, Record::Turn, "t1")?.unwrap_or_default()["waiting"].is_null());
        assert!(one(&db, Record::Conversation, "c1")?.unwrap_or_default()["waiting"].is_null());
        assert!(one(&db, Record::Job, "j1")?.unwrap_or_default()["waiting"].is_null());

        db.fixture_batch(
            "UPDATE jobs SET state = 'waiting' WHERE job_id = 'j1';
             INSERT INTO job_budgets (org_id, job_id, state, trials, reason, detail, created_at)
             VALUES (:org_id, 'j1', 'waiting', 1, 'budget_exhausted', 'the project budget is used up', 'now')",
        )?;
        let turn = one(&db, Record::Turn, "t1")?.unwrap_or_default();
        assert_eq!(turn["waiting"]["reason"], "held");
        assert_eq!(turn["waiting"]["detail"], "budget_exhausted");
        Ok(())
    }
    #[test]
    fn summarising_turn_shows_the_summary_jobs_waiting() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        seed_conversation(&db, "c1", "now", false, None)?;
        seed_job(&db, "j1", "i1")?;
        db.fixture_batch(
            "INSERT INTO turns (org_id, turn_id, conversation_id, seq, message, state, posted_by, posted_at)
             VALUES (:org_id, 't1', 'c1', 2, 'hi', 'summarising', 'owner', 'now');
             INSERT INTO summaries (org_id, summary_id, conversation_id, through_seq, job_id, state, requested_by, created_at)
             VALUES (:org_id, 's1', 'c1', 1, 'j1', 'running', 'owner', 'now');
             UPDATE jobs SET state = 'waiting' WHERE job_id = 'j1';
             INSERT INTO job_budgets (org_id, job_id, state, trials, reason, detail, created_at)
             VALUES (:org_id, 'j1', 'waiting', 1, 'budget_exhausted', 'the project budget is used up', 'now')",
        )?;
        let turn = one(&db, Record::Turn, "t1")?.unwrap_or_default();
        assert!(turn["job_id"].is_null());
        assert_eq!(
            turn["waiting"],
            json!({"reason": "held", "detail": "budget_exhausted",
                   "message": "the project budget is used up", "node": "estate"})
        );
        db.execute(
            "UPDATE jobs SET state = 'accepted' WHERE job_id = 'j1'",
            params![],
        )?;
        assert!(one(&db, Record::Turn, "t1")?.unwrap_or_default()["waiting"].is_null());
        Ok(())
    }
}
