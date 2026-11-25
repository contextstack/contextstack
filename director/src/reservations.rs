//! Project and conversation budgets applied to jobs (the rule is in `budget.rs`).
//!
//! Every job made since step 7 has a row in `job_budgets`, created in the transaction that creates the job:
//!
//! | `state` | Meaning |
//! | --- | --- |
//! | `free` | no budget applied when it was created; its charge still counts once it settles |
//! | `held` | `reserved_micros` is set aside and written into its spec; it is offered, or waits (`jobs.state = 'waiting'`, `reason = 'executor_unavailable'`) until its node can run it |
//! | `waiting` | nothing remained; the job is created and held (`jobs.state = 'waiting'`), with `reason` and `detail` |
//! | `settled` | `charged_micros` is spent, the rest released: its runs reported, or it closed before they did |
//!
//! A held reservation whose run never reports closes on the job's end, a cancel outcome, a stale node or a timeout,
//! counted in full (`presumed`); any later report recomputes the charge whole (`settle`), so nothing is released
//! twice (CLIENT-API.md §12.3).
//!
//! A budget's room is `usd − settled charges this period − held reservations`, all read from these rows, so a
//! restart loses nothing: reservations are rows, and each change of them commits with the change that caused it.

use crate::budget::{self, Micros, Place, Room, Scope};
use crate::jobs::{BundleFile, bundle_sha};
use crate::scopes::room;
use crate::store::{Record, Store, touch};
use crate::turns::{Res, db_err};
use crate::util::now_utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// The budgets a job falls under: its project's and its conversation's.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct JobScope {
    /// The project of its intent or conversation.
    pub project: Option<String>,
    /// The conversation whose turn or summary it carries.
    pub conversation: Option<String>,
}

impl JobScope {
    fn scopes(&self) -> Vec<Scope> {
        let project = self.project.iter().map(|id| Scope {
            kind: "project",
            id: id.clone(),
        });
        let conversation = self.conversation.iter().map(|id| Scope {
            kind: "conversation",
            id: id.clone(),
        });
        project.chain(conversation).collect()
    }
}

fn rooms(db: &Connection, js: &JobScope) -> Res<Vec<Room>> {
    let mut out = Vec::new();
    for scope in js.scopes() {
        if let Some(r) = room(db, &scope).map_err(db_err)? {
            out.push(r);
        }
    }
    Ok(out)
}

/// The bundle with its spec's `budget.usd` set to `per_run` (`stamp.rs` writes the intent the same way).
fn rewrite(store: &Store, files: &mut [BundleFile], spec: &str, per_run: Micros) -> Res<()> {
    crate::stamp::rewrite(store, files, spec, |bytes| {
        budget::with_usd(bytes, per_run.max(1))
    })
}

/// A new job's place under its budgets, decided before it is recorded.
#[derive(Debug)]
pub struct Placed {
    scope: JobScope,
    ceiling: Option<Micros>,
    trials: u32,
    place: Place,
}

impl Placed {
    /// Whether the job waits instead of being offered.
    pub fn waits(&self) -> bool {
        matches!(self.place, Place::Wait(_))
    }
}

/// Decide a new job's place, rewriting its spec in `files` when an amount is reserved. A job under a budget needs
/// a spec that is a YAML mapping.
pub fn place_new(
    store: &Store,
    db: &Connection,
    scope: &JobScope,
    spec: &str,
    trials: u32,
    files: &mut [BundleFile],
) -> Res<Placed> {
    let rooms = rooms(db, scope)?;
    let bytes = files
        .iter()
        .find(|f| f.path == spec)
        .and_then(|f| store.read_object(&f.sha256))
        .unwrap_or_default();
    let ceiling = if rooms.is_empty() {
        budget::ceiling(&bytes, trials).ok().flatten()
    } else {
        budget::ceiling(&bytes, trials).map_err(|e| format!("a job under a budget: {e}"))?
    };
    let place = budget::place(ceiling, &rooms);
    if let Place::Reserve(amount) = place {
        rewrite(store, files, spec, budget::per_run(amount, trials))?;
    }
    Ok(Placed {
        scope: scope.clone(),
        ceiling,
        trials,
        place,
    })
}

/// Record a new job's place, in the transaction that recorded the job.
pub fn record(db: &Connection, job_id: &str, p: &Placed) -> Res<()> {
    let now = now_utc();
    let (state, reserved, reason, detail) = match &p.place {
        Place::Free => ("free", 0, None, None),
        Place::Reserve(m) => ("held", *m, None, None),
        Place::Wait(w) => ("waiting", 0, Some(w.reason), Some(w.detail.clone())),
    };
    db.execute(
        "INSERT INTO job_budgets (job_id, project_id, conversation_id, state, ceiling_micros, trials, reserved_micros,
                                  reason, detail, created_at, reserved_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![job_id, p.scope.project, p.scope.conversation, state, p.ceiling, p.trials, reserved, reason,
                detail, now, (state == "held").then_some(&now)],
    )
    .map_err(db_err)?;
    touch_scopes(db, &p.scope)
}

fn touch_scopes(db: &Connection, scope: &JobScope) -> Res<()> {
    if let Some(p) = &scope.project {
        touch(db, Record::Project, p).map_err(db_err)?;
    }
    if let Some(c) = &scope.conversation {
        touch(db, Record::Conversation, c).map_err(db_err)?;
    }
    Ok(())
}

fn scope_of(db: &Connection, job_id: &str) -> Res<JobScope> {
    db.query_row(
        "SELECT project_id, conversation_id FROM job_budgets WHERE job_id = ?1",
        params![job_id],
        |r| {
            Ok(JobScope {
                project: r.get(0)?,
                conversation: r.get(1)?,
            })
        },
    )
    .map_err(db_err)
}

/// The runs linked to a job: the run its end names, runs that sent live events for it, and runs whose report
/// names it (`run_finalized` with `job_id`, one per trial).
const LINKED: &str = "SELECT run_id FROM jobs WHERE job_id = ?1 AND run_id IS NOT NULL
                      UNION SELECT run_id FROM live_runs WHERE job_id = ?1
                      UNION SELECT run_id FROM runs WHERE job_id = ?1";

/// What is known of a job's spending.
#[derive(Debug)]
struct Known {
    /// Its node reported it over (`done` or `error`).
    over: bool,
    /// The run its end names has not reported yet.
    pending: bool,
    /// Linked runs that reported.
    runs: usize,
    /// Their `charged_usd`, added up.
    charged: Micros,
    /// A pay-as-you-go run among them reported no charge.
    unknown: bool,
}

