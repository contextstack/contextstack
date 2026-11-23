//! A planner and its deferred coding bundle share one intent. The terminal report
//! and the coding offer are committed together, including on envelope replay.

use crate::jobs::{BundleFile, Plan};
use crate::server::State;
use crate::store::{Record, Store, touch, write_tx};
use crate::util::{now_utc, sha256_hex, utc_in};
use crate::work::No;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::fmt::Write as _;

const MAX_PLAN_BYTES: u32 = 16 * 1024;

struct Request {
    model: String,
    goal: String,
}

fn request(m: &Map<String, Value>) -> Result<Option<Request>, No> {
    let lane = m.get("lane").unwrap_or(&Value::Null);
    let model = match lane.get("plan_with") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(model))
            if crate::lane::model_name(model) && crate::capabilities::is_cli_template(model) =>
        {
            model
        }
        _ => {
            return Err(No::invalid(
                "lane.plan_with must name a CLI executor template",
            ));
        }
    };
    if !lane["repo"].is_string() || !lane["lane"].is_string() {
        return Err(No::invalid("lane.plan_with requires a repo and lane"));
    }
    Ok(Some(Request {
        model: model.clone(),
        goal: lane["goal"].as_str().unwrap_or_default().to_owned(),
    }))
}

fn pick_node(db: &Connection, executor: &str, model: &str, repo: &str) -> Result<String, String> {
    let mut stmt = db.prepare(
        "SELECT name, status FROM nodes WHERE revoked_at IS NULL ORDER BY name != ?1, last_seen_at DESC, name",
    ).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([executor], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (node, status) = row.map_err(|e| e.to_string())?;
        let status: Value = status
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let known_linux = status["platform"].as_str().is_some_and(|p| p != "Darwin");
        let suitable = if node == executor {
            crate::capabilities::refuse_dispatch(db, &node, model, false).is_ok()
        } else {
            crate::capabilities::can_run(model, &status)
                && status["repos"].as_array().is_some_and(|repos| {
                    repos
                        .iter()
                        .any(|entry| entry["name"].as_str() == Some(repo))
                })
        };
        if suitable && !known_linux {
            return Ok(node);
        }
    }
    Err(format!(
        "no read-only planner node can run {model} for {repo}"
    ))
}

