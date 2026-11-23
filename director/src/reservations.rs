//! Project and conversation budgets applied to jobs (the rule is in `budget.rs`).
//!
//! Every job made since step 7 has a row in `job_budgets`, created in the transaction that creates the job:
//!
//! | `state` | Meaning |
//! | --- | --- |
//! | `free` | no budget applied when it was created; its charge still counts once it settles |
//! | `held` | `reserved_micros` is set aside and written into its spec; it is offered |
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
#[derive(Debug, Clone, Default)]
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
            }
            place => {
                let reserved = match place {
                    Place::Reserve(m) => m,
                    _ => 0,
                };
                offer_job(store, db, &job_id, &spec, trials, reserved)?;
                touch_scopes(db, &scope)?;
                offered += 1;
            }
        }
    }
    Ok(offered)
}

/// A waiting job is offered: its spec carries the amount reserved (none when its budget was removed meanwhile).
pub fn offer_job(
    store: &Store,
    db: &Connection,
    job_id: &str,
    spec: &str,
    trials: u32,
    reserved: Micros,
) -> Res<()> {
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
    let now = now_utc();
    db.execute(
        "UPDATE jobs SET state = 'offered', bundle_sha256 = ?1 WHERE job_id = ?2 AND state = 'waiting'",
        params![bundle_sha(&files), job_id],
    )
    .map_err(db_err)?;
    db.execute(
        "UPDATE job_budgets SET state = ?1, reserved_micros = ?2, reserved_at = ?3, reason = NULL, detail = NULL
         WHERE job_id = ?4",
        params![if reserved > 0 { "held" } else { "free" }, reserved, now, job_id],
    )
    .map_err(db_err)?;
    touch(db, Record::Job, job_id).map_err(db_err)?;
    crate::logf!(
        "job {job_id}: offered after waiting for budget (reserved {reserved} micro-dollars)"
    );
    Ok(())
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

/// A cancel of a job was applied on its node (`stopped` or `already_finished`): its reservation closes. A job the
/// node never accepted and says it will never start is over: it ends `error` and costs $0.
pub fn after_cancel(db: &Connection, job_id: &str, outcome: &str) {
    guarded(db, job_id, || {
        if outcome == "stopped" {
            let never = db
                .execute(
                    "UPDATE jobs SET state = 'error', finished_at = ?1, terminal_row = ?2, error = ?3
                     WHERE job_id = ?4 AND state = 'offered' AND accepted_at IS NULL",
                    params![
                        now_utc(),
                        json!({"state": "cancelled", "detail": "cancelled before its node started it"})
                            .to_string(),
                        "cancelled before its node started it",
                        job_id
                    ],
                )
                .map_err(db_err)?
                > 0;
            if never {
                touch(db, Record::Job, job_id).map_err(db_err)?;
            }
        }
        settle(db, job_id, Some("cancel"))?;
        Ok(())
    });
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
             WHERE b.state = 'held' AND ((COALESCE(n.last_seen_at, '') <= ?1 AND b.reserved_at <= ?1)
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
        // Accepted, then stopped: the spend is unknown until the node's end and report arrive.
        let b = job(&store, 2.0)?;
        store.db().execute(
            "UPDATE jobs SET state = 'accepted', accepted_at = ?1 WHERE job_id = ?2",
            params![now_utc(), b],
        )?;
        after_cancel(&store.db(), &b, "stopped");
        let v = view(&store, &b)?;
        assert_eq!(
            (v["charge_basis"].as_str(), v["settled_by"].as_str()),
            (Some("presumed"), Some("cancel"))
        );
        Ok(())
    }
}