fn known(db: &Connection, job_id: &str) -> Res<Known> {
    let (state, named): (String, Option<String>) = db
        .query_row(
            "SELECT state, run_id FROM jobs WHERE job_id = ?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(db_err)?;
    let mut stmt = db
        .prepare_cached(&format!(
            "SELECT usage FROM runs WHERE run_id IN ({LINKED})"
        ))
        .map_err(db_err)?;
    let usages: Vec<String> = stmt
        .query_map(params![job_id], |r| r.get(0))
        .map_err(db_err)?
        .collect::<Result<_, _>>()
        .map_err(db_err)?;
    let pending = match named {
        Some(run) => db
            .query_row("SELECT 1 FROM runs WHERE run_id = ?1", params![run], |_| {
                Ok(())
            })
            .optional()
            .map_err(db_err)?
            .is_none(),
        None => false,
    };
    let mut k = Known {
        over: matches!(state.as_str(), "done" | "error"),
        pending,
        runs: usages.len(),
        charged: 0,
        unknown: false,
    };
    for u in usages {
        let u: Value = serde_json::from_str(&u).unwrap_or(Value::Null);
        match budget::amount(&u["charged_usd"]) {
            Some(m) => k.charged += m,
            None => k.unknown |= u["source"]["class"] == "api",
        }
    }
    Ok(k)
}

/// What a job counts as charged, `(micros, basis)`; `None` while nothing is known yet and nothing closes it.
///
/// Once the job is over and the run its end names has reported, the charge is its linked runs' `charged_usd`
/// (`reported`), or the whole reservation when a pay-as-you-go run reported none (`unreported`), or $0 with no run
/// (`no_run`). Before that, a reservation that is closed (`presumed`) counts at the whole reservation, or what the
/// runs reported if that is more: what it spent is not known, and a run is held to its reservation.
fn charge(k: &Known, reserved: Micros, closing: bool) -> Option<(Micros, &'static str)> {
    if k.over && !k.pending {
        return Some(if k.runs == 0 {
            (0, "no_run")
        } else if k.unknown {
            (k.charged.max(reserved), "unreported")
        } else {
            (k.charged, "reported")
        });
    }
    closing.then_some((k.charged.max(reserved), "presumed"))
}

/// Settle or correct a job's reservation. `by` names the signal closing it before the job is fully known
/// (`terminal`, `cancel`, `node_stale`, `timeout`); with `None` it settles only once the job is over and its
/// named run has reported. A settled job is corrected whenever more becomes known (a late or another trial's
/// report): its charge is recomputed whole, never added to, so nothing is released twice. `Ok(true)` when the row
/// changed.
pub fn settle(db: &Connection, job_id: &str, by: Option<&str>) -> Res<bool> {
    let found: Option<(String, Micros, Option<Micros>, Option<String>)> = db
        .query_row(
            "SELECT state, reserved_micros, charged_micros, charge_basis FROM job_budgets WHERE job_id = ?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(db_err)?;
    let Some((state, reserved, charged, basis)) = found else {
        return Ok(false);
    };
    let k = known(db, job_id)?;
    let now = now_utc();
    let rows = match state.as_str() {
        // A free job holds nothing, so only its end settles it.
        "free" | "held" => {
            let Some((c, b)) = charge(&k, reserved, by.is_some() && state == "held") else {
                return Ok(false);
            };
            let by = if b == "presumed" { by } else { Some("ended") };
            db.execute(
                "UPDATE job_budgets SET state = 'settled', charged_micros = ?1, charge_basis = ?2, settled_at = ?3,
                                        settled_by = ?4, reason = NULL, detail = NULL
                 WHERE job_id = ?5",
                params![c, b, now, by, job_id],
            )
            .map_err(db_err)?
        }
        "settled" if basis.as_deref() != Some("never_ran") => {
            let presumed = basis.as_deref() == Some("presumed");
            let Some((c, b)) = charge(&k, reserved, presumed) else {
                return Ok(false);
            };
            if Some(c) == charged && Some(b) == basis.as_deref() {
                return Ok(false);
            }
            // The charge stays in the period it was first counted in (`settled_at`).
            db.execute(
                "UPDATE job_budgets SET charged_micros = ?1, charge_basis = ?2, corrected_at = ?3 WHERE job_id = ?4",
                params![c, b, now, job_id],
            )
            .map_err(db_err)?
        }
        _ => return Ok(false),
    };
    if rows > 0 {
        touch(db, Record::Job, job_id).map_err(db_err)?;
        touch_scopes(db, &scope_of(db, job_id)?)?;
    }
    Ok(rows > 0)
}

/// A waiting job that is cancelled ends at once: it never reached its node and costs nothing.
pub fn cancel_waiting(db: &Connection, job_id: &str) -> Res<bool> {
    let waiting = db
        .execute(
            "UPDATE jobs SET state = 'error', finished_at = ?1, terminal_row = ?2, error = ?3
             WHERE job_id = ?4 AND state = 'waiting'",
            params![
                now_utc(),
                json!({"state": "cancelled", "detail": "cancelled while it waited for budget"})
                    .to_string(),
                "cancelled while it waited for budget",
                job_id
            ],
        )
        .map_err(db_err)?
        > 0;
    if waiting {
        db.execute(
            "UPDATE job_budgets SET state = 'settled', charged_micros = 0, charge_basis = 'never_ran',
                                    settled_at = ?1, reason = NULL, detail = NULL WHERE job_id = ?2",
            params![now_utc(), job_id],
        )
        .map_err(db_err)?;
        touch(db, Record::Job, job_id).map_err(db_err)?;
        touch_scopes(db, &scope_of(db, job_id)?)?;
    }
    Ok(waiting)
}

/// A waiting job: `(job_id, scope, ceiling, trials, spec, reason, detail)`.
type Held = (
    String,
    JobScope,
    Option<Micros>,
    u32,
    String,
    Option<String>,
    Option<String>,
);

/// Offer waiting jobs, oldest first, while their budgets have room. Returns how many were offered.
pub fn release(store: &Store, db: &Connection) -> Res<usize> {
    let mut stmt = db
        .prepare_cached(
            "SELECT b.job_id, b.project_id, b.conversation_id, b.ceiling_micros, b.trials, j.spec, b.reason, b.detail
             FROM job_budgets b JOIN jobs j ON j.job_id = b.job_id
             WHERE b.state = 'waiting' AND j.state = 'waiting' ORDER BY j.rowid",
        )
        .map_err(db_err)?;
    let held: Vec<Held> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                JobScope {
                    project: r.get(1)?,
                    conversation: r.get(2)?,
                },
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .map_err(db_err)?
        .collect::<Result<_, _>>()
        .map_err(db_err)?;
    let mut offered = 0;
    for (job_id, scope, ceiling, trials, spec, reason, detail) in held {
        // One row that cannot be offered (its spec cannot carry a reservation, say) is logged and left waiting;
        // it never stops the others, the sweep, a budget change or the Director starting.
        let one = || -> Res<bool> {
            match budget::place(ceiling, &rooms(db, &scope)?) {
                Place::Wait(w) => {
                    if reason.as_deref() != Some(w.reason) || detail.as_ref() != Some(&w.detail) {
                        db.execute(
                            "UPDATE job_budgets SET reason = ?1, detail = ?2 WHERE job_id = ?3",
                            params![w.reason, w.detail, job_id],
                        )
                        .map_err(db_err)?;
                        touch(db, Record::Job, &job_id).map_err(db_err)?;
                    }
                    Ok(false)
                }
                place => {
                    let reserved = match place {
                        Place::Reserve(m) => m,
                        _ => 0,
                    };
                    let sent = offer_job(store, db, &job_id, &spec, trials, reserved)?;
                    touch_scopes(db, &scope)?;
                    Ok(sent)
                }
            }
        };
        match savepoint(db, "release_job", one) {
            Ok(sent) => offered += usize::from(sent),
            Err(e) => crate::logf!("job {job_id}: left waiting, it could not be offered: {e}"),
        }
    }
    Ok(offered)
}

/// Run `f` under a named savepoint, rolling back only its own changes when it fails.
pub(crate) fn savepoint<T>(db: &Connection, name: &str, f: impl FnOnce() -> Res<T>) -> Res<T> {
    db.execute_batch(&format!("SAVEPOINT {name}"))
        .map_err(db_err)?;
    let done = f();
    let end = if done.is_ok() {
        format!("RELEASE {name}")
    } else {
        format!("ROLLBACK TO {name}; RELEASE {name}")
    };
    db.execute_batch(&end).map_err(db_err)?;
    done
}

/// A waiting job's budget allows it: its spec carries the amount reserved (none when its budget was removed
/// meanwhile). It is offered (`Ok(true)`) unless its node cannot run it; then it keeps the reservation, which
/// still counts against its budgets, and waits for executor fit (`capabilities::offer_waiting`).
pub fn offer_job(
    store: &Store,
    db: &Connection,
    job_id: &str,
    spec: &str,
    trials: u32,
    reserved: Micros,
) -> Res<bool> {
    let mut files = crate::jobs::bundle_of_job(db, job_id).map_err(db_err)?;
    if reserved > 0 {
        rewrite(store, &mut files, spec, budget::per_run(reserved, trials))?;
        let file = files.iter().find(|f| f.path == spec).ok_or("no spec")?;
        db.execute(
            "UPDATE job_files SET sha256 = ?1, bytes = ?2 WHERE job_id = ?3 AND path = ?4",
            params![
                file.sha256,
                i64::try_from(file.bytes).unwrap_or(i64::MAX),
                job_id,
                spec
            ],
        )
        .map_err(db_err)?;
    }
    let (node_id, node_name): (i64, String) = db
        .query_row(
            "SELECT n.id, n.name FROM jobs j JOIN nodes n ON n.id = j.node_id WHERE j.job_id = ?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(db_err)?;
    let now = now_utc();
    let state = if reserved > 0 { "held" } else { "free" };
    let wait = crate::capabilities::capability_wait(store, db, node_id, &files, spec, &node_name);
    db.execute(
        "UPDATE job_budgets SET state = ?1, reserved_micros = ?2, reserved_at = ?3, reason = ?4, detail = ?5
         WHERE job_id = ?6",
        params![state, reserved, now, wait.as_ref().map(|w| w.reason), wait.as_ref().map(|w| &w.detail), job_id],
    )
    .map_err(db_err)?;
    if wait.is_none() {
        db.execute(
            "UPDATE jobs SET state = 'offered', bundle_sha256 = ?1, offered_at = ?3 WHERE job_id = ?2 AND state = 'waiting'",
            params![bundle_sha(&files), job_id, now],
        )
        .map_err(db_err)?;
    } else {
        db.execute(
            "UPDATE jobs SET bundle_sha256 = ?1 WHERE job_id = ?2",
            params![bundle_sha(&files), job_id],
        )
        .map_err(db_err)?;
    }
    touch(db, Record::Job, job_id).map_err(db_err)?;
    if wait.is_some() {
        crate::logf!(
            "job {job_id}: reserved {reserved} micro-dollars; waiting for {node_name} to run it"
        );
    } else {
        crate::logf!("job {job_id}: offered after waiting (reserved {reserved} micro-dollars)");
    }
    Ok(wait.is_none())
}

/// Run `f` under a savepoint: a failure is logged and rolled back on its own, never failing the caller.
fn guarded(db: &Connection, what: &str, f: impl FnOnce() -> Res<()>) {
    if db.execute_batch("SAVEPOINT budgets").is_err() {
        return;
    }
    let done = f();
    if let Err(e) = &done {
        crate::logf!("budgets after {what}: {e}");
    }
    let end = if done.is_ok() {
        "RELEASE budgets"
    } else {
        "ROLLBACK TO budgets; RELEASE budgets"
    };
    let _ = db.execute_batch(end);
}

/// After an acked envelope: the job it ends, or every job its run is linked to, settles or is corrected. A job
/// that ended with its named run's report still on its way is closed at once (`terminal`).
pub fn after_envelope(db: &Connection, job_terminal: bool, subject: &str) {
    guarded(db, subject, || {
        if job_terminal {
            settle(db, subject, Some("terminal"))?;
            return Ok(());
        }
        let mut stmt = db
            .prepare_cached(
                "SELECT job_id FROM runs WHERE run_id = ?1 AND job_id IS NOT NULL
                 UNION SELECT job_id FROM jobs WHERE run_id = ?1 UNION SELECT job_id FROM live_runs WHERE run_id = ?1",
            )
            .map_err(db_err)?;
        let jobs: Vec<String> = stmt
            .query_map(params![subject], |r| r.get(0))
            .map_err(db_err)?
            .collect::<Result<_, _>>()
            .map_err(db_err)?;
        for job in jobs {
            settle(db, &job, None)?;
        }
        Ok(())
    });
}

/// A cancel of a job was applied on its node (`stopped` or `already_finished`): its reservation closes. A job with
/// no live run that the node stops (still `offered`, or `accepted` with no run / live events / report) is over: it
/// ends `error` with terminal `cancelled` and costs $0. An accepted job with run evidence waits for `job_terminal`.
pub fn after_cancel(db: &Connection, job_id: &str, outcome: &str) {
    guarded(db, job_id, || {
        // A node that finds nothing running answers `already_finished`; with no run evidence it ends the same way.
        if outcome == "stopped" || outcome == "already_finished" {
            let never = finish_cancel_without_run(
                db,
                job_id,
                "offered",
                "cancelled before its node started it",
            )? || finish_cancel_without_run(
                db,
                job_id,
                "accepted",
                "cancelled: its node had nothing to stop",
            )?;
            if never {
                touch(db, Record::Job, job_id).map_err(db_err)?;
            }
        }
        settle(db, job_id, Some("cancel"))?;
        Ok(())
    });
}

/// The terminal path after a cancel is applied: end a no-run job when that applies, settle its reservation, offer
/// waiting work. Shared by a newly applied control, a second `POST …/cancel`, and the startup orphan sweep.
pub fn on_cancel_applied(store: &Store, db: &Connection, job_id: &str, outcome: &str) {
    after_cancel(db, job_id, outcome);
    crate::settle::after_envelope(store, db, true, job_id);
    release_logged(store, db, job_id);
}

/// An applied cancel (`stopped`) on a still-open job with no run evidence: the outcome string, else `None`.
fn orphan_applied_cancel_outcome(db: &Connection, job_id: &str) -> Res<Option<String>> {
    db.query_row(
        "SELECT k.outcome FROM jobs j
         JOIN cancels c ON c.job_id = j.job_id
         JOIN controls k ON k.control_id = c.control_id
         WHERE j.job_id = ?1 AND j.state IN ('offered', 'accepted')
           AND k.state = 'applied' AND k.outcome IN ('stopped', 'already_finished')
           AND j.run_id IS NULL
           AND NOT EXISTS (SELECT 1 FROM live_runs WHERE job_id = j.job_id)
           AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id = j.job_id)",
        params![job_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(db_err)
}

/// Finish a non-terminal job whose cancel is already applied and that has no run evidence (the live gap when a
/// cancel was applied before the Director ended such jobs). Same terminal path as a newly applied cancel.
/// Returns whether the job was ended.
pub fn finish_orphan_applied_cancel(store: &Store, db: &Connection, job_id: &str) -> Res<bool> {
    let Some(outcome) = orphan_applied_cancel_outcome(db, job_id)? else {
        return Ok(false);
    };
    on_cancel_applied(store, db, job_id, &outcome);
    let ended: bool = db
        .query_row(
            "SELECT state = 'error' FROM jobs WHERE job_id = ?1",
            params![job_id],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    Ok(ended)
}

/// At start: finish every non-terminal job with an applied cancel and no run evidence. Returns how many ended.
pub fn sweep_orphan_applied_cancels(store: &Store) -> Res<usize> {
    let mut db = store.db();
    let mut stmt = db
        .prepare(
            "SELECT j.job_id FROM jobs j
             JOIN cancels c ON c.job_id = j.job_id
             JOIN controls k ON k.control_id = c.control_id
             WHERE j.state IN ('offered', 'accepted')
               AND k.state = 'applied' AND k.outcome IN ('stopped', 'already_finished')
               AND j.run_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM live_runs WHERE job_id = j.job_id)
               AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id = j.job_id)",
        )
        .map_err(db_err)?;
    let jobs: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .map_err(db_err)?
        .collect::<Result<_, _>>()
        .map_err(db_err)?;
    drop(stmt);
    if jobs.is_empty() {
        return Ok(0);
    }
    let tx = crate::store::write_tx(&mut db).map_err(db_err)?;
    let mut finished = 0;
    for job_id in &jobs {
        if finish_orphan_applied_cancel(store, &tx, job_id)? {
            crate::logf!("job {job_id}: finished orphan applied cancel");
            finished += 1;
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(finished)
}

/// End a job that a cancel stopped before any run existed (`offered`, or `accepted` with no named run,
/// live events, or reported run). An accepted job that already has run evidence waits for `job_terminal`.
fn finish_cancel_without_run(
    db: &Connection,
    job_id: &str,
    state: &str,
    detail: &str,
) -> Res<bool> {
    Ok(db
        .execute(
            "UPDATE jobs SET state = 'error', finished_at = ?1, terminal_row = ?2, error = ?3
             WHERE job_id = ?4 AND state = ?5 AND run_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM live_runs WHERE job_id = ?4)
               AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id = ?4)",
            params![
                now_utc(),
                json!({"state": "cancelled", "detail": detail}).to_string(),
                detail,
                job_id,
                state
            ],
        )
        .map_err(db_err)?
        > 0)
}

/// The bounds after which a held reservation closes though its job has not ended (`serve
/// --reservation-stale-after`, `--reservation-timeout`).
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    /// Its node has not been heard from for this many seconds (and the reservation is at least this old).
    pub node_stale: i64,
    /// The reservation is this many seconds old: the last resort.
    pub timeout: i64,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            node_stale: 3600,
            timeout: 86_400,
        }
    }
}

