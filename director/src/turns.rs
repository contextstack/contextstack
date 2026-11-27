//! Conversation turns: one active turn per conversation and a queue behind it, stop and resume,
//! summaries made by ordinary jobs, and replies taken from finished runs.
//!
//! A turn moves `queued` → (`summarising` while a summary it needs is made) → `running` (its job is offered) →
//! `done`, `error` or `stopped`. Its context is assembled when it becomes eligible, so it includes the reply
//! before it. Every step runs inside the caller's transaction, under the database lock, so two devices posting at
//! once still queue one behind the other.

use crate::decision_log::{self, TurnLog};
use crate::jobs::{BundleFile, Plan};
use crate::store::ScopedConnection;
use crate::store::{Record, Store, touch};
use crate::thread::{self, Choice, Past, Summary};
use crate::util::{self, now_utc};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};

pub(crate) type Res<T> = Result<T, String>;

#[allow(
    clippy::needless_pass_by_value,
    reason = "it is the map_err of every query"
)]
pub(crate) fn db_err(e: rusqlite::Error) -> String {
    format!("database: {e}")
}

/// A conversation as the turns need it.
struct Conv {
    id: String,
    node: String,
    spec: String,
    spec_version: String,
    model: String,
    instructions: i64,
    tools: i64,
    held: bool,
    revision: i64,
    project: Option<String>,
    lead: bool,
}

fn load(db: &ScopedConnection, id: &str) -> Res<Option<Conv>> {
    db.query_row(
        "SELECT conversation_id, node, spec, spec_version, model, instructions_tokens, tools, held, revision,
                project_id, lead
         FROM conversations WHERE +org_id = :org_id AND conversation_id = ?1",
        params![id],
        |r| {
            Ok(Conv {
                id: r.get(0)?,
                node: r.get(1)?,
                spec: r.get(2)?,
                spec_version: r.get(3)?,
                model: r.get(4)?,
                instructions: r.get(5)?,
                tools: r.get(6)?,
                held: r.get(7)?,
                revision: r.get(8)?,
                project: r.get(9)?,
                lead: r.get(10)?,
            })
        },
    )
    .optional()
    .map_err(db_err)
}

/// The turns of a conversation that are over, before `before`, in order.
fn past(db: &ScopedConnection, conv: &str, before: i64) -> Res<Vec<Past>> {
    let mut stmt = db
        .prepare_cached(
            "SELECT seq, message, reply, state, origin FROM turns WHERE +org_id = :org_id AND conversation_id = ?1 AND seq < ?2
             AND state IN ('done', 'error', 'stopped') ORDER BY seq",
        )
        .map_err(db_err)?;
    stmt.query_map(params![conv, before], |r| {
        let origin = r.get_ref(4)?.as_str()?;
        let message: String = r.get(1)?;
        Ok(Past {
            seq: r.get(0)?,
            source: if origin == "browser_data" {
                "untrusted Google Tasks data"
            } else if crate::peer_messages::attribution(origin, &message).is_some() {
                "untrusted peer conversation"
            } else {
                "the person"
            },
            message: crate::peer_messages::prompt(origin, message),
            reply: r.get(2)?,
            state: r.get(3)?,
        })
    })
    .map_err(db_err)?
    .collect::<Result<_, _>>()
    .map_err(db_err)
}

