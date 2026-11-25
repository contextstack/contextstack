//! A planner and its deferred coding bundle share one intent. The terminal report
//! and the coding offer are committed together, including on envelope replay.

use crate::jobs::{BundleFile, Plan};
use crate::server::State;
use crate::store::{Record, Store, touch, write_tx};
use crate::util::{now_utc, sha256_hex, utc_in};
use crate::work::No;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// A frozen plan is cut at this size, with `TRUNCATED` appended.
const MAX_PLAN_BYTES: usize = 64 * 1024;
/// A larger `plan.md` is not read at all.
const MAX_PLAN_READ: u32 = 1024 * 1024;
const TRUNCATED: &str = "\n[plan truncated by the Director]\n";
const CROSS_LANGUAGE_MIN: usize = 2;
pub(crate) const LOCAL_MODEL: &str = "harness:local-coder";
pub(crate) const CODEX_MODEL: &str = "codex";

// Routing counts language families, including Blade as PHP and Vue as JS.
const LANGUAGE_EXTENSIONS: &[(&str, &[&str])] = &[
    ("rust", &["rs"]),
    ("python", &["py", "pyi"]),
    ("php", &["php"]),
    (
        "js",
        &["js", "jsx", "ts", "tsx", "vue", "mjs", "cjs", "mts", "cts"],
    ),
    ("go", &["go"]),
    ("ruby", &["rb"]),
    ("java", &["java"]),
    ("kotlin", &["kt", "kts"]),
    ("swift", &["swift"]),
    ("c", &["c", "h"]),
    ("cpp", &["cc", "cpp", "cxx", "hh", "hpp", "hxx"]),
    ("csharp", &["cs"]),
    ("shell", &["sh", "bash", "zsh"]),
    ("sql", &["sql"]),
];

pub(crate) fn auto_requested(lane: &Value) -> bool {
    lane["model"] == "auto"
}

fn file_languages(markdown: &str) -> Option<BTreeSet<&'static str>> {
    let mut labels = markdown
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "## Files")
        .skip(1)
        .take_while(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let (path, label) = line
                .strip_prefix("- ")?
                .strip_suffix(')')?
                .rsplit_once(" (")?;
            (crate::util::plain_path(path.trim())
                && !label.is_empty()
                && label.bytes().all(|byte| byte.is_ascii_lowercase()))
            .then_some(label)
        })
        .peekable();
    // A docs-only list is authoritative too. Check for entries before filtering languages.
    labels.peek()?;
    Some(
        labels
            .filter_map(|label| {
                let label = match label {
                    "javascript" | "typescript" => "js",
                    "sh" | "bash" | "zsh" => "shell",
                    _ => label,
                };
                LANGUAGE_EXTENSIONS
                    .iter()
                    .find_map(|(language, _)| (*language == label).then_some(*language))
            })
            .collect(),
    )
}

fn extension_languages(markdown: &str) -> BTreeSet<&'static str> {
    // Accept inline code, bare paths, and Markdown link destinations. Only
    // recognized extensions count, so docs, config, and lock files are ignored.
    markdown
        .split(|c: char| c.is_whitespace() || "`*~()[]{}<>,;'\"".contains(c))
        .filter_map(|token| {
            let token = token.trim_end_matches(['.', ':']);
            let path = token.strip_prefix("./").unwrap_or(token);
            if path.contains(':') || !crate::util::plain_path(path) {
                return None;
            }
            let (stem, ext) = path.rsplit('/').next()?.rsplit_once('.')?;
            if stem.is_empty() {
                return None;
            }
            LANGUAGE_EXTENSIONS
                .iter()
                .find_map(|(language, extensions)| {
                    extensions
                        .iter()
                        .any(|entry| ext.eq_ignore_ascii_case(entry))
                        .then_some(*language)
                })
        })
        .collect()
}

struct Request {
    model: String,
    goal: String,
    auto: bool,
}

fn request(m: &Map<String, Value>) -> Result<Option<Request>, No> {
    let lane = m.get("lane").unwrap_or(&Value::Null);
    let model = match lane.get("plan_with") {
        None | Some(Value::Null) if auto_requested(lane) => CODEX_MODEL,
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
        model: model.to_owned(),
        goal: lane["goal"].as_str().unwrap_or_default().to_owned(),
        auto: auto_requested(lane),
    }))
}