/// Close held reservations past their bounds, then offer what fits. Returns how many closed.
pub fn sweep(store: &Store, bounds: Bounds) -> Res<usize> {
    let mut db = store.db();
    let stale = crate::util::utc_in(-bounds.node_stale);
    let timeout = crate::util::utc_in(-bounds.timeout);
    let mut stmt = db
        .prepare_cached(
            "SELECT b.job_id, CASE WHEN COALESCE(n.last_seen_at, '') <= ?1 AND b.reserved_at <= ?1 THEN 'node_stale'
                                   ELSE 'timeout' END
             FROM job_budgets b JOIN jobs j ON j.job_id = b.job_id JOIN nodes n ON n.id = j.node_id
             WHERE b.state = 'held' AND j.state <> 'waiting'
               AND ((COALESCE(n.last_seen_at, '') <= ?1 AND b.reserved_at <= ?1)
                                         OR b.reserved_at <= ?2)",
        )
        .map_err(db_err)?;
    let due: Vec<(String, String)> = stmt
        .query_map(params![stale, timeout], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(db_err)?
        .collect::<Result<_, _>>()
        .map_err(db_err)?;
    drop(stmt);
    if due.is_empty() {
        return Ok(0);
    }
    let tx = crate::store::write_tx(&mut db).map_err(db_err)?;
    let mut closed = 0;
    for (job, why) in &due {
        if settle(&tx, job, Some(why))? {
            crate::logf!("job {job}: reservation closed ({why})");
            closed += 1;
        }
    }
    release(store, &tx)?;
    tx.commit().map_err(db_err)?;
    Ok(closed)
}

/// Offer what now fits, logging rather than failing.
pub fn release_logged(store: &Store, db: &Connection, what: &str) {
    guarded(db, what, || release(store, db).map(|_| ()));
}

/// At start: settle every finished job not yet settled (closing one whose run never reported), then offer what
/// fits. Returns `(settled, offered)`.
pub fn reconcile(store: &Store) -> Res<(usize, usize)> {
    let mut db = store.db();
    let tx = crate::store::write_tx(&mut db).map_err(db_err)?;
    let mut stmt = tx
        .prepare(
            "SELECT b.job_id FROM job_budgets b JOIN jobs j ON j.job_id = b.job_id
             WHERE b.state IN ('free', 'held') AND j.state IN ('done', 'error')",
        )
        .map_err(db_err)?;
    let open: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .map_err(db_err)?
        .collect::<Result<_, _>>()
        .map_err(db_err)?;
    drop(stmt);
    let mut settled = 0;
    for job in open {
        settled += usize::from(settle(&tx, &job, Some("terminal"))?);
    }
    let offered = release(store, &tx)?;
    tx.commit().map_err(db_err)?;
    Ok((settled, offered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Period;
    use crate::jobs::Plan;
    use crate::scopes::{clear, job_view, scope_view, set};
    use crate::util::sha256_hex;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn setup(dir: &std::path::Path) -> Result<Store, Box<dyn std::error::Error>> {
        let store = Store::open(&dir.join("data"))?;
        crate::access::register_node(&store, "node-a")?;
        store.db().execute(
            "INSERT INTO projects (project_id, name, created_by, created_at) VALUES ('p1', 'p', 'cred', ?1)",
            params![now_utc()],
        )?;
        Ok(store)
    }

    fn files(store: &Store, usd: f64) -> Result<Vec<BundleFile>, Box<dyn std::error::Error>> {
        let spec = format!("id: x\nmodel: m\nbudget: {{turns: 1, usd: {usd}}}\n");
        let mut out = Vec::new();
        for (path, data) in [
            ("spec.yaml", spec.as_bytes()),
            ("tasks/t/goal.md", b"go\n".as_slice()),
        ] {
            store.put_object(data)?;
            out.push(BundleFile {
                path: path.into(),
                sha256: sha256_hex(data),
                bytes: data.len() as u64,
                mode: 0o644,
            });
        }
        Ok(out)
    }

    fn job(store: &Store, usd: f64) -> Result<String, Box<dyn std::error::Error>> {
        let files = files(store, usd)?;
        let db = store.db();
        let plan = Plan::new(
            &db,
            "node-a",
            "spec.yaml",
            "tasks/t",
            1,
            None,
            serde_json::Map::new(),
            files,
        )?
        .under(JobScope {
            project: Some("p1".into()),
            conversation: None,
        });
        Ok(plan.insert(store, &db, None)?)
    }

    fn state(store: &Store, job: &str) -> Result<(String, String), rusqlite::Error> {
        store.db().query_row(
            "SELECT j.state, b.state FROM jobs j JOIN job_budgets b ON b.job_id = j.job_id WHERE j.job_id = ?1",
            params![job],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    fn finish(store: &Store, job: &str, charged: f64) -> R {
        let db = store.db();
        let usage = json!({"input_tokens": 1, "output_tokens": 1, "est_usd": 9, "est_basis": "rate",
                           "charged_usd": charged, "source": {"class": "api", "model": "m"}});
        db.execute(
            "INSERT INTO runs (run_id, node_id, usage, received_at) VALUES (?1, 1, ?2, ?3)",
            params![format!("run-{job}"), usage.to_string(), now_utc()],
        )?;
        db.execute(
            "UPDATE jobs SET state = 'done', run_id = ?1 WHERE job_id = ?2",
            params![format!("run-{job}"), job],
        )?;
        settle(&db, job, None)?;
        release(store, &db)?;
        Ok(())
    }

    #[test]
    fn reserve_wait_settle_release() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            5_000_000,
            Period::Total,
            "cred",
        )?;
        let a = job(&store, 5.0)?;
        let b = job(&store, 5.0)?;
        assert_eq!(state(&store, &a)?, ("offered".into(), "held".into()));
        assert_eq!(state(&store, &b)?, ("waiting".into(), "waiting".into()));
        let view = job_view(&store.db(), &b)?;
        assert_eq!(view["reason"], "budget_reserved");
        let offered_at = |j: &str| -> rusqlite::Result<Option<String>> {
            store.db().query_row(
                "SELECT offered_at FROM jobs WHERE job_id = ?1",
                params![j],
                |r| r.get(0),
            )
        };
        assert!(offered_at(&a)?.is_some(), "offered at creation");
        assert_eq!(offered_at(&b)?, None, "a waiting job has not been offered");
        let held = crate::records::job_waiting(&store.db(), &b)?;
        assert_eq!(
            (held["reason"].as_str(), held["detail"].as_str()),
            (Some("held"), Some("budget_reserved"))
        );
        // The node sees only the offered job, and its spec carries the reservation.
        assert_eq!(crate::jobs::for_node(&store.db(), 1)?.len(), 1);
        let p = scope_view(&store.db(), "project", "p1")?;
        assert_eq!(
            (p["state"].as_str(), p["reserved_usd"].as_f64()),
            (Some("reserved"), Some(5.0))
        );
        finish(&store, &a, 1.5)?;
        assert_eq!(state(&store, &a)?.1, "settled");
        assert_eq!(state(&store, &b)?, ("offered".into(), "held".into()));
        assert!(
            offered_at(&b)?.is_some(),
            "offered when its budget allowed it"
        );
        assert!(
            crate::records::job_waiting(&store.db(), &b)?.is_null(),
            "within the grace"
        );
        let vb = job_view(&store.db(), &b)?;
        assert_eq!(
            vb["reserved_usd"].as_f64(),
            Some(3.5),
            "what the first released"
        );
        let sha: String = store.db().query_row(
            "SELECT sha256 FROM job_files WHERE job_id = ?1 AND path = 'spec.yaml'",
            params![b],
            |r| r.get(0),
        )?;
        let spec = store.read_object(&sha).ok_or("no spec")?;
        assert_eq!(
            budget::ceiling(&spec, 1)?,
            Some(3_500_000),
            "the spec carries it"
        );
        finish(&store, &b, 3.5)?;
        let p = scope_view(&store.db(), "project", "p1")?;
        assert_eq!(
            (p["state"].as_str(), p["shown"].as_str()),
            (Some("exhausted"), Some("budget used up"))
        );
        let c = job(&store, 1.0)?;
        assert_eq!(job_view(&store.db(), &c)?["reason"], "budget_exhausted");
        clear(&store.db(), "project", "p1")?;
        assert_eq!(scope_view(&store.db(), "project", "p1")?["state"], "none");
        release(&store, &store.db())?;
        assert_eq!(
            state(&store, &c)?,
            ("offered".into(), "free".into()),
            "no budget: offered as written"
        );
        Ok(())
    }

    #[test]
    fn a_reservation_survives_a_reopen_and_unreported_charges_count_in_full() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            2_000_000,
            Period::Month,
            "cred",
        )?;
        let a = job(&store, 0.0)?;
        assert_eq!(
            job_view(&store.db(), &a)?["reserved_usd"].as_f64(),
            Some(2.0),
            "no ceiling: the room"
        );
        drop(store);
        let store = Store::open(&tmp.path().join("data"))?;
        assert_eq!(reconcile(&store)?, (0, 0));
        assert_eq!(state(&store, &a)?.1, "held", "still held after a reopen");
        let db = store.db();
        db.execute(
            "INSERT INTO runs (run_id, node_id, usage, received_at) VALUES ('r', 1, ?1, ?2)",
            params![
                json!({"charged_usd": null, "source": {"class": "api", "model": "m"}}).to_string(),
                now_utc()
            ],
        )?;
        db.execute(
            "UPDATE jobs SET state = 'done', run_id = 'r' WHERE job_id = ?1",
            params![a],
        )?;
        drop(db);
        assert_eq!(
            reconcile(&store)?,
            (1, 0),
            "a finished job settles at start"
        );
        let v = job_view(&store.db(), &a)?;
        assert_eq!(
            (v["charged_usd"].as_f64(), v["charge_basis"].as_str()),
            (Some(2.0), Some("unreported"))
        );
        Ok(())
    }

    #[test]
    fn a_cancelled_waiting_job_ends_at_once() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            1_000_000,
            Period::Total,
            "cred",
        )?;
        let _a = job(&store, 1.0)?;
        let b = job(&store, 1.0)?;
        assert!(cancel_waiting(&store.db(), &b)?);
        assert_eq!(state(&store, &b)?, ("error".into(), "settled".into()));
        assert!(!cancel_waiting(&store.db(), &b)?, "once");
        Ok(())
    }

    fn run_report(store: &Store, run: &str, job: Option<&str>, charged: f64) -> R {
        let usage = json!({"charged_usd": charged, "source": {"class": "api", "model": "m"}});
        store.db().execute(
            "INSERT INTO runs (run_id, node_id, usage, received_at, job_id) VALUES (?1, 1, ?2, ?3, ?4)",
            params![run, usage.to_string(), now_utc(), job],
        )?;
        Ok(())
    }

    fn view(store: &Store, job: &str) -> Result<Value, rusqlite::Error> {
        job_view(&store.db(), job)
    }

    #[test]
    fn a_terminal_whose_run_never_reports_closes_and_a_late_report_corrects_it() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            4_000_000,
            Period::Total,
            "cred",
        )?;
        let a = job(&store, 4.0)?;
        let b = job(&store, 4.0)?;
        // The job ends naming run r1, whose report has not arrived.
        store.db().execute(
            "UPDATE jobs SET state = 'done', run_id = 'r1' WHERE job_id = ?1",
            params![a],
        )?;
        after_envelope(&store.db(), true, &a);
        let v = view(&store, &a)?;
        assert_eq!(
            (
                v["state"].as_str(),
                v["charge_basis"].as_str(),
                v["settled_by"].as_str()
            ),
            (Some("settled"), Some("presumed"), Some("terminal"))
        );
        assert_eq!(
            v["charged_usd"].as_f64(),
            Some(4.0),
            "unknown spend counts in full"
        );
        release(&store, &store.db())?;
        assert_eq!(
            state(&store, &b)?.0,
            "waiting",
            "nothing is freed on a guess"
        );
        // The late report: $1 charged. The rest is released once, and the waiting job gets it.
        run_report(&store, "r1", None, 1.0)?;
        after_envelope(&store.db(), false, "r1");
        release(&store, &store.db())?;
        let v = view(&store, &a)?;
        assert_eq!(
            (
                v["charge_basis"].as_str(),
                v["charged_usd"].as_f64(),
                v["released_usd"].as_f64()
            ),
            (Some("reported"), Some(1.0), Some(3.0))
        );
        assert!(v["corrected_at"].is_string());
        assert_eq!(view(&store, &b)?["reserved_usd"].as_f64(), Some(3.0));
        // Again: nothing changes, nothing is released twice.
        assert!(!settle(&store.db(), &a, Some("terminal"))?);
        assert!(!settle(&store.db(), &a, None)?);
        let p = scope_view(&store.db(), "project", "p1")?;
        assert_eq!(
            (p["spent_usd"].as_f64(), p["reserved_usd"].as_f64()),
            (Some(1.0), Some(3.0))
        );
        Ok(())
    }

    #[test]
    fn every_trials_run_counts_when_it_names_its_job() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            5_000_000,
            Period::Total,
            "cred",
        )?;
        let a = job(&store, 2.0)?;
        run_report(&store, "t1", Some(&a), 0.5)?;
        run_report(&store, "t2", Some(&a), 0.25)?;
        store.db().execute(
            "UPDATE jobs SET state = 'done', run_id = 't2' WHERE job_id = ?1",
            params![a],
        )?;
        assert!(settle(&store.db(), &a, Some("terminal"))?);
        let v = view(&store, &a)?;
        assert_eq!(
            (v["charged_usd"].as_f64(), v["charge_basis"].as_str()),
            (Some(0.75), Some("reported"))
        );
        Ok(())
    }

    #[test]
    fn stale_nodes_and_timeouts_close_held_reservations() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            3_000_000,
            Period::Total,
            "cred",
        )?;
        let a = job(&store, 1.0)?;
        let bounds = Bounds {
            node_stale: 3600,
            timeout: 7200,
        };
        assert_eq!(sweep(&store, bounds)?, 0, "fresh");
        // The node was last heard two hours ago, and the reservation is as old.
        let old = crate::util::utc_in(-7000);
        store
            .db()
            .execute("UPDATE nodes SET last_seen_at = ?1", params![old])?;
        store
            .db()
            .execute("UPDATE job_budgets SET reserved_at = ?1", params![old])?;
        assert_eq!(sweep(&store, bounds)?, 1);
        let v = view(&store, &a)?;
        assert_eq!(
            (
                v["charge_basis"].as_str(),
                v["settled_by"].as_str(),
                v["charged_usd"].as_f64()
            ),
            (Some("presumed"), Some("node_stale"), Some(1.0))
        );
        // A node heard from just now, but a reservation past the timeout.
        let b = job(&store, 1.0)?;
        store
            .db()
            .execute("UPDATE nodes SET last_seen_at = ?1", params![now_utc()])?;
        store.db().execute(
            "UPDATE job_budgets SET reserved_at = ?1 WHERE job_id = ?2",
            params![crate::util::utc_in(-7300), b],
        )?;
        assert_eq!(sweep(&store, bounds)?, 1);
        assert_eq!(view(&store, &b)?["settled_by"], "timeout");
        assert_eq!(sweep(&store, bounds)?, 0, "once");
        Ok(())
    }

    #[test]
    fn a_cancel_before_the_node_started_costs_nothing() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            2_000_000,
            Period::Total,
            "cred",
        )?;
        let a = job(&store, 2.0)?;
        after_cancel(&store.db(), &a, "stopped");
        assert_eq!(state(&store, &a)?, ("error".into(), "settled".into()));
        let v = view(&store, &a)?;
        assert_eq!(
            (v["charged_usd"].as_f64(), v["charge_basis"].as_str()),
            (Some(0.0), Some("no_run"))
        );
        // Accepted with no run, then stopped: ends at once (nothing to stop), $0.
        let b = job(&store, 2.0)?;
        store.db().execute(
            "UPDATE jobs SET state = 'accepted', accepted_at = ?1 WHERE job_id = ?2",
            params![now_utc(), b],
        )?;
        after_cancel(&store.db(), &b, "stopped");
        assert_eq!(state(&store, &b)?, ("error".into(), "settled".into()));
        let v = view(&store, &b)?;
        assert_eq!(
            (v["charged_usd"].as_f64(), v["charge_basis"].as_str()),
            (Some(0.0), Some("no_run"))
        );
        assert_eq!(
            v["settled_by"].as_str(),
            Some("ended"),
            "known end, not a presumed cancel hold"
        );
        let (term, err): (String, String) = store.db().query_row(
            "SELECT terminal_row, error FROM jobs WHERE job_id = ?1",
            params![b],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(
            serde_json::from_str::<Value>(&term)?["state"].as_str(),
            Some("cancelled")
        );
        assert_eq!(err, "cancelled: its node had nothing to stop");
        // Accepted with live run evidence: wait for the node's end; reservation presumed until then.
        let c = job(&store, 2.0)?;
        store.db().execute(
            "UPDATE jobs SET state = 'accepted', accepted_at = ?1 WHERE job_id = ?2",
            params![now_utc(), c],
        )?;
        store.db().execute(
            "INSERT INTO live_runs (run_id, node_id, job_id, confirmed, first_at, last_at)
             VALUES ('live-run', 1, ?1, 1, ?2, ?2)",
            params![c, now_utc()],
        )?;
        after_cancel(&store.db(), &c, "stopped");
        assert_eq!(state(&store, &c)?.0, "accepted");
        let v = view(&store, &c)?;
        assert_eq!(
            (v["charge_basis"].as_str(), v["settled_by"].as_str()),
            (Some("presumed"), Some("cancel"))
        );
        Ok(())
    }

    /// Plant an applied cancel on a still-open job (the pre-fix orphan shape).
    fn plant_orphan_applied_cancel(store: &Store, job_id: &str) -> R {
        let now = now_utc();
        store.db().execute(
            "INSERT INTO controls (control_id, node_id, kind, body, state, outcome, detail, issued_at, settled_at)
             VALUES (?1, 1, 'cancel', ?2, 'applied', 'stopped', 'stopped', ?3, ?3)",
            params![
                format!("ctl-{job_id}"),
                json!({"job_id": job_id}).to_string(),
                now
            ],
        )?;
        store.db().execute(
            "INSERT INTO cancels (cancel_id, job_id, state, requested_by, requested_at, control_id)
             VALUES (?1, ?2, 'requested', 'cred', ?3, ?4)",
            params![
                format!("can-{job_id}"),
                job_id,
                now,
                format!("ctl-{job_id}")
            ],
        )?;
        Ok(())
    }

    #[test]
    fn orphan_applied_cancel_finishes_on_sweep_and_leaves_run_evidence() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            4_000_000,
            Period::Total,
            "cred",
        )?;
        // Accepted, no run, cancel already applied: startup sweep ends it.
        let a = job(&store, 2.0)?;
        store.db().execute(
            "UPDATE jobs SET state = 'accepted', accepted_at = ?1 WHERE job_id = ?2",
            params![now_utc(), a],
        )?;
        plant_orphan_applied_cancel(&store, &a)?;
        assert_eq!(state(&store, &a)?.0, "accepted");
        assert_eq!(sweep_orphan_applied_cancels(&store)?, 1);
        assert_eq!(state(&store, &a)?, ("error".into(), "settled".into()));
        let v = view(&store, &a)?;
        assert_eq!(
            (v["charged_usd"].as_f64(), v["charge_basis"].as_str()),
            (Some(0.0), Some("no_run"))
        );
        assert_eq!(sweep_orphan_applied_cancels(&store)?, 0, "once");

        // The node answered `already_finished` (it had nothing running) and never sent job_terminal.
        let c = job(&store, 2.0)?;
        store.db().execute(
            "UPDATE jobs SET state = 'accepted', accepted_at = ?1 WHERE job_id = ?2",
            params![now_utc(), c],
        )?;
        plant_orphan_applied_cancel(&store, &c)?;
        store.db().execute(
            "UPDATE controls SET outcome = 'already_finished', detail = 'already_finished' WHERE control_id = ?1",
            params![format!("ctl-{c}")],
        )?;
        assert_eq!(sweep_orphan_applied_cancels(&store)?, 1);
        assert_eq!(state(&store, &c)?, ("error".into(), "settled".into()));

        // Accepted with live run evidence: sweep leaves it for job_terminal.
        let b = job(&store, 2.0)?;
        store.db().execute(
            "UPDATE jobs SET state = 'accepted', accepted_at = ?1 WHERE job_id = ?2",
            params![now_utc(), b],
        )?;
        plant_orphan_applied_cancel(&store, &b)?;
        store.db().execute(
            "INSERT INTO live_runs (run_id, node_id, job_id, confirmed, first_at, last_at)
             VALUES ('live-orphan', 1, ?1, 1, ?2, ?2)",
            params![b, now_utc()],
        )?;
        assert_eq!(sweep_orphan_applied_cancels(&store)?, 0);
        assert_eq!(state(&store, &b)?.0, "accepted");
        assert!(!finish_orphan_applied_cancel(&store, &store.db(), &b)?);
        assert_eq!(state(&store, &b)?.0, "accepted");
        Ok(())
    }

    #[test]
    fn orphan_applied_cancel_finishes_when_asked_again() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            2_000_000,
            Period::Total,
            "cred",
        )?;
        let a = job(&store, 2.0)?;
        store.db().execute(
            "UPDATE jobs SET state = 'accepted', accepted_at = ?1 WHERE job_id = ?2",
            params![now_utc(), a],
        )?;
        plant_orphan_applied_cancel(&store, &a)?;
        assert!(finish_orphan_applied_cancel(&store, &store.db(), &a)?);
        assert_eq!(state(&store, &a)?, ("error".into(), "settled".into()));
        let err: String = store.db().query_row(
            "SELECT error FROM jobs WHERE job_id = ?1",
            params![a],
            |r| r.get(0),
        )?;
        assert_eq!(err, "cancelled: its node had nothing to stop");
        assert!(!finish_orphan_applied_cancel(&store, &store.db(), &a)?);
        Ok(())
    }

    fn node_harness(store: &Store, harness: bool) -> R {
        let status = json!({"agent_capabilities": {"templates": [], "harness": harness}});
        store.db().execute(
            "UPDATE nodes SET status = ?1 WHERE id = 1",
            params![status.to_string()],
        )?;
        Ok(())
    }

    fn reason(store: &Store, job: &str) -> Result<Option<String>, rusqlite::Error> {
        store.db().query_row(
            "SELECT reason FROM job_budgets WHERE job_id = ?1",
            params![job],
            |r| r.get(0),
        )
    }

    #[test]
    fn a_capability_held_job_keeps_its_reservation_and_release_leaves_it() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            5_000_000,
            Period::Total,
            "cred",
        )?;
        node_harness(&store, false)?;
        let a = job(&store, 5.0)?;
        assert_eq!(state(&store, &a)?.0, "waiting");
        assert_eq!(reason(&store, &a)?.as_deref(), Some("executor_unavailable"));
        // B2: what it holds still counts, so another job waits for budget.
        let p = scope_view(&store.db(), "project", "p1")?;
        assert_eq!(p["reserved_usd"].as_f64(), Some(5.0), "still reserved");
        let b = job(&store, 1.0)?;
        assert_eq!(state(&store, &b)?, ("waiting".into(), "waiting".into()));
        // B1: release neither offers it to a node that cannot run it nor overwrites why it waits.
        release(&store, &store.db())?;
        assert_eq!(state(&store, &a)?.0, "waiting");
        assert_eq!(reason(&store, &a)?.as_deref(), Some("executor_unavailable"));
        // The sweep does not presume a charge for a job that never reached its node.
        store.db().execute(
            "UPDATE job_budgets SET reserved_at = '2000-01-01T00:00:00Z'",
            [],
        )?;
        assert_eq!(sweep(&store, Bounds::default())?, 0);
        // Once the node can run it, it is offered with its reservation; the other still waits for budget.
        node_harness(&store, true)?;
        assert_eq!(
            crate::capabilities::offer_waiting(&store, &mut store.db(), 1, "node-a")?,
            1
        );
        assert_eq!(state(&store, &a)?, ("offered".into(), "held".into()));
        assert_eq!(
            job_view(&store.db(), &a)?["reserved_usd"].as_f64(),
            Some(5.0)
        );
        assert_eq!(state(&store, &b)?, ("waiting".into(), "waiting".into()));
        Ok(())
    }

    #[test]
    fn a_budget_waiting_job_released_to_a_node_that_cannot_run_it_waits_for_capability() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            1_000_000,
            Period::Total,
            "cred",
        )?;
        let a = job(&store, 1.0)?;
        let b = job(&store, 1.0)?;
        assert_eq!(state(&store, &b)?, ("waiting".into(), "waiting".into()));
        node_harness(&store, false)?;
        finish(&store, &a, 0.0)?;
        assert_eq!(
            state(&store, &b)?,
            ("waiting".into(), "held".into()),
            "reserved, not offered"
        );
        assert_eq!(reason(&store, &b)?.as_deref(), Some("executor_unavailable"));
        node_harness(&store, true)?;
        assert_eq!(
            crate::capabilities::offer_waiting(&store, &mut store.db(), 1, "node-a")?,
            1
        );
        assert_eq!(state(&store, &b)?, ("offered".into(), "held".into()));
        Ok(())
    }

    #[test]
    fn a_waiting_job_whose_spec_cannot_be_edited_is_skipped_not_fatal() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            1_000_000,
            Period::Total,
            "cred",
        )?;
        let a = job(&store, 1.0)?;
        let bad = job(&store, 1.0)?;
        let good = job(&store, 1.0)?;
        // The bad one's spec is not stored, so it cannot carry a reservation.
        store.db().execute(
            "UPDATE job_files SET sha256 = ?1 WHERE job_id = ?2 AND path = 'spec.yaml'",
            params![sha256_hex(b"missing"), bad],
        )?;
        store.db().execute(
            "UPDATE job_budgets SET ceiling_micros = 1 WHERE job_id = ?1",
            params![bad],
        )?;
        store.db().execute(
            "UPDATE job_budgets SET ceiling_micros = 1 WHERE job_id = ?1",
            params![good],
        )?;
        let db = store.db();
        db.execute(
            "UPDATE jobs SET state = 'done' WHERE job_id = ?1",
            params![a],
        )?;
        drop(db);
        // At start the bad row is skipped and logged; the rest goes on.
        let (settled, offered) = reconcile(&store)?;
        assert_eq!((settled, offered), (1, 1));
        assert_eq!(state(&store, &bad)?, ("waiting".into(), "waiting".into()));
        assert_eq!(state(&store, &good)?.0, "offered");
        assert_eq!(release(&store, &store.db())?, 0);
        assert!(sweep(&store, Bounds::default()).is_ok());
        Ok(())
    }

    #[test]
    fn offer_waiting_offers_each_job_on_its_own_and_reports_failures() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        set(
            &store.db(),
            "project",
            "p1",
            5_000_000,
            Period::Total,
            "cred",
        )?;
        node_harness(&store, false)?;
        let bad = job(&store, 1.0)?;
        let good = job(&store, 1.0)?;
        store.db().execute(
            "UPDATE job_files SET sha256 = ?1 WHERE job_id = ?2 AND path = 'spec.yaml'",
            params![sha256_hex(b"missing"), bad],
        )?;
        node_harness(&store, true)?;
        let done = crate::capabilities::offer_waiting(&store, &mut store.db(), 1, "node-a");
        assert!(done.is_err(), "the failure is surfaced: {done:?}");
        assert_eq!(state(&store, &good)?, ("offered".into(), "held".into()));
        assert_eq!(
            state(&store, &bad)?,
            ("waiting".into(), "held".into()),
            "untouched"
        );
        assert_eq!(
            reason(&store, &bad)?.as_deref(),
            Some("executor_unavailable")
        );
        Ok(())
    }
}
