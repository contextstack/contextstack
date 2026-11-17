//! Jobs: a hand-made job offered to one node from a bundle directory (`director-rs job create`), and
//! the node's view of its jobs (NODE-PROTOCOL.md §4, §5). A hand-made job belongs to no intent: its spec is offered
//! as it is, so it names its own `intent:` (or is a legacy `EXP-` spec) or the runner refuses it. A job made from an
//! intent has that intent's id written into its spec (`stamp.rs`).

use crate::store::{Record, Store, touch};
use crate::util::{self, now_utc, sha256_hex};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::fmt::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// What a hand-made job is made of.
#[derive(Debug, Default)]
pub struct NewJob {
    /// The node it is offered to.
    pub node: String,
    /// A directory holding `spec.yaml`, `tasks/<name>/goal.md` and whatever else the spec names.
    pub bundle: std::path::PathBuf,
    /// The task directory inside the bundle; found when the bundle holds exactly one.
    pub task: Option<String>,
    /// Trials, default 1.
    pub trials: u32,
    /// A grant id; `grants/<id>.yaml` must be in the bundle.
    pub grant: Option<String>,
    /// String meta for the node (`schedule_id`, `slot`, …).
    pub meta: Map<String, Value>,
}

/// One bundle file as listed in `GET jobs`.
#[derive(Debug, Clone)]
pub struct BundleFile {
    /// Its plain relative path.
    pub path: String,
    /// Its object.
    pub sha256: String,
    /// Its size.
    pub bytes: u64,
    /// 0o644 or 0o755.
    pub mode: u32,
}

/// sha256 of `path:sha256:mode\n` (mode in octal) over the files sorted by path (§4).
pub fn bundle_sha(files: &[BundleFile]) -> String {
    let text = files.iter().fold(String::new(), |mut out, f| {
        let _ = writeln!(out, "{}:{}:{:o}", f.path, f.sha256, f.mode);
        out
    });
    sha256_hex(text.as_bytes())
}

/// Every regular file under `root`, as plain relative paths sorted; symlinks and specials refused.
fn walk(root: &Path, rel: &str, out: &mut Vec<(String, std::path::PathBuf)>) -> Result<(), String> {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|n| format!("{}: not UTF-8", n.display()))?;
        let path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file() && util::plain_path(&path) {
            out.push((path, entry.path()));
        } else {
            return Err(format!("{path}: only plain regular files can be bundled"));
        }
    }
    Ok(())
}

/// Offer a job to a node: store the bundle's objects and record the job as offered. Returns the
/// job's `{job_id, bundle_sha256, files, grant}`.
pub fn create(store: &Store, job: &NewJob) -> Result<Value, String> {
    let mut found = Vec::new();
    walk(&job.bundle, "", &mut found)?;
    found.sort();
    let mut files = Vec::with_capacity(found.len());
    for (path, full) in &found {
        let data = std::fs::read(full).map_err(|e| format!("{path}: {e}"))?;
        let exec = std::fs::metadata(full)
            .map_err(|e| e.to_string())?
            .permissions()
            .mode()
            & 0o111;
        store.put_object(&data)?;
        files.push(BundleFile {
            path: path.clone(),
            sha256: sha256_hex(&data),
            bytes: data.len() as u64,
            mode: if exec == 0 { 0o644 } else { 0o755 },
        });
    }
    let task = match &job.task {
        Some(t) => t.clone(),
        None => only_task(&files)?,
    };
    let mut db = store.db();
    let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
    let plan = Plan::new(
        &tx,
        &job.node,
        "spec.yaml",
        &task,
        job.trials,
        job.grant.as_deref(),
        job.meta.clone(),
        files,
    )?;
    let job_id = plan.insert(store, &tx, None)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(
        json!({"job_id": job_id, "bundle_sha256": plan.digest, "files": plan.files.len(), "grant": job.grant}),
    )
}

/// The one `tasks/<name>` holding a `goal.md`.
fn only_task(files: &[BundleFile]) -> Result<String, String> {
    let tasks: Vec<&str> = files
        .iter()
        .filter_map(|f| f.path.strip_suffix("/goal.md"))
        .filter(|d| d.starts_with("tasks/"))
        .collect();
    match tasks.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => Err("the bundle has no tasks/<name>/goal.md".into()),
        _ => Err("the bundle holds several tasks; name one with --task".into()),
    }
}