fn pick_node(
    db: &Connection,
    executor: &str,
    model: &str,
    repo: &str,
) -> Result<Option<String>, String> {
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
        let has_repo = node == executor
            || status["repos"].as_array().is_some_and(|repos| {
                repos
                    .iter()
                    .any(|entry| entry["name"].as_str() == Some(repo))
            });
        let suitable = if let Some(model) = model.strip_prefix("harness:") {
            crate::capabilities::validate_harness_model(model, &status, &node).is_ok()
        } else if node == executor {
            crate::capabilities::refuse_dispatch(db, &node, model, false).is_ok()
        } else {
            crate::capabilities::can_run(model, &status)
        };
        if has_repo && suitable && !known_linux {
            return Ok(Some(node));
        }
    }
    Ok(None)
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
    let node = pick_node(db, executor_node, &request.model, repo)?.ok_or_else(|| {
        format!(
            "no read-only planner node can run {} for {repo}",
            request.model
        )
    })?;
    let goal = format!(
        "{}\n\nWrite a short `plan.md` (about 60 lines maximum) to the output path supplied above. \
         Explain why each file needs to change, the change in each file, the order of edits, and the check or tests to verify the work. \
         Research the goal without editing repository files.\n\n\
         End the plan with a machine-readable section headed `## Files`. Use one line per file that will change, \
         with its repository-relative path and language in the exact format shown below. Use a single lowercase word \
         for each language, including for documentation, config, and data files. For extensionless files, read the \
         shebang or the file itself to identify the language.\n\n\
         ## Files\n- path/to/file.rs (rust)\n- cs/ops/bin/finalize-run (python)",
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
    if request.auto {
        let mut form = submitted["lane"].clone();
        form["model"] = json!(CODEX_MODEL);
        form["agent"] = json!("coder");
        form["lane"] = want["meta"]["lane"].clone();
        let (_, contents) = crate::lane::build(&form, &crate::lane::root()?)?;
        let (path, spec, mode) = contents
            .into_iter()
            .find(|(path, _, _)| path == "spec.yaml")
            .ok_or_else(|| No::internal(&"missing Codex coding spec"))?;
        let bytes = crate::stamp::with_intent(spec.as_bytes(), intent, None)
            .map_err(|e| No::internal(&format!("{e:?}")))?
            .ok_or_else(|| No::internal(&"missing intent stamp"))?;
        state
            .store
            .put_object(&bytes)
            .map_err(|e| No::internal(&e))?;
        executor.enable_auto(BundleFile {
            path,
            mode,
            sha256: sha256_hex(&bytes),
            bytes: bytes.len() as u64,
        });
    }
    match prepare(state, db, want, &request, intent) {
        Ok(planner) => Ok(Some(planner)),
        Err(detail) => {
            fallback(executor, &detail).map_err(|e| No::internal(&e))?;
            Ok(None)
        }
    }
}

fn route(executor: &mut Plan, plan: Result<&str, &str>) -> Result<(), String> {
    if !executor.is_auto() {
        return Ok(());
    }
    let (languages, from_files) = match plan {
        Ok(markdown) => match file_languages(markdown) {
            Some(languages) => (languages, true),
            None => (extension_languages(markdown), false),
        },
        Err(_) => (BTreeSet::new(), false),
    };
    let model = if languages.len() >= CROSS_LANGUAGE_MIN {
        CODEX_MODEL
    } else {
        LOCAL_MODEL
    };
    let listed = languages.iter().copied().collect::<Vec<_>>().join("+");
    let reason = match plan {
        Err(detail) => format!("{detail} → {model}"),
        Ok(_) if from_files && languages.is_empty() => {
            format!("plan files list no recognized coding languages → {model}")
        }
        Ok(_) if from_files => format!("plan files list {listed} → {model}"),
        Ok(_) if languages.is_empty() => {
            format!("plan names no files with recognized coding extensions → {model}")
        }
        Ok(_) => format!("plan names {listed} (extensions) → {model}"),
    };
    // The coding job stays on the intent's node; only the planner may run elsewhere.
    executor.route(json!({"model": model, "reason": reason, "languages": languages}))
}

fn fallback(executor: &mut Plan, detail: &str) -> Result<(), String> {
    executor.set_work_reason(Some(
        json!({
            "kind": "planning_fallback", "detail": detail,
        })
        .to_string(),
    ));
    route(executor, Err(detail))
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

/// The planner's `plan.md`, whole. Size is capped when it is frozen (`frozen`), not here, so routing still reads
/// the `## Files` section a long plan ends with.
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
        params![job, run, MAX_PLAN_READ],
        |r| r.get(0),
    ).optional().map_err(|e| e.to_string())?;
    Ok(sha
        .and_then(|sha| store.read_object(&sha))
        .filter(|bytes| bytes.len() <= MAX_PLAN_READ as usize)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|text| !text.trim().is_empty()))
}

