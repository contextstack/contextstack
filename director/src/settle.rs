//! How a conversation's work ends: when a turn's or summary's job is over and its run reported, the
//! reply is taken from the run's evidence (`thread::reply_of`), the turn or summary is settled as `done`, `error`
//! or `stopped`, and the queue moves on.

use crate::store::{Record, Store, touch};
use crate::thread;
use crate::turns::{Res, advance, db_err, set_turn};
use crate::util::now_utc;
use rusqlite::{Connection, OptionalExtension, params};

/// How a job ended, once it has: its state, the reply its run carried, and whether a cancel stopped it.
fn ending(
    store: &Store,
    db: &Connection,
    job_id: &str,
) -> Res<Option<(String, Option<String>, bool)>> {
    let job: Option<(String, Option<String>, Option<String>)> = db
        .query_row(
            "SELECT j.state, j.run_id, k.outcome FROM jobs j LEFT JOIN cancels c ON c.job_id = j.job_id
             LEFT JOIN controls k ON k.control_id = c.control_id WHERE j.job_id = ?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(db_err)?;
    let Some((state, run, outcome)) = job else {
        return Ok(None);
    };
    if !matches!(state.as_str(), "done" | "error") {
        return Ok(None);
    }
    let mut reply = None;
    if let Some(run) = run {
        let recorded = db
            .query_row("SELECT 1 FROM runs WHERE run_id = ?1", params![run], |_| {
                Ok(())
            })
            .optional()
            .map_err(db_err)?
            .is_some();
        if !recorded {
            return Ok(None); // the run's report is still on its way
        }
        let sha: Option<String> = db
            .query_row(
                "SELECT sha256 FROM run_files WHERE run_id = ?1 AND path = 'messages.jsonl'",
                params![run],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        reply = sha
            .and_then(|s| store.read_object(&s))
            .and_then(|m| thread::reply_of(&m));
    }
    Ok(Some((state, reply, outcome.as_deref() == Some("stopped"))))
}

/// A job ended or its run arrived: settle the turn or summary it carries, then move the queue.
pub fn settle_job(store: &Store, db: &Connection, job_id: &str) -> Res<()> {
    let owner = |sql: &str| {
        db.query_row(sql, params![job_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .optional()
        .map_err(db_err)
    };
    let turn = owner(
        "SELECT turn_id, conversation_id FROM turns WHERE job_id = ?1 AND state = 'running'",
    )?;
    let summary = owner(
        "SELECT summary_id, conversation_id FROM summaries WHERE job_id = ?1 AND state = 'running'",
    )?;
    if turn.is_none() && summary.is_none() {
        return Ok(());
    }
    let Some((job_state, reply, stopped)) = ending(store, db, job_id)? else {
        return Ok(());
    };
    let (state, detail) = match (stopped, job_state.as_str(), &reply) {
        (true, _, _) => ("stopped", Some("stopped at the person's request")),
        (false, "done", Some(_)) => ("done", None),
        (false, "done", None) => ("error", Some("the run reported no reply")),
        _ => ("error", Some("the job ended in error")),
    };
    let now = now_utc();
    if let Some((id, conv)) = turn {
        set_turn(
            db,
            &conv,
            &id,
            "UPDATE turns SET state = ?1, reply = ?2, detail = ?3, finished_at = ?4 WHERE turn_id = ?5",
            &[&state, &reply, &detail, &now, &id],
        )?;
        crate::logf!("conversation {conv}: turn {id} {state}");
        advance(store, db, &conv)?;
    }
    if let Some((id, conv)) = summary {
        db.execute(
            "UPDATE summaries SET state = ?1, text = ?2, detail = ?3, finished_at = ?4 WHERE summary_id = ?5",
            params![state, reply, detail, now, id],
        )
        .map_err(db_err)?;
        touch(db, Record::Summary, &id).map_err(db_err)?;
        touch(db, Record::Conversation, &conv).map_err(db_err)?;
        crate::logf!("conversation {conv}: summary {id} {state}");
        advance(store, db, &conv)?;
    }
    Ok(())
}

/// After an acked envelope: a job's end or a run's report may settle a turn. A failure here is logged and rolled
/// back on its own; it never refuses the envelope.
pub fn after_envelope(store: &Store, db: &Connection, job_terminal: bool, subject: &str) {
    let run = || -> Res<()> {
        if job_terminal {
            return settle_job(store, db, subject);
        }
        let job: Option<String> = db
            .query_row(
                "SELECT job_id FROM jobs WHERE run_id = ?1",
                params![subject],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        job.map_or(Ok(()), |j| settle_job(store, db, &j))
    };
    if db.execute_batch("SAVEPOINT turns").is_err() {
        return;
    }
    let done = run();
    let end = if done.is_ok() {
        "RELEASE turns"
    } else {
        "ROLLBACK TO turns; RELEASE turns"
    };
    if let Err(e) = done {
        crate::logf!("conversations after {subject}: {e}");
    }
    let _ = db.execute_batch(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turns;
    use crate::util::sha256_hex;

    type R = Result<(), Box<dyn std::error::Error>>;

    /// A store with node `node-a` and conversation `c1` on a spec that names the generated files.
    fn setup(dir: &std::path::Path) -> Result<Store, Box<dyn std::error::Error>> {
        let store = Store::open(&dir.join("data"))?;
        crate::access::register_node(&store, "node-a")?;
        let spec: &[u8] =
            b"model: local-coder\nsystem: prompts/p.md\ncontext: [conversation/context.md]\n\
                            tasks: [tasks/turn]\n";
        let prompt: &[u8] = b"Be brief.\n";
        let db = store.db();
        db.execute(
            "INSERT INTO conversations (conversation_id, node, spec, spec_version, model, instructions_tokens, tools,
                                        created_by, created_at)
             VALUES ('c1', 'node-a', 'spec.yaml', 'v1', 'local-coder', 3, 0, 'cred', ?1)",
            params![now_utc()],
        )?;
        for (path, data) in [("spec.yaml", spec), ("prompts/p.md", prompt)] {
            store.put_object(data)?;
            db.execute(
                "INSERT INTO conversation_files VALUES ('c1', ?1, ?2, ?3, 420)",
                params![path, sha256_hex(data), i64::try_from(data.len())?],
            )?;
        }
        drop(db);
        Ok(store)
    }

    fn post(store: &Store, id: &str, message: &str) -> R {
        let db = store.db();
        db.execute(
            "INSERT INTO turns (turn_id, conversation_id, seq, message, state, posted_by, posted_at)
             VALUES (?1, 'c1', (SELECT COALESCE(MAX(seq), 0) + 1 FROM turns), ?2, 'queued', 'cred', ?3)",
            params![id, message, now_utc()],
        )?;
        turns::advance(store, &db, "c1")?;
        Ok(())
    }

    /// `(state, job_id, reply)` of a turn.
    fn turn(
        store: &Store,
        id: &str,
    ) -> Result<(String, Option<String>, Option<String>), rusqlite::Error> {
        store.db().query_row(
            "SELECT state, job_id, reply FROM turns WHERE turn_id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    }

    /// The node reports the job over, with a run whose last model reply is `reply` (none: no run).
    fn finish(store: &Store, job: &str, state: &str, reply: Option<&str>) -> R {
        let db = store.db();
        let run = reply.map(|text| {
            let messages = format!(
                "{{\"event\":\"turn\",\"turn\":1,\"text\":{}}}\n",
                serde_json::json!(text)
            );
            (format!("run-{job}"), messages)
        });
        if let Some((run_id, messages)) = &run {
            store.put_object(messages.as_bytes())?;
            db.execute(
                "INSERT INTO runs (run_id, node_id, usage, received_at) VALUES (?1, 1, '{}', ?2)",
                params![run_id, now_utc()],
            )?;
            db.execute(
                "INSERT INTO run_files VALUES (?1, 'messages.jsonl', ?2, ?3)",
                params![
                    run_id,
                    sha256_hex(messages.as_bytes()),
                    i64::try_from(messages.len())?
                ],
            )?;
        }
        db.execute(
            "UPDATE jobs SET state = ?1, run_id = ?2 WHERE job_id = ?3",
            params![state, run.map(|r| r.0), job],
        )?;
        settle_job(store, &db, job)?;
        Ok(())
    }

    fn context_of(store: &Store, job: &str) -> Result<String, Box<dyn std::error::Error>> {
        let sha: String = store.db().query_row(
            "SELECT sha256 FROM job_files WHERE job_id = ?1 AND path = ?2",
            params![job, thread::CONTEXT_PATH],
            |r| r.get(0),
        )?;
        Ok(String::from_utf8(
            store.read_object(&sha).ok_or("no object")?,
        )?)
    }

    #[test]
    fn one_turn_at_a_time_and_the_next_carries_the_reply() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        post(&store, "t1", "first")?;
        post(&store, "t2", "second")?;
        let (s1, j1, _) = turn(&store, "t1")?;
        let (s2, j2, _) = turn(&store, "t2")?;
        assert_eq!((s1.as_str(), s2.as_str(), j2), ("running", "queued", None));
        let j1 = j1.ok_or("no job")?;
        assert!(context_of(&store, &j1)?.contains("first message of the conversation"));
        finish(&store, &j1, "done", Some("REPLY-1"))?;
        assert_eq!(turn(&store, "t1")?.2.as_deref(), Some("REPLY-1"));
        let (s2, j2, _) = turn(&store, "t2")?;
        assert_eq!(s2, "running");
        let context = context_of(&store, &j2.ok_or("no job")?)?;
        assert!(
            context.contains("first") && context.contains("REPLY-1") && !context.contains("second")
        );
        Ok(())
    }

    #[test]
    fn stop_holds_the_queue_and_resume_releases_it() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        post(&store, "t1", "first")?;
        post(&store, "t2", "second")?;
        let j1 = turn(&store, "t1")?.1.ok_or("no job")?;
        let cancel = turns::stop(&store, &store.db(), "c1", "cred")?.ok_or("no cancel")?;
        // The node applies the cancel and reports the job over, without a run.
        store.db().execute(
            "UPDATE controls SET state = 'applied', outcome = 'stopped'
             WHERE control_id = (SELECT control_id FROM cancels WHERE cancel_id = ?1)",
            params![cancel],
        )?;
        finish(&store, &j1, "error", None)?;
        assert_eq!(turn(&store, "t1")?.0, "stopped");
        assert_eq!(turn(&store, "t2")?.0, "queued", "held");
        turns::resume(&store, &store.db(), "c1")?;
        let (s2, j2, _) = turn(&store, "t2")?;
        assert_eq!(s2, "running");
        assert!(context_of(&store, &j2.ok_or("no job")?)?.contains("the turn was stopped"));
        Ok(())
    }

    #[test]
    fn a_run_without_a_reply_is_an_error_and_the_queue_moves() -> R {
        let tmp = tempfile::tempdir()?;
        let store = setup(tmp.path())?;
        post(&store, "t1", "first")?;
        post(&store, "t2", "second")?;
        let j1 = turn(&store, "t1")?.1.ok_or("no job")?;
        finish(&store, &j1, "done", Some("   "))?;
        assert_eq!(turn(&store, "t1")?.0, "error");
        assert_eq!(turn(&store, "t2")?.0, "running");
        Ok(())
    }
}