/// The summary a turn numbered `before` may use: done, covering the most turns, the newest of those.
fn best_summary(db: &ScopedConnection, conv: &str, before: i64) -> Res<Option<Summary>> {
    db.query_row(
        "SELECT summary_id, through_seq, text FROM summaries
         WHERE +org_id = :org_id AND conversation_id = ?1 AND state = 'done' AND through_seq < ?2
         ORDER BY through_seq DESC, rowid DESC LIMIT 1",
        params![conv, before],
        |r| {
            Ok(Summary {
                id: r.get(0)?,
                through: r.get(1)?,
                text: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        },
    )
    .optional()
    .map_err(db_err)
}

fn summary_by_id(db: &ScopedConnection, id: &str) -> Res<Option<Summary>> {
    db.query_row(
        "SELECT summary_id, through_seq, text FROM summaries WHERE +org_id = :org_id AND summary_id = ?1 AND state = 'done'",
        params![id],
        |r| {
            Ok(Summary {
                id: r.get(0)?,
                through: r.get(1)?,
                text: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        },
    )
    .optional()
    .map_err(db_err)
}

/// Offer a job made of the conversation's spec bundle plus the generated context, goal and check.
fn make_job(
    store: &Store,
    db: &ScopedConnection,
    conv: &Conv,
    context: &str,
    goal: &str,
    log: Option<&TurnLog>,
) -> Res<String> {
    let mut stmt = db
        .prepare_cached(
            "SELECT path, sha256, bytes, mode FROM conversation_files WHERE +org_id = :org_id AND conversation_id = ?1 ORDER BY path",
        )
        .map_err(db_err)?;
    let mut files: Vec<BundleFile> = stmt
        .query_map(params![conv.id], |r| {
            Ok(BundleFile {
                path: r.get(0)?,
                sha256: r.get(1)?,
                bytes: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                mode: r.get(3)?,
            })
        })
        .map_err(db_err)?
        .collect::<Result<_, _>>()
        .map_err(db_err)?;
    if conv.lead {
        files.retain(|file| file.path != decision_log::LESSONS_PATH);
    }
    let lessons = log.map_or("# Lessons\n", |log| log.lessons.as_str());
    for (path, data, mode) in [
        (thread::CONTEXT_PATH, context.as_bytes(), 0o644),
        (thread::GOAL_PATH, goal.as_bytes(), 0o644),
        (thread::CHECK_PATH, thread::CHECK, 0o755),
    ]
    .into_iter()
    .chain(log.map(|log| (decision_log::TURN_PATH, log.markdown.as_bytes(), 0o644)))
    .chain(
        conv.lead
            .then_some((decision_log::LESSONS_PATH, lessons.as_bytes(), 0o644)),
    ) {
        store.put_object(data)?;
        files.push(BundleFile {
            path: path.into(),
            sha256: util::sha256_hex(data),
            bytes: data.len() as u64,
            mode,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let plan = Plan::new(
        db,
        &conv.node,
        &conv.spec,
        thread::TASK,
        1,
        None,
        Map::new(),
        files,
    )?
    .under(crate::reservations::JobScope {
        project: conv.project.clone(),
        conversation: Some(conv.id.clone()),
        schedule: None,
    });
    plan.insert(store, db, None)
}

/// Make a summary of turns 1 to `through` with an ordinary job. Returns its id.
pub fn start_summary(
    store: &Store,
    db: &ScopedConnection,
    conv_id: &str,
    through: i64,
    based_on: Option<&str>,
    redoes: Option<&str>,
    requested_by: &str,
) -> Res<String> {
    let conv = load(db, conv_id)?.ok_or("no such conversation")?;
    let base = match based_on {
        Some(id) => summary_by_id(db, id)?,
        None => None,
    };
    let from = base.as_ref().map_or(0, |b| b.through);
    let turns: Vec<Past> = past(db, conv_id, through + 1)?
        .into_iter()
        .filter(|t| t.seq > from)
        .collect();
    let context = thread::render(base.as_ref(), &turns);
    let job = make_job(
        store,
        db,
        &conv,
        &context,
        &thread::summary_goal(through),
        None,
    )?;
    let id = format!("sum_{}", util::ulid()?.to_lowercase());
    db.execute(
        "INSERT INTO summaries (org_id, summary_id, conversation_id, through_seq, based_on, redoes, job_id, state,
                                requested_by, created_at)
         VALUES (:org_id, ?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, ?8)",
        params![
            id,
            conv_id,
            through,
            base.as_ref().map(|b| b.id.clone()),
            redoes,
            job,
            requested_by,
            now_utc()
        ],
    )
    .map_err(db_err)?;
    touch(db, Record::Summary, &id).map_err(db_err)?;
    touch(db, Record::Conversation, conv_id).map_err(db_err)?;
    crate::logf!("conversation {conv_id}: summary {id} of turns 1-{through} is job {job}");
    Ok(id)
}

/// Start the Director's summary of turns 1 to `through`. A summary whose job cannot be offered is rolled back on
/// its own and recorded as failed (`Ok(false)`): the portion is not tried again, and the queue moves on to send
/// the turn whole.
fn try_summary(
    store: &Store,
    db: &ScopedConnection,
    conv_id: &str,
    through: i64,
    based_on: Option<&str>,
) -> Res<bool> {
    let started = crate::reservations::savepoint(db, "summary", || {
        start_summary(store, db, conv_id, through, based_on, None, "director")
    });
    match started {
        Ok(_) => Ok(true),
        Err(e) => {
            fail_summary(db, conv_id, through, based_on, &e)?;
            Ok(false)
        }
    }
}

/// A summary that could not start: recorded as `error`, with no job, so it is not tried again on its own.
fn fail_summary(
    db: &ScopedConnection,
    conv_id: &str,
    through: i64,
    based_on: Option<&str>,
    why: &str,
) -> Res<()> {
    let id = format!("sum_{}", util::ulid()?.to_lowercase());
    let now = now_utc();
    let detail = format!("the summary's job could not be offered: {why}");
    db.execute(
        "INSERT INTO summaries (org_id, summary_id, conversation_id, through_seq, based_on, job_id, state, detail,
                                requested_by, created_at, finished_at)
         VALUES (:org_id, ?1, ?2, ?3, ?4, '', 'error', ?5, 'director', ?6, ?6)",
        params![id, conv_id, through, based_on, detail, now],
    )
    .map_err(db_err)?;
    touch(db, Record::Summary, &id).map_err(db_err)?;
    touch(db, Record::Conversation, conv_id).map_err(db_err)?;
    crate::logf!("conversation {conv_id}: summary of turns 1-{through} failed: {why}");
    Ok(())
}

pub(crate) fn set_turn(
    db: &ScopedConnection,
    conv: &str,
    turn: &str,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Res<()> {
    db.execute(sql, args).map_err(db_err)?;
    touch(db, Record::Turn, turn).map_err(db_err)?;
    touch(db, Record::Conversation, conv).map_err(db_err)?;
    Ok(())
}

/// Record and advance a new turn inside the caller's write transaction.
pub(crate) fn enqueue(
    store: &Store,
    db: &ScopedConnection,
    conversation: &str,
    message: &str,
    actor: &str,
    origin: &str,
    detail: Option<&Value>,
) -> Res<String> {
    let turn = format!("trn_{}", util::ulid()?.to_lowercase());
    db.execute(
        "INSERT INTO turns (org_id, turn_id, conversation_id, seq, message, state, posted_by, posted_at, origin, detail)
         VALUES (:org_id, ?1, ?2, (SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE +org_id = :org_id AND conversation_id = ?2), ?3,
                 'queued', ?4, ?5, ?6, ?7)",
        params![turn, conversation, message, actor, now_utc(), origin, detail.map(Value::to_string)],
    )
    .map_err(db_err)?;
    touch(db, Record::Turn, &turn).map_err(db_err)?;
    touch(db, Record::Conversation, conversation).map_err(db_err)?;
    advance(store, db, conversation)?;
    Ok(turn)
}

/// Start the next turn when nothing is running and the queue is not held. Call after any change to a conversation.
pub fn advance(store: &Store, db: &ScopedConnection, conv_id: &str) -> Res<()> {
    // Each pass starts a turn, waits, or ends a turn that cannot start and looks again.
    while step(store, db, conv_id)? {}
    Ok(())
}

fn failed_summary_through(db: &ScopedConnection, conv_id: &str) -> Res<Option<i64>> {
    // The newest summary attempt failed: its portion is not tried again until someone redoes it.
    Ok(db
        .query_row(
            "SELECT through_seq, state FROM summaries WHERE +org_id = :org_id AND conversation_id = ?1 ORDER BY rowid DESC LIMIT 1",
            params![conv_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(db_err)?
        .filter(|(_, s)| s == "error")
        .map(|(t, _)| t))
}

fn project_log(db: &ScopedConnection, conv: &Conv) -> Res<Option<TurnLog>> {
    match (conv.lead, conv.project.as_deref()) {
        (true, Some(project)) => decision_log::for_turn(db, project).map_err(|e| e.2),
        _ => Ok(None),
    }
}

fn step(store: &Store, db: &ScopedConnection, conv_id: &str) -> Res<bool> {
    let Some(conv) = load(db, conv_id)? else {
        return Ok(false);
    };
    let one = |sql: &str| {
        db.query_row(sql, params![conv_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                crate::peer_messages::prompt(r.get_ref(4)?.as_str()?, r.get(2)?),
                r.get::<_, String>(3)?,
            ))
        })
        .optional()
        .map_err(db_err)
    };
    if !crate::sponsorship::can_dispatch(db, conv_id).map_err(db_err)?
        || conv.held
        || one("SELECT turn_id, seq, message, state, origin FROM turns WHERE +org_id = :org_id AND conversation_id = ?1 AND state = 'running'")?
            .is_some()
    {
        return Ok(false);
    }
    let Some((turn, seq, message, state)) = one(
        "SELECT turn_id, seq, message, state, origin FROM turns WHERE +org_id = :org_id AND conversation_id = ?1
         AND state IN ('queued', 'summarising') ORDER BY state = 'summarising' DESC, seq LIMIT 1",
    )?
    else {
        return Ok(false);
    };
    let summarising = db
        .query_row(
            "SELECT 1 FROM summaries WHERE +org_id = :org_id AND conversation_id = ?1 AND state = 'running'",
            params![conv_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(db_err)?
        .is_some();
    if summarising {
        if state == "queued" {
            set_turn(
                db,
                conv_id,
                &turn,
                "UPDATE turns SET state = 'summarising' WHERE +org_id = :org_id AND (turn_id = ?1)",
                &[&turn],
            )?;
        }
        return Ok(false);
    }
    let turns = past(db, conv_id, seq)?;
    let best = best_summary(db, conv_id, seq)?;
    let failed = failed_summary_through(db, conv_id)?;
    let log = project_log(db, &conv)?;
    let instructions = conv.instructions
        + log.as_ref().map_or(0, |log| {
            thread::tokens(&log.markdown) + thread::tokens(&log.lessons)
        });
    let budget = thread::budget(&conv.model, instructions, conv.tools);
    match thread::choose(&turns, &message, best.as_ref(), &budget, failed) {
        Choice::Summarise { through, based_on } => {
            if !try_summary(store, db, conv_id, through, based_on.as_deref())? {
                return Ok(true);
            }
            set_turn(
                db,
                conv_id,
                &turn,
                "UPDATE turns SET state = 'summarising' WHERE +org_id = :org_id AND (turn_id = ?1)",
                &[&turn],
            )?;
            Ok(false)
        }
        Choice::Send { summary, over } => {
            let eligible = Eligible {
                turn,
                seq,
                message,
                log,
                budget,
            };
            send(
                store,
                db,
                &conv,
                &eligible,
                turns,
                summary.and(best.as_ref()),
                over,
            )
        }
    }
}

/// The turn that may start next.
struct Eligible {
    turn: String,
    seq: i64,
    message: String,
    log: Option<TurnLog>,
    budget: thread::Budget,
}

/// Offer the eligible turn's job with the summary (if any) and the turns after it whole. `Ok(true)` when the job
/// could not be offered, so the turn ended and the queue should be looked at again.
fn send(
    store: &Store,
    db: &ScopedConnection,
    conv: &Conv,
    el: &Eligible,
    turns: Vec<Past>,
    summary: Option<&Summary>,
    over: bool,
) -> Res<bool> {
    let (conv_id, turn, seq, message) = (conv.id.as_str(), &el.turn, el.seq, &el.message);
    let budget = &el.budget;
    let from = summary.map_or(0, |s| s.through);
    let whole: Vec<Past> = turns.into_iter().filter(|t| t.seq > from).collect();
    let context = thread::render(summary, &whole);
    let mut ctx = json!({
        "conversation_revision": conv.revision, "spec_version": conv.spec_version, "model": conv.model,
        "context_window": budget.window, "window_source": budget.source, "threshold_tokens": budget.threshold,
        "tokens": thread::tokens(&context) + thread::tokens(message), "over_threshold": over,
        "summary_id": summary.map(|s| s.id.clone()),
        "summary_through_seq": summary.map(|s| s.through),
        "whole_from_seq": whole.first().map(|t| t.seq), "whole_through_seq": whole.last().map(|t| t.seq),
    });
    if let Some(log) = &el.log {
        ctx["decision_log"] = json!({"revision": log.revision, "entry_count": log.entry_count});
    }
    match make_job(store, db, conv, &context, message, el.log.as_ref()) {
        Ok(job) => {
            set_turn(
                db,
                conv_id,
                turn,
                "UPDATE turns SET state = 'running', job_id = ?1, context = ?2, started_at = ?3
                 WHERE +org_id = :org_id AND (turn_id = ?4)",
                &[&job, &ctx.to_string(), &now_utc(), &turn],
            )?;
            crate::logf!("conversation {conv_id}: turn {seq} is job {job}");
            Ok(false)
        }
        Err(e) => {
            // The job cannot be offered (for instance its node was revoked): the turn ends, the queue moves.
            set_turn(
                db,
                conv_id,
                turn,
                "UPDATE turns SET state = 'error', detail = CASE WHEN origin = 'wake' AND json_valid(detail)
                     THEN json_set(detail, '$.error', ?1) ELSE ?1 END, context = ?2, finished_at = ?3
                 WHERE +org_id = :org_id AND (turn_id = ?4)",
                &[
                    &format!("its job could not be offered: {e}"),
                    &ctx.to_string(),
                    &now_utc(),
                    turn,
                ],
            )?;
            Ok(true)
        }
    }
}

/// Cancel a job for a stop, unless it is over or already has a cancel. Returns the cancel's id.
fn cancel_job(db: &ScopedConnection, job_id: &str, who: &str) -> Res<Option<String>> {
    let job: Option<(String, i64, Option<String>)> = db
        .query_row(
            "SELECT state, node_id, (SELECT cancel_id FROM cancels WHERE +org_id = :org_id AND job_id = ?1) FROM jobs WHERE +org_id = :org_id AND job_id = ?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(db_err)?;
    match job {
        Some((_, _, Some(c))) => Ok(Some(c)),
        Some((s, node, None)) if !matches!(s.as_str(), "done" | "error") => {
            crate::work::issue_cancel(db, job_id, node, Some("the conversation was stopped"), who)
                .map(Some)
        }
        _ => Ok(None),
    }
}

/// Stop: hold the queue at once and request cancel of what is running. Returns the active turn's cancel, if any.
pub fn stop(store: &Store, db: &ScopedConnection, conv_id: &str, who: &str) -> Res<Option<String>> {
    db.execute(
        "UPDATE conversations SET held = 1 WHERE +org_id = :org_id AND (conversation_id = ?1)",
        params![conv_id],
    )
    .map_err(db_err)?;
    touch(db, Record::Conversation, conv_id).map_err(db_err)?;
    let running: Option<String> = db
        .query_row(
            "SELECT job_id FROM turns WHERE +org_id = :org_id AND conversation_id = ?1 AND state = 'running'",
            params![conv_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    let cancel = match &running {
        Some(job) => cancel_job(db, job, who)?,
        None => None,
    };
    // A turn waiting for a summary goes back to the queue; the summary's job is cancelled.
    let waiting: Option<String> = db
        .query_row(
            "SELECT turn_id FROM turns WHERE +org_id = :org_id AND conversation_id = ?1 AND state = 'summarising'",
            params![conv_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    if let Some(turn) = waiting {
        set_turn(
            db,
            conv_id,
            &turn,
            "UPDATE turns SET state = 'queued' WHERE +org_id = :org_id AND (turn_id = ?1)",
            &[&turn],
        )?;
        let mut stmt = db
            .prepare(
                "SELECT job_id FROM summaries WHERE +org_id = :org_id AND conversation_id = ?1 AND state = 'running'",
            )
            .map_err(db_err)?;
        let jobs: Vec<String> = stmt
            .query_map(params![conv_id], |r| r.get(0))
            .map_err(db_err)?
            .collect::<Result<_, _>>()
            .map_err(db_err)?;
        for job in jobs {
            cancel_job(db, &job, who)?;
            crate::settle::settle_job(store, db, &job)?;
        }
    }
    // A job that was still waiting for budget is over already; its turn ends now.
    if let Some(job) = running {
        crate::settle::settle_job(store, db, &job)?;
    }
    Ok(cancel)
}

/// Resume: release the queue and start the next turn.
pub fn resume(store: &Store, db: &ScopedConnection, conv_id: &str) -> Res<()> {
    db.execute(
        "UPDATE conversations SET held = 0 WHERE +org_id = :org_id AND (conversation_id = ?1)",
        params![conv_id],
    )
    .map_err(db_err)?;
    touch(db, Record::Conversation, conv_id).map_err(db_err)?;
    advance(store, db, conv_id)
}