/// The plan as frozen into the coding bundle: whole up to `MAX_PLAN_BYTES`, else cut at a line and marked.
fn frozen(plan: &str) -> std::borrow::Cow<'_, str> {
    if plan.len() <= MAX_PLAN_BYTES {
        return plan.into();
    }
    let mut end = MAX_PLAN_BYTES;
    while !plan.is_char_boundary(end) {
        end -= 1;
    }
    let end = plan[..end].rfind('\n').map_or(end, |at| at + 1);
    format!("{}{TRUNCATED}", &plan[..end]).into()
}

/// Settle a planner job: offer its coding job, planned or as a fallback. A failure while planning falls back to
/// the default executor with the error as its reason; a row that cannot be settled at all is logged and left for
/// the sweep. Neither refuses the caller (an envelope ack or the sweep's other rows).
pub fn advance(store: &Store, db: &Connection, job: &str, expired: bool) -> Result<(), String> {
    db.execute_batch("SAVEPOINT planning_advance")
        .map_err(|e| e.to_string())?;
    let mut outcome = settle(store, db, job, expired, None);
    if let Err(detail) = &outcome {
        db.execute_batch("ROLLBACK TO planning_advance")
            .map_err(|e| e.to_string())?;
        let detail = format!("planning failed: {detail}");
        outcome = settle(store, db, job, expired, Some(&detail));
        if let Err(error) = &outcome {
            db.execute_batch("ROLLBACK TO planning_advance")
                .map_err(|e| e.to_string())?;
            eprintln!("director: planning job {job} not settled: {detail}; fallback: {error}");
        }
    }
    db.execute_batch("RELEASE planning_advance")
        .map_err(|e| e.to_string())
}