/// Why a client's file list cannot become a bundle.
#[derive(Debug)]
pub enum Unusable {
    /// These objects are not stored yet: upload them first.
    Missing(Vec<String>),
    /// The list is malformed.
    Invalid(String),
}

/// A bundle from a client's file list `[{path, sha256, mode}]`, every object already stored.
pub fn bundle_of_objects(store: &Store, listed: &Value) -> Result<Vec<BundleFile>, Unusable> {
    let bad = |m: &str| Unusable::Invalid(m.to_string());
    let listed = listed
        .as_array()
        .filter(|l| !l.is_empty())
        .ok_or_else(|| bad("files must be a non-empty list of {path, sha256, mode}"))?;
    let mut files = Vec::with_capacity(listed.len());
    let mut missing = Vec::new();
    for f in listed {
        let path = f
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| util::plain_path(p));
        let sha = f
            .get("sha256")
            .and_then(Value::as_str)
            .filter(|s| util::is_sha256(s));
        let mode = match f.get("mode").map(Value::as_u64) {
            None => Some(0o644),
            Some(Some(m)) if m == 0o644 || m == 0o755 => u32::try_from(m).ok(),
            Some(_) => None,
        };
        let (Some(path), Some(sha), Some(mode)) = (path, sha, mode) else {
            return Err(bad(
                "every file needs a plain relative path, a sha256 and mode 420 or 493",
            ));
        };
        if files.iter().any(|b: &BundleFile| b.path == path) {
            return Err(Unusable::Invalid(format!("{path} is listed twice")));
        }
        let Some(bytes) = store.object_len(sha) else {
            if !missing.iter().any(|m| m == sha) {
                missing.push(sha.to_string());
            }
            continue;
        };
        files.push(BundleFile {
            path: path.to_string(),
            sha256: sha.to_string(),
            bytes,
            mode,
        });
    }
    if !missing.is_empty() {
        return Err(Unusable::Missing(missing));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// An earlier job's bundle, to offer again.
pub fn bundle_of_job(db: &Connection, job_id: &str) -> Result<Vec<BundleFile>, rusqlite::Error> {
    let mut stmt = db.prepare(
        "SELECT path, sha256, bytes, mode FROM job_files WHERE job_id = ?1 ORDER BY path",
    )?;
    stmt.query_map(params![job_id], |r| {
        Ok(BundleFile {
            path: r.get(0)?,
            sha256: r.get(1)?,
            bytes: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
            mode: r.get(3)?,
        })
    })?
    .collect()
}

/// Presence alone matters, just as in the client's legacy Run again fallback.
fn missing_check(files: &[BundleFile], task: &str) -> Option<String> {
    let path = format!("{}/check", task.trim_end_matches('/'));
    (!files.iter().any(|f| f.path == path)).then_some(path)
}

/// Whether the spec explicitly names the selected task.
fn spec_names_task(spec: &[u8], task: &str) -> bool {
    let Ok(doc) = serde_yaml::from_slice::<serde_yaml::Value>(spec) else {
        return false;
    };
    doc.get("tasks")
        .and_then(serde_yaml::Value::as_sequence)
        .is_some_and(|tasks| {
            tasks.iter().any(|entry| {
                entry
                    .as_str()
                    .is_some_and(|path| path.trim_end_matches('/') == task.trim_end_matches('/'))
            })
        })
}

/// A job checked and ready to record: its node exists, its bundle holds its spec, task and grant.
#[derive(Debug)]
pub struct Plan {
    node_id: i64,
    spec: String,
    task: String,
    trials: u32,
    grant: Option<String>,
    grant_sha: Option<String>,
    meta: Map<String, Value>,
    files: Vec<BundleFile>,
    /// The bundle's `bundle_sha256`, as given (before any reservation is written into its spec).
    pub digest: String,
    scope: crate::reservations::JobScope,
}

impl Plan {
    /// Check a job against its node and bundle.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: &Connection,
        node: &str,
        spec: &str,
        task: &str,
        trials: u32,
        grant: Option<&str>,
        meta: Map<String, Value>,
        files: Vec<BundleFile>,
    ) -> Result<Self, String> {
        let node_id: i64 = db
            .query_row(
                "SELECT id FROM nodes WHERE name = ?1 AND revoked_at IS NULL",
                params![node],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no live node named {node}"))?;
        let has = |p: &str| files.iter().any(|f| f.path == p);
        if !has(spec) {
            return Err(format!("the bundle has no {spec}"));
        }
        let task = task.trim_end_matches('/').to_string();
        if !util::plain_path(&task) || !has(&format!("{task}/goal.md")) {
            return Err(format!("the bundle has no {task}/goal.md"));
        }
        let grant_sha = match grant {
            Some(g) => Some(
                files
                    .iter()
                    .find(|f| f.path == format!("grants/{g}.yaml"))
                    .map(|f| f.sha256.clone())
                    .ok_or_else(|| format!("the bundle has no grants/{g}.yaml"))?,
            ),
            None => None,
        };
        Ok(Self {
            node_id,
            spec: spec.to_string(),
            task,
            trials: trials.max(1),
            grant: grant.map(String::from),
            grant_sha,
            meta,
            digest: bundle_sha(&files),
            files,
            scope: crate::reservations::JobScope::default(),
        })
    }

    /// The budgets the job falls under (its project's, its conversation's).
    #[must_use]
    pub fn under(mut self, scope: crate::reservations::JobScope) -> Self {
        self.scope = scope;
        self
    }

    /// Uploaded bundles receive a default check only when their spec names the task.
    pub fn spec_names_task(&self, store: &Store) -> bool {
        self.files
            .iter()
            .find(|f| f.path == self.spec)
            .and_then(|f| store.read_object(&f.sha256))
            .is_some_and(|bytes| spec_names_task(&bytes, &self.task))
    }

    /// Metadata after Director-owned defaults have been applied.
    pub fn meta(&self) -> &Map<String, Value> {
        &self.meta
    }

    /// Fill the form's empty check for uploaded bundles and `from_job` compatibility.
    pub fn fill_missing_check(&mut self, store: &Store) -> Result<(), String> {
        if let Some(path) = missing_check(&self.files, &self.task) {
            const NO_CHECK: &[u8] =
                b"#!/bin/sh\n# no check: the form's check was empty, so every run passes\nexit 0\n";
            store.put_object(NO_CHECK)?;
            self.files.push(BundleFile {
                path,
                sha256: sha256_hex(NO_CHECK),
                bytes: NO_CHECK.len() as u64,
                mode: 0o755,
            });
            self.files.sort_by(|a, b| a.path.cmp(&b.path));
            self.digest = bundle_sha(&self.files);
            self.meta.insert("check_added".into(), json!("none"));
        }
        Ok(())
    }

    /// Write the intent it carries out into its spec (`stamp.rs`); `replaceable` is the intent a `from_job` bundle's
    /// spec was stamped with. The intent keeps `digest`, the bundle as given; the job's hash covers the stamp.
    pub fn stamp(
        &mut self,
        store: &Store,
        intent: &str,
        replaceable: Option<&str>,
    ) -> Result<(), crate::stamp::Refused> {
        crate::stamp::intent(store, &mut self.files, &self.spec, intent, replaceable)
    }

    /// Record the job inside the caller's transaction: offered, with any reservation written into its spec, or
    /// `waiting` when a budget has no room (`reservations.rs`). Returns its id.
    pub fn insert(
        &self,
        store: &Store,
        tx: &Connection,
        intent: Option<&str>,
    ) -> Result<String, String> {
        let job_id = util::ulid()?;
        let mut files = self.files.clone();
        let placed = crate::reservations::place_new(
            store,
            tx,
            &self.scope,
            &self.spec,
            self.trials,
            &mut files,
        )?;
        let state = if placed.waits() { "waiting" } else { "offered" };
        tx.execute(
            "INSERT INTO jobs (job_id, node_id, spec, task, trials, bundle_sha256, grant_id, grant_sha256, meta, state,
                               created_at, intent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![job_id, self.node_id, self.spec, self.task, self.trials, bundle_sha(&files), self.grant,
                    self.grant_sha, Value::Object(self.meta.clone()).to_string(), state, now_utc(), intent],
        )
        .map_err(|e| e.to_string())?;
        record_files(tx, &job_id, &files).map_err(|e| e.to_string())?;
        crate::reservations::record(tx, &job_id, &placed)?;
        touch(tx, Record::Job, &job_id).map_err(|e| e.to_string())?;
        Ok(job_id)
    }
}

/// Record a job's bundle files.
fn record_files(
    tx: &Connection,
    job_id: &str,
    files: &[BundleFile],
) -> Result<(), rusqlite::Error> {
    for f in files {
        tx.execute(
            "INSERT INTO job_files (job_id, path, sha256, bytes, mode) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                job_id,
                f.path,
                f.sha256,
                i64::try_from(f.bytes).unwrap_or(i64::MAX),
                f.mode
            ],
        )?;
    }
    Ok(())
}