fn prepare(
    state: &State,
    db: &Connection,
    want: &Value,
    request: &Request,
    intent: &str,
) -> Result<Plan, String> {
    let repo = want["meta"]["repo"].as_str().ok_or("missing coding repo")?;
    let executor_node = want["node"].as_str().ok_or("missing coding node")?;
    let node = pick_node(db, executor_node, &request.model, repo)?;
    let goal = format!(
        "{}\n\nWrite a short `plan.md` (about 60 lines maximum) to the output path supplied above. \
         List the files to change and why, the change in each file, the order of edits, and the check or tests to verify the work. \
         Research the goal without editing repository files.",
        request.goal
    );
    let form = json!({
        "title": want["title"], "goal": goal, "repo": repo,
        "agent": "researcher", "model": request.model,
        "budget": {"turns": 15, "seconds": 600, "usd": 0},
    });
    let root = crate::lane::root().map_err(|e| e.2)?;
    let (fields, contents) = crate::lane::build(&form, &root).map_err(|e| e.2)?;
    let mut files = Vec::new();
    let lane = want["meta"]["lane"].as_str().ok_or("missing coding lane")?;
    for (path, mut data, mode) in contents {
        if path == "spec.yaml" {
            // The coding bundle builder already validated this lane name.
            writeln!(data, "planning_lane: {lane}").map_err(|e| e.to_string())?;
        }
        state.store.put_object(data.as_bytes())?;
        files.push(BundleFile {
            path,
            sha256: sha256_hex(data.as_bytes()),
            bytes: data.len() as u64,
            mode,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut meta = fields["meta"].as_object().cloned().unwrap_or_default();
    meta.insert("phase".into(), json!("planning"));
    meta.insert("lane".into(), want["meta"]["lane"].clone());
    let mut planner = Plan::new(
        db,
        &node,
        "spec.yaml",
        fields["task"].as_str().ok_or("missing planning task")?,
        1,
        None,
        meta,
        files,
    )?
    .under(crate::reservations::JobScope {
        project: want["project_id"].as_str().map(str::to_owned),
        conversation: None,
    });
    planner
        .stamp(&state.store, intent, None)
        .map_err(|e| format!("{e:?}"))?;
    planner.set_work_reason(Some(json!({"kind": "planning"}).to_string()));
    Ok(planner)
}

pub fn prepare_if_requested(
    state: &State,
    db: &Connection,
    want: &Value,
    submitted: &Map<String, Value>,
    intent: &str,
    executor: &mut Plan,
) -> Result<Option<Plan>, No> {
    let Some(request) = request(submitted)? else {
        return Ok(None);
    };
    executor.mark_coding();
    match prepare(state, db, want, &request, intent) {
        Ok(planner) => Ok(Some(planner)),
        Err(detail) => {
            fallback(executor, &detail);
            Ok(None)
        }
    }
}

fn fallback(executor: &mut Plan, detail: &str) {
    executor.set_work_reason(Some(
        json!({
            "kind": "planning_fallback", "detail": detail,
        })
        .to_string(),
    ));
}

pub fn insert(
    store: &Store,
    db: &Connection,
    intent: &str,
    planner: &Plan,
    executor: &Plan,
) -> Result<(), String> {
    let job = planner.insert(store, db, Some(intent))?;
    let executor = serde_json::to_string(executor).map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO planning_jobs (job_id, executor, deadline) VALUES (?1, ?2, ?3)",
        params![job, executor, utc_in(900)],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn markdown(
    store: &Store,
    db: &Connection,
    job: &str,
    run: Option<&str>,
) -> Result<Option<String>, String> {
    let sha: Option<String> = db.query_row(
        "SELECT f.sha256 FROM run_files f JOIN runs r ON r.run_id = f.run_id
         JOIN jobs j ON j.job_id = ?1 AND j.node_id = r.node_id
         WHERE r.run_id = ?2 AND (r.job_id IS NULL OR r.job_id = j.job_id) AND f.path = 'plan.md' AND f.bytes BETWEEN 1 AND ?3",
        params![job, run, MAX_PLAN_BYTES],
        |r| r.get(0),
    ).optional().map_err(|e| e.to_string())?;
    Ok(sha
        .and_then(|sha| store.read_object(&sha))
        .filter(|bytes| bytes.len() <= MAX_PLAN_BYTES as usize)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|text| !text.trim().is_empty() && text.lines().count() <= 60))
}

pub fn advance(store: &Store, db: &Connection, job: &str, expired: bool) -> Result<(), String> {
    let row = db
        .query_row(
            "SELECT p.executor, j.intent_id, j.state, j.run_id,
                EXISTS(SELECT 1 FROM cancels c LEFT JOIN controls k ON k.control_id = c.control_id
                       WHERE c.job_id = j.job_id AND COALESCE(k.state, c.state) != 'refused'
                         AND COALESCE(k.outcome, '') != 'already_finished'),
                EXISTS(SELECT 1 FROM runs r WHERE r.run_id = j.run_id AND r.node_id = j.node_id),
                j.node_id
         FROM planning_jobs p JOIN jobs j ON j.job_id = p.job_id WHERE p.job_id = ?1",
            [job],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((raw, intent, state, run, cancelled, restored, node_id)) = row else {
        return Ok(());
    };
    let mut executor: Plan = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if !cancelled {
        match state.as_str() {
            "done" if run.is_some() && !restored && !expired => return Ok(()),
            "done" => {
                if let Some(plan) = markdown(store, db, job, run.as_deref())? {
                    executor.freeze_plan(store, &plan)?;
                    executor.set_work_reason(Some(
                        json!({"kind": "planned", "planner_job_id": job}).to_string(),
                    ));
                } else {
                    fallback(&mut executor, "planning finished without a usable plan.md");
                }
            }
            "error" => fallback(&mut executor, "planning job failed"),
            _ if expired => {
                crate::work::issue_cancel(
                    db,
                    job,
                    node_id,
                    Some("planning dispatch deadline"),
                    "director",
                )?;
                fallback(&mut executor, "planning exceeded its dispatch deadline");
            }
            _ => return Ok(()),
        }
        executor.insert(store, db, Some(&intent))?;
    }
    db.execute("DELETE FROM planning_jobs WHERE job_id = ?1", [job])
        .map_err(|e| e.to_string())?;
    touch(db, Record::Intent, &intent)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn after_run(store: &Store, db: &Connection, run: &str) -> Result<(), String> {
    let job: Option<String> = db
        .query_row(
            "SELECT j.job_id FROM planning_jobs p JOIN jobs j ON j.job_id = p.job_id
         JOIN runs r ON r.run_id = j.run_id AND r.node_id = j.node_id
         WHERE j.run_id = ?1 AND j.state = 'done'",
            [run],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(job) = job {
        advance(store, db, &job, false)?;
    }
    Ok(())
}

pub fn sweep(store: &Store) -> Result<(), String> {
    let mut db = store.db();
    let jobs: Vec<String> = db
        .prepare(
            "SELECT p.job_id FROM planning_jobs p JOIN jobs j ON j.job_id = p.job_id
         WHERE p.deadline <= ?1 OR j.state = 'error'",
        )
        .and_then(|mut stmt| stmt.query_map([now_utc()], |r| r.get(0))?.collect())
        .map_err(|e| e.to_string())?;
    for job in jobs {
        let tx = write_tx(&mut db).map_err(|e| e.to_string())?;
        advance(store, &tx, &job, true)?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    Ok(())
}