/// `failed` forces the fallback with that reason.
fn settle(
    store: &Store,
    db: &Connection,
    job: &str,
    expired: bool,
    failed: Option<&str>,
) -> Result<(), String> {
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
            "done" if failed.is_some() => fallback(&mut executor, failed.unwrap_or_default())?,
            "done" => {
                if let Some(plan) = markdown(store, db, job, run.as_deref())? {
                    executor.freeze_plan(store, &frozen(&plan))?;
                    route(&mut executor, Ok(&plan))?;
                    executor.set_work_reason(Some(
                        json!({"kind": "planned", "planner_job_id": job}).to_string(),
                    ));
                } else {
                    fallback(&mut executor, "planning finished without a usable plan.md")?;
                }
            }
            "error" => fallback(&mut executor, failed.unwrap_or("planning job failed"))?,
            _ if expired => {
                crate::work::issue_cancel(
                    db,
                    job,
                    node_id,
                    Some("planning dispatch deadline"),
                    "director",
                )?;
                fallback(
                    &mut executor,
                    failed.unwrap_or("planning exceeded its dispatch deadline"),
                )?;
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
         WHERE p.deadline <= ?1 OR j.state = 'error' ORDER BY p.job_id",
        )
        .and_then(|mut stmt| stmt.query_map([now_utc()], |r| r.get(0))?.collect())
        .map_err(|e| e.to_string())?;
    for job in jobs {
        // One row's failure must not hold up the rest.
        let settled = write_tx(&mut db).map_err(|e| e.to_string()).and_then(|tx| {
            advance(store, &tx, &job, true)?;
            tx.commit().map_err(|e| e.to_string())
        });
        if let Err(error) = settled {
            eprintln!("director: planning sweep skipped {job}: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::register_node;

    type Res = Result<(), Box<dyn std::error::Error>>;

    fn file(store: &Store, path: &str, data: &[u8], keep: bool) -> Result<BundleFile, String> {
        if keep {
            store.put_object(data)?;
        }
        Ok(BundleFile {
            path: path.into(),
            sha256: sha256_hex(data),
            bytes: data.len() as u64,
            mode: 0o644,
        })
    }

    /// A lead node `lead` that reports the repo but no Codex, and a node `other` that can run Codex.
    fn setup(tmp: &tempfile::TempDir) -> Result<Store, Box<dyn std::error::Error>> {
        let store = Store::open(&tmp.path().join("data"))?;
        register_node(&store, "lead")?;
        register_node(&store, "other")?;
        let repos = json!([{"name": "demo"}]);
        let db = store.db();
        db.execute(
            "UPDATE nodes SET status = ?1 WHERE name = 'lead'",
            [
                json!({"repos": repos, "agent_capabilities": {"templates": [], "harness": true}})
                    .to_string(),
            ],
        )?;
        db.execute(
            "UPDATE nodes SET status = ?1 WHERE name = 'other'",
            [json!({"repos": repos, "agent_capabilities": {"templates": ["codex"]}}).to_string()],
        )?;
        drop(db);
        Ok(store)
    }

    fn executor(
        store: &Store,
        db: &Connection,
        goal_stored: bool,
        auto: bool,
    ) -> Result<Plan, String> {
        let files = vec![
            file(store, "spec.yaml", b"id: x\ntasks: [tasks/t]\n", true)?,
            file(store, "tasks/t/goal.md", b"Fix it.", goal_stored)?,
        ];
        let mut meta = Map::new();
        meta.insert("repo".into(), json!("demo"));
        let mut plan = Plan::new(db, "lead", "spec.yaml", "tasks/t", 1, None, meta, files)?;
        plan.mark_coding();
        if auto {
            plan.enable_auto(file(
                store,
                "spec.yaml",
                b"id: x\nmodel: codex\ntasks: [tasks/t]\n",
                true,
            )?);
        }
        Ok(plan)
    }

    /// An intent on `lead` with a planner job on `lead` in `state`, deferring `executor`.
    fn planning(db: &Connection, intent: &str, job: &str, state: &str, executor: &str) -> Res {
        db.execute(
            "INSERT INTO intents (intent_id, node, spec, task, trials, meta, bundle_sha256, created_by, created_at)
             VALUES (?1, 'lead', 'spec.yaml', 'tasks/t', 1, '{}', 'x', 'cred', ?2)",
            params![intent, now_utc()],
        )?;
        db.execute(
            "INSERT INTO jobs (job_id, node_id, spec, task, trials, bundle_sha256, meta, state, created_at, intent_id)
             VALUES (?1, 1, 'spec.yaml', 'tasks/t', 1, 'x', '{\"phase\":\"planning\"}', ?2, ?3, ?4)",
            params![job, state, now_utc(), intent],
        )?;
        db.execute(
            "INSERT INTO planning_jobs (job_id, executor, deadline) VALUES (?1, ?2, ?3)",
            params![job, executor, utc_in(900)],
        )?;
        Ok(())
    }

    /// Give a done planner job a run with `plan.md`.
    fn planned(store: &Store, db: &Connection, job: &str, run: &str, plan: &str) -> Res {
        store.put_object(plan.as_bytes())?;
        db.execute(
            "INSERT INTO runs (run_id, node_id, usage, received_at, job_id) VALUES (?1, 1, '{}', ?2, ?3)",
            params![run, now_utc(), job],
        )?;
        db.execute(
            "INSERT INTO run_files (run_id, path, sha256, bytes) VALUES (?1, 'plan.md', ?2, ?3)",
            params![run, sha256_hex(plan.as_bytes()), i64::try_from(plan.len())?],
        )?;
        db.execute(
            "UPDATE jobs SET run_id = ?1 WHERE job_id = ?2",
            params![run, job],
        )?;
        Ok(())
    }

    fn coding(db: &Connection, intent: &str) -> Result<(String, Value, Value), rusqlite::Error> {
        db.query_row(
            "SELECT n.name, j.meta, j.work_reason FROM jobs j JOIN nodes n ON n.id = j.node_id
             WHERE j.intent_id = ?1 AND j.meta LIKE '%\"coding\"%'",
            [intent],
            |r| {
                let meta: String = r.get(1)?;
                let reason: Option<String> = r.get(2)?;
                Ok((
                    r.get(0)?,
                    serde_json::from_str(&meta).unwrap_or_default(),
                    reason
                        .and_then(|r| serde_json::from_str(&r).ok())
                        .unwrap_or_default(),
                ))
            },
        )
    }

    const CROSS: &str = "# Plan\n\n## Files\n- a.rs (rust)\n- b.py (python)\n";

    #[test]
    fn auto_routing_keeps_the_coding_job_and_intent_on_the_leads_node() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = setup(&tmp)?;
        let db = store.db();
        let raw = serde_json::to_string(&executor(&store, &db, true, true)?)?;
        planning(&db, "intent-1", "planner-1", "done", &raw)?;
        planned(&store, &db, "planner-1", "run-1", CROSS)?;
        after_run(&store, &db, "run-1")?;
        let (node, meta, _) = coding(&db, "intent-1")?;
        assert_eq!(meta["route"]["model"], CODEX_MODEL);
        assert_eq!(node, "lead", "the coding job stays on the intent's node");
        let node: String = db.query_row(
            "SELECT node FROM intents WHERE intent_id = 'intent-1'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(node, "lead", "intents.node is unchanged");
        Ok(())
    }

    #[test]
    fn a_long_plan_is_kept_and_routed_on_its_files() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = setup(&tmp)?;
        let db = store.db();
        let raw = serde_json::to_string(&executor(&store, &db, true, true)?)?;
        planning(&db, "intent-1", "planner-1", "done", &raw)?;
        let mut plan = "# Plan\n".to_owned();
        for n in 0..120 {
            writeln!(plan, "{n}. Explain step {n} of the change.")?;
        }
        plan.push_str("\n## Files\n- a.rs (rust)\n- b.py (python)\n");
        planned(&store, &db, "planner-1", "run-1", &plan)?;
        after_run(&store, &db, "run-1")?;
        let (_, meta, reason) = coding(&db, "intent-1")?;
        assert_eq!(meta["plan"], "frozen", "{reason}");
        assert_eq!(reason["kind"], "planned");
        assert_eq!(meta["route"]["model"], CODEX_MODEL, "{meta}");
        assert_eq!(meta["route"]["languages"], json!(["python", "rust"]));
        Ok(())
    }

    #[test]
    fn an_oversized_plan_is_truncated_with_a_marker_but_still_routes_on_its_files() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = setup(&tmp)?;
        let db = store.db();
        let raw = serde_json::to_string(&executor(&store, &db, true, true)?)?;
        planning(&db, "intent-1", "planner-1", "done", &raw)?;
        let mut plan = "# Plan\n".to_owned();
        while plan.len() < MAX_PLAN_BYTES * 2 {
            plan.push_str("Explain a step of the change in some detail, é.\n");
        }
        plan.push_str("\n## Files\n- a.rs (rust)\n- b.py (python)\n");
        planned(&store, &db, "planner-1", "run-1", &plan)?;
        after_run(&store, &db, "run-1")?;
        let (_, meta, _) = coding(&db, "intent-1")?;
        assert_eq!(meta["route"]["model"], CODEX_MODEL, "{meta}");
        let (sha, bytes): (String, i64) = db.query_row(
            "SELECT sha256, bytes FROM job_files WHERE path = 'tasks/t/context/plan.md'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let kept = String::from_utf8(store.read_object(&sha).ok_or("frozen plan missing")?)?;
        assert!(usize::try_from(bytes)? <= MAX_PLAN_BYTES + TRUNCATED.len());
        assert!(kept.ends_with(TRUNCATED) && plan.starts_with(kept.trim_end_matches(TRUNCATED)));
        Ok(())
    }

    #[test]
    fn a_planning_error_falls_back_instead_of_refusing_the_envelope() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = setup(&tmp)?;
        let db = store.db();
        // The coding goal object is missing, so freezing the plan fails.
        let raw = serde_json::to_string(&executor(&store, &db, false, false)?)?;
        planning(&db, "intent-1", "planner-1", "done", &raw)?;
        planned(&store, &db, "planner-1", "run-1", CROSS)?;
        after_run(&store, &db, "run-1")?;
        let (node, meta, reason) = coding(&db, "intent-1")?;
        assert_eq!(node, "lead");
        assert!(meta.get("plan").is_none());
        assert_eq!(reason["kind"], "planning_fallback");
        assert!(
            reason["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("coding goal object missing"),
            "{reason}"
        );
        Ok(())
    }

    #[test]
    fn the_sweep_skips_a_bad_row() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = setup(&tmp)?;
        let db = store.db();
        planning(&db, "intent-0", "planner-0", "error", "{not json")?;
        let raw = serde_json::to_string(&executor(&store, &db, true, false)?)?;
        planning(&db, "intent-1", "planner-1", "error", &raw)?;
        assert!(
            advance(&store, &db, "planner-0", false).is_ok(),
            "a bad row does not refuse the envelope"
        );
        drop(db);
        sweep(&store)?;
        let db = store.db();
        let (_, _, reason) = coding(&db, "intent-1")?;
        assert_eq!(reason["kind"], "planning_fallback");
        Ok(())
    }
}