/// A job's bundle files in `GET jobs` form, sorted by path.
pub fn files(db: &Connection, job_id: &str) -> Result<Vec<Value>, rusqlite::Error> {
    let mut stmt = db.prepare(
        "SELECT path, sha256, bytes, mode FROM job_files WHERE job_id = ?1 ORDER BY path",
    )?;
    stmt.query_map(params![job_id], |r| {
        Ok(
            json!({"path": r.get::<_, String>(0)?, "sha256": r.get::<_, String>(1)?,
                  "bytes": r.get::<_, i64>(2)?, "mode": r.get::<_, i64>(3)?}),
        )
    })?
    .collect()
}

/// This node's offered and accepted jobs, oldest first, with Director-owned turn identity.
pub fn for_node(db: &Connection, node_id: i64) -> Result<Vec<Value>, rusqlite::Error> {
    let mut stmt = db.prepare(
        "SELECT job_id, spec, task, trials, bundle_sha256, grant_id, grant_sha256, meta FROM jobs
         WHERE node_id = ?1 AND state IN ('offered', 'accepted') ORDER BY rowid",
    )?;
    let rows: Vec<(String, Value)> = stmt
        .query_map(params![node_id], |r| {
            let meta: String = r.get(7)?;
            Ok((
                r.get::<_, String>(0)?,
                json!({"job_id": r.get::<_, String>(0)?, "spec": r.get::<_, String>(1)?,
                       "task": r.get::<_, String>(2)?, "trials": r.get::<_, i64>(3)?,
                       "bundle_sha256": r.get::<_, String>(4)?,
                       "grant": r.get::<_, Option<String>>(5)?, "grant_sha256": r.get::<_, Option<String>>(6)?,
                       "meta": serde_json::from_str::<Value>(&meta).ok().filter(Value::is_object)
                           .unwrap_or_else(|| json!({}))}),
            ))
        })?
        .collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(id, mut job)| {
            job["files"] = Value::Array(files(db, &id)?);
            // Intent metadata is caller-controlled; only the turns table grants turn identity.
            let turn: Option<(String, String)> = db
                .query_row(
                    "SELECT conversation_id, turn_id FROM turns WHERE job_id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((conversation_id, turn_id)) = turn {
                job["conversation_id"] = json!(conversation_id);
                job["turn_id"] = json!(turn_id);
            }
            Ok(job)
        })
        .collect()
}

/// The outcome of an accept (§5).
pub enum Accept {
    /// 200 with the job's state.
    State(String),
    /// 404: not this node's job.
    NotFound,
    /// 409: another bundle.
    Conflict,
    /// 422: no `bundle_sha256` string.
    Invalid,
}

/// Accept a job: offered → accepted the first time the node names its bundle; a repeat changes nothing.
pub fn accept(
    db: &Connection,
    node_id: i64,
    job_id: &str,
    body: &[u8],
) -> Result<Accept, rusqlite::Error> {
    let job: Option<(String, String)> = db
        .query_row(
            "SELECT bundle_sha256, state FROM jobs WHERE job_id = ?1 AND node_id = ?2",
            params![job_id, node_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((digest, state)) = job else {
        return Ok(Accept::NotFound);
    };
    let body: Value =
        serde_json::from_slice(if body.is_empty() { b"{}" } else { body }).unwrap_or(Value::Null);
    let Some(want) = body.get("bundle_sha256").and_then(Value::as_str) else {
        return Ok(Accept::Invalid);
    };
    if want != digest {
        return Ok(Accept::Conflict);
    }
    if state == "offered" {
        db.execute(
            "UPDATE jobs SET state = 'accepted', accepted_at = ?1 WHERE job_id = ?2 AND state = 'offered'",
            params![now_utc(), job_id],
        )?;
        touch(db, Record::Job, job_id)?;
        return Ok(Accept::State("accepted".into()));
    }
    Ok(Accept::State(state))
}

/// Whether an object is a file of one of this node's job bundles, in any state (§6).
pub fn node_may_read(db: &Connection, node_id: i64, sha: &str) -> Result<bool, rusqlite::Error> {
    Ok(db
        .query_row(
            "SELECT 1 FROM job_files f JOIN jobs j ON j.job_id = f.job_id WHERE f.sha256 = ?1 AND j.node_id = ?2 LIMIT 1",
            params![sha, node_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Res = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn task_detection_uses_the_specs_task_list() {
        for spec in [
            "tasks: [tasks/t]",
            "tasks:\n  - tasks/t/\n",
            "tasks: [tasks/other, tasks/t]",
        ] {
            assert!(spec_names_task(spec.as_bytes(), "tasks/t/"));
        }
        for spec in [
            "id: t",
            "tasks: []",
            "tasks: tasks/t",
            "tasks: [tasks/other]",
            "tasks: [null, 42]",
            "[",
        ] {
            assert!(!spec_names_task(spec.as_bytes(), "tasks/t"));
        }
    }

    #[test]
    fn missing_check_matches_only_the_selected_task() {
        let files = vec![BundleFile {
            path: "tasks/other/check".into(),
            sha256: "a".repeat(64),
            bytes: 0,
            mode: 0o644,
        }];
        assert_eq!(
            missing_check(&files, "tasks/t/"),
            Some("tasks/t/check".into())
        );
        assert_eq!(missing_check(&files, "tasks/other"), None);
        assert_eq!(missing_check(&[], "tasks/t"), Some("tasks/t/check".into()));
    }

    #[test]
    fn bundle_hash_uses_octal_modes() {
        let f = BundleFile {
            path: "tasks/t/check".into(),
            sha256: "a".repeat(64),
            bytes: 1,
            mode: 0o755,
        };
        assert_eq!(
            bundle_sha(std::slice::from_ref(&f)),
            sha256_hex(format!("tasks/t/check:{}:755\n", "a".repeat(64)).as_bytes())
        );
    }

    #[test]
    fn create_offers_a_bundle() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        crate::access::register_node(&store, "node-a")?;
        let bundle = tmp.path().join("bundle");
        std::fs::create_dir_all(bundle.join("tasks/t"))?;
        std::fs::write(bundle.join("spec.yaml"), "id: x\n")?;
        std::fs::write(bundle.join("tasks/t/goal.md"), "Say done.\n")?;
        std::fs::write(bundle.join("tasks/t/check"), "#!/bin/sh\ntrue\n")?;
        std::fs::set_permissions(
            bundle.join("tasks/t/check"),
            std::fs::Permissions::from_mode(0o755),
        )?;
        let mut job = NewJob {
            node: "node-a".into(),
            bundle: bundle.clone(),
            trials: 1,
            ..NewJob::default()
        };
        let made = create(&store, &job)?;
        assert_eq!(made["files"], 3);
        let jobs = for_node(&store.db(), 1)?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["task"], "tasks/t");
        assert_eq!(jobs[0]["meta"], json!({}));
        assert_eq!(jobs[0]["files"][1]["path"], "tasks/t/check");
        assert_eq!(jobs[0]["files"][1]["mode"], 493);
        let job_id = jobs[0]["job_id"].as_str().ok_or("missing job id")?;
        let db = store.db();
        db.execute(
            "UPDATE jobs SET meta = ?1 WHERE job_id = ?2",
            params![
                json!({"conversation_id": "forged", "turn_id": "forged"}).to_string(),
                job_id
            ],
        )?;
        let forged = for_node(&db, 1)?;
        assert!(forged[0].get("conversation_id").is_none());
        assert!(forged[0].get("turn_id").is_none());
        db.execute(
            "INSERT INTO conversations (conversation_id, node, spec, spec_version, model, instructions_tokens,
                                        tools, created_by, created_at)
             VALUES ('conversation-real', 'node-a', 'spec.yaml', 'v1', 'local-coder', 0, 0, 'cred', ?1)",
            params![now_utc()],
        )?;
        db.execute(
            "INSERT INTO turns (turn_id, conversation_id, seq, message, state, posted_by, posted_at, job_id)
             VALUES ('turn-real', 'conversation-real', 1, 'hello', 'running', 'cred', ?1, ?2)",
            params![now_utc(), job_id],
        )?;
        let real = for_node(&db, 1)?;
        assert_eq!(real[0]["conversation_id"], "conversation-real");
        assert_eq!(real[0]["turn_id"], "turn-real");
        drop(db);
        job.node = "nobody".into();
        assert!(create(&store, &job).is_err(), "unknown node");
        std::os::unix::fs::symlink("/etc/passwd", bundle.join("link"))?;
        job.node = "node-a".into();
        assert!(create(&store, &job).is_err(), "symlinks are refused");
        Ok(())
    }
}
