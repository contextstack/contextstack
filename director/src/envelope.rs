//! `POST envelopes` (NODE-PROTOCOL.md §7): judge a node's `run_finalized` or `job_terminal` event and
//! answer `acked`, `missing`, `rejected` or `conflict`, always as a 200 body.
//!
//! Evidence is hashed and verified before the database lock; settlement runs in one `write_tx`.

use crate::store::{Record, Store, touch, write_tx};
use crate::util::{self, is_sha256, now_utc, plain_path, sha256_hex};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

#[cfg(test)]
mod test_seam {
    use std::sync::atomic::{AtomicBool, Ordering};

    static PAUSE_VERIFY: AtomicBool = AtomicBool::new(false);
    static VERIFYING: AtomicBool = AtomicBool::new(false);
    static GO: AtomicBool = AtomicBool::new(false);

    /// When armed, the first hashed object spins until [`release_verify_pause`].
    pub fn arm_verify_pause() {
        PAUSE_VERIFY.store(true, Ordering::SeqCst);
        GO.store(false, Ordering::SeqCst);
        VERIFYING.store(false, Ordering::SeqCst);
    }

    pub fn release_verify_pause() {
        GO.store(true, Ordering::SeqCst);
    }

    pub fn disarm_verify_pause() {
        PAUSE_VERIFY.store(false, Ordering::SeqCst);
        GO.store(true, Ordering::SeqCst);
        VERIFYING.store(false, Ordering::SeqCst);
    }

    pub fn wait_until_verifying() {
        while !VERIFYING.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }

    pub(crate) fn mark_verifying() {
        VERIFYING.store(true, Ordering::SeqCst);
    }

    pub(crate) fn pause_verify_mid() {
        if !PAUSE_VERIFY.load(Ordering::SeqCst) {
            return;
        }
        while !GO.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }
}

/// The kind of event an envelope reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    RunFinalized,
    JobTerminal,
}

/// One listed evidence file.
#[derive(Debug, Clone)]
struct File {
    path: String,
    sha256: String,
    bytes: u64,
}

/// An envelope that passed the shape checks (§7.1, §7.2 step 1).
#[derive(Debug)]
struct Parsed<'a> {
    kind: Kind,
    subject: String,
    event_id: String,
    files: Vec<File>,
    envelope_sha256: String,
    row_sha256: Option<String>,
    row_line: Option<&'a str>,
    usage: Option<&'a Value>,
    /// A run's job, when the node names it (`run_finalized` with `job_id`).
    job: Option<&'a str>,
}

/// Run evidence judged before the database lock.
#[derive(Debug)]
struct VerifiedRun {
    row: Option<Map<String, Value>>,
}

/// A `job_terminal` row judged before the lock.
#[derive(Debug)]
struct VerifiedJob {
    row: Map<String, Value>,
}

/// Everything settled under the database lock, after [`prepare`].
#[derive(Debug)]
enum Prepared<'a> {
    Run(&'a Parsed<'a>, VerifiedRun),
    Job(&'a Parsed<'a>, VerifiedJob),
}

enum Prepare<'a> {
    Ready(Prepared<'a>),
    Missing(BTreeSet<String>),
}

/// Judge one `POST envelopes` body for a node.
pub fn receive(store: &Store, node_id: i64, body: &[u8]) -> Value {
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let parsed = match parse(body.get("envelope"), body.get("row_line")) {
        Ok(p) => p,
        Err(e) => return rejected(&e),
    };
    match earlier_ack(store, &parsed.event_id) {
        Ok(Some((owner, sha, ack))) => {
            match repeat_answer(
                &parsed.event_id,
                node_id,
                owner,
                &sha,
                &parsed.envelope_sha256,
                &ack,
            ) {
                Ok(answer) => return answer,
                Err(e) => return rejected(&e),
            }
        }
        Ok(None) => {}
        Err(e) => return rejected(&e),
    }
    let prepared = match prepare(store, &parsed) {
        Ok(Prepare::Ready(p)) => p,
        Ok(Prepare::Missing(m)) => return json!({"status": "missing", "missing": m}),
        Err(e) => return rejected(&e),
    };
    let mut db = store.db();
    let tx = match write_tx(&mut db) {
        Ok(tx) => tx,
        Err(e) => return rejected(&format!("database: {e}")),
    };
    match settle(store, &tx, node_id, prepared) {
        Ok(answer) => {
            if answer["status"] == "acked"
                && let Err(e) = tx.commit()
            {
                return rejected(&format!("database: {e}"));
            }
            answer
        }
        Err(e) => rejected(&e),
    }
}

fn rejected(error: &str) -> Value {
    json!({"status": "rejected", "error": error})
}

/// A settled envelope, if any.
fn earlier_ack(
    store: &Store,
    event_id: &str,
) -> Result<Option<(i64, String, String)>, String> {
    let db = store.db();
    db.query_row(
        "SELECT node_id, envelope_sha256, ack FROM envelopes WHERE event_id = ?1",
        params![event_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .map_err(|e| format!("database: {e}"))
}

/// §7: a repeat is judged on shape and `envelope_sha256` only — not `row_line` or the objects again.
fn repeat_answer(
    event_id: &str,
    node_id: i64,
    owner: i64,
    sha: &str,
    want: &str,
    ack: &str,
) -> Result<Value, String> {
    if owner != node_id || sha != want {
        return Ok(json!({
            "status": "conflict",
            "error": format!("{event_id} is already acked with envelope_sha256 {sha}")
        }));
    }
    let ack: Value = serde_json::from_str(ack).map_err(|e| e.to_string())?;
    Ok(json!({"status": "acked", "ack": ack}))
}


fn parse<'a>(
    envelope: Option<&'a Value>,
    row_line: Option<&'a Value>,
) -> Result<Parsed<'a>, String> {
    let env = envelope
        .and_then(Value::as_object)
        .ok_or("envelope must be a JSON object")?;
    if env.get("envelope_version").and_then(Value::as_u64) != Some(1) {
        return Err("envelope_version must be 1".into());
    }
    let event_id = env
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (kind, subject) = match event_id.split_once(':') {
        Some(("run_finalized", s)) if !s.is_empty() && !s.contains('\n') => (Kind::RunFinalized, s),
        Some(("job_terminal", s)) if !s.is_empty() && !s.contains('\n') => (Kind::JobTerminal, s),
        _ => return Err("event_id must be run_finalized:<run> or job_terminal:<job>".into()),
    };
    let envelope_sha256 = env
        .get("envelope_sha256")
        .and_then(Value::as_str)
        .filter(|s| is_sha256(s))
        .ok_or("envelope_sha256 must be a sha256")?;
    let row_sha256 = match env.get("row_sha256") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if is_sha256(s) => Some(s.clone()),
        Some(_) => return Err("row_sha256 must be null or a sha256".into()),
    };
    let row_line = match row_line {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err("row_line must be a string or null".into()),
    };
    let mut usage = None;
    let mut job = None;
    if kind == Kind::RunFinalized {
        job = match env.get("job_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(j)) if !j.is_empty() => Some(j.as_str()),
            Some(_) => return Err("job_id must be a job id or null".into()),
        };
        let plain = util::plain_name(subject, usize::MAX) && !subject.contains("..");
        if env.get("run_id").and_then(Value::as_str) != Some(subject) || !plain {
            return Err("run_id must equal the event suffix and be a plain name".into());
        }
        check_usage(env.get("usage"))?;
        usage = env.get("usage");
    } else if env.contains_key("job_id") && env["job_id"].as_str() != Some(subject) {
        return Err("job_id must equal the event suffix".into());
    }
    let files = parse_files(env)?;
    Ok(Parsed {
        kind,
        subject: subject.to_string(),
        event_id: event_id.to_string(),
        files,
        envelope_sha256: envelope_sha256.to_string(),
        row_sha256,
        row_line,
        usage,
        job,
    })
}

fn parse_files(env: &Map<String, Value>) -> Result<Vec<File>, String> {
    let listed = env
        .get("files")
        .and_then(Value::as_array)
        .ok_or("files must be a list")?;
    let mut seen = BTreeSet::new();
    let mut files = Vec::with_capacity(listed.len());
    for f in listed {
        let path = f
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| plain_path(p));
        let sha = f
            .get("sha256")
            .and_then(Value::as_str)
            .filter(|s| is_sha256(s));
        let bytes = f.get("bytes").and_then(Value::as_u64);
        let (Some(path), Some(sha), Some(bytes)) = (path, sha, bytes) else {
            return Err("every file needs a plain relative path, a sha256 and bytes".into());
        };
        if !seen.insert(path) {
            return Err(format!("{path} is listed twice"));
        }
        files.push(File {
            path: path.to_string(),
            sha256: sha.to_string(),
            bytes,
        });
    }
    Ok(files)
}

/// §7.2 usage: six required keys, each a non-negative figure or null, and a `{class, model}` source.
fn check_usage(usage: Option<&Value>) -> Result<(), String> {
    let u = usage
        .and_then(Value::as_object)
        .ok_or("usage must be an object")?;
    for k in ["input_tokens", "output_tokens"] {
        if !u.get(k).is_some_and(|v| v.is_null() || v.is_u64()) {
            return Err(format!("usage.{k} must be a non-negative integer or null"));
        }
    }
    for k in ["est_usd", "charged_usd"] {
        if !u
            .get(k)
            .is_some_and(|v| v.is_null() || v.as_f64().is_some_and(|n| n >= 0.0))
        {
            return Err(format!("usage.{k} must be a non-negative number or null"));
        }
    }
    if !u
        .get("est_basis")
        .is_some_and(|v| v.is_null() || v.is_string())
    {
        return Err("usage.est_basis must be a string or null".into());
    }
    let source_ok = u.get("source").and_then(Value::as_object).is_some_and(|s| {
        s.get("class").is_some_and(|c| {
            c.is_null() || matches!(c.as_str(), Some("api" | "subscription" | "free_local"))
        }) && s.get("model").is_some_and(|m| m.is_null() || m.is_string())
    });
    if !source_ok {
        return Err("usage.source must be {class, model}".into());
    }
    Ok(())
}

/// sha256 of `path:sha256\n` over the files sorted by path.
fn envelope_sha(files: &[File]) -> String {
    let sorted: BTreeMap<&str, &str> = files
        .iter()
        .map(|f| (f.path.as_str(), f.sha256.as_str()))
        .collect();
    let text = sorted.iter().fold(String::new(), |mut out, (p, s)| {
        let _ = writeln!(out, "{p}:{s}");
        out
    });
    sha256_hex(text.as_bytes())
}

/// The row, once its line hashes to `row_sha256`; `None` when the envelope carries no row.
fn row(p: &Parsed) -> Result<Option<Map<String, Value>>, String> {
    let Some(want) = &p.row_sha256 else {
        return Ok(None);
    };
    let line = p
        .row_line
        .filter(|l| sha256_hex(l.as_bytes()) == *want)
        .ok_or("row_line is missing or does not hash to row_sha256")?;
    match serde_json::from_str(line) {
        Ok(Value::Object(row)) => Ok(Some(row)),
        _ => Err("row_line is not a JSON object".into()),
    }
}

fn missing_shas(store: &Store, files: &[File]) -> BTreeSet<String> {
    files
        .iter()
        .filter(|f| !store.has_object(&f.sha256))
        .map(|f| f.sha256.clone())
        .collect()
}

/// §7.2 steps 2–4: presence (first pass), then read, hash and shape-check every object — without the database lock.
fn prepare<'a>(store: &Store, p: &'a Parsed<'a>) -> Result<Prepare<'a>, String> {
    match p.kind {
        Kind::RunFinalized => {
            let missing = missing_shas(store, &p.files);
            if !missing.is_empty() {
                return Ok(Prepare::Missing(missing));
            }
            let row = prepare_run(store, p)?;
            Ok(Prepare::Ready(Prepared::Run(p, VerifiedRun { row })))
        }
        Kind::JobTerminal => {
            if !p.files.is_empty() {
                return Err("a job_terminal envelope carries no files".into());
            }
            if envelope_sha(&p.files) != p.envelope_sha256 {
                return Err("envelope_sha256 does not match the listed files".into());
            }
            let row = row(p)?.ok_or("row_sha256 is required")?;
            Ok(Prepare::Ready(Prepared::Job(p, VerifiedJob { row })))
        }
    }
}

/// Read and hash every listed object; check envelope sha, row, terminal and restorable rules.
fn prepare_run(store: &Store, p: &Parsed) -> Result<Option<Map<String, Value>>, String> {
    #[cfg(test)]
    test_seam::mark_verifying();
    let mut terminal = None;
    for f in &p.files {
        let data = store.read_object(&f.sha256).unwrap_or_default();
        #[cfg(test)]
        test_seam::pause_verify_mid();
        if sha256_hex(&data) != f.sha256 || data.len() as u64 != f.bytes {
            return Err(format!(
                "object {} for {} does not verify",
                f.sha256, f.path
            ));
        }
        if f.path == "terminal.json" {
            terminal = serde_json::from_slice::<Value>(&data).ok();
        }
    }
    if envelope_sha(&p.files) != p.envelope_sha256 {
        return Err("envelope_sha256 does not match the listed files".into());
    }
    let row = row(p)?;
    if terminal
        .as_ref()
        .and_then(|t| t.get("run_id"))
        .and_then(Value::as_str)
        != Some(&p.subject)
    {
        return Err(format!(
            "terminal.json is missing or does not name run {}",
            p.subject
        ));
    }
    let _ = restorable(p)?;
    Ok(row)
}

fn settle(store: &Store, tx: &Transaction, node_id: i64, prep: Prepared<'_>) -> Result<Value, String> {
    let (kind, event_id, envelope_sha256, subject, files) = match prep {
        Prepared::Run(p, _) => (
            Kind::RunFinalized,
            p.event_id.clone(),
            p.envelope_sha256.clone(),
            p.subject.clone(),
            p.files.clone(),
        ),
        Prepared::Job(p, _) => (
            Kind::JobTerminal,
            p.event_id.clone(),
            p.envelope_sha256.clone(),
            p.subject.clone(),
            p.files.clone(),
        ),
    };
    let db = |e: rusqlite::Error| format!("database: {e}");
    let earlier: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT node_id, envelope_sha256, ack FROM envelopes WHERE event_id = ?1",
            params![event_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(db)?;
    if let Some((owner, sha, ack)) = earlier {
        if owner != node_id || sha != envelope_sha256 {
            return Ok(json!({"status": "conflict",
                "error": format!("{event_id} is already acked with envelope_sha256 {sha}")}));
        }
        let ack: Value = serde_json::from_str(&ack).map_err(|e| e.to_string())?;
        return Ok(json!({"status": "acked", "ack": ack}));
    }
    match prep {
        Prepared::Run(p, verified) => {
            let missing = missing_shas(store, &files);
            if !missing.is_empty() {
                return Ok(json!({"status": "missing", "missing": missing}));
            }
            record_run(tx, node_id, p, &verified)?;
        }
        Prepared::Job(p, verified) => record_terminal(tx, node_id, p, &verified)?,
    }
    // The job's reservation settles on its actual charge; then a conversation turn or summary carried by this job
    // (or run) may be settled and its queue moved; then waiting jobs get what was released.
    crate::reservations::after_envelope(tx, kind == Kind::JobTerminal, &subject);
    crate::settle::after_envelope(store, tx, kind == Kind::JobTerminal, &subject);
    crate::reservations::release_logged(store, tx, &subject);
    let ack = json!({"event_id": event_id, "envelope_sha256": envelope_sha256,
                     "verified_at": now_utc(), "restored": true});
    tx.execute(
        "INSERT INTO envelopes (event_id, node_id, envelope_sha256, ack, received_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![event_id, node_id, envelope_sha256, ack.to_string(), now_utc()],
    )
    .map_err(db)?;
    Ok(json!({"status": "acked", "ack": ack}))
}

/// §7.2 step 4: the files a restored run needs are listed. Returns path → sha256.
fn restorable<'p>(p: &'p Parsed) -> Result<BTreeMap<&'p str, &'p str>, String> {
    let listed: BTreeMap<&str, &str> = p
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.sha256.as_str()))
        .collect();
    for need in ["transcript.md", "check.json"] {
        if !listed.contains_key(need) {
            return Err(format!("{need} is missing"));
        }
    }
    if listed.keys().any(|k| k.starts_with("stimulus/"))
        && !listed.contains_key("stimulus/manifest.json")
    {
        return Err("stimulus/manifest.json is missing".into());
    }
    Ok(listed)
}

/// §7.2 step 5: record the run once objects are already verified.
fn record_run(
    tx: &Transaction,
    node_id: i64,
    p: &Parsed,
    verified: &VerifiedRun,
) -> Result<(), String> {
    let db = |e: rusqlite::Error| format!("database: {e}");
    let listed = restorable(p)?;
    let mut stmt = tx
        .prepare("SELECT path, sha256 FROM run_files WHERE run_id = ?1")
        .map_err(db)?;
    let before: Vec<(String, String)> = stmt
        .query_map(params![p.subject], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(db)?
        .collect::<Result<_, _>>()
        .map_err(db)?;
    if before
        .iter()
        .any(|(path, sha)| listed.get(path.as_str()).is_some_and(|s| s != sha))
    {
        return Err(format!(
            "run {} already exists and differs from the envelope",
            p.subject
        ));
    }
    let row = &verified.row;
    let transcript = row
        .as_ref()
        .and_then(|r| r.get("transcript"))
        .and_then(Value::as_str);
    // The row is recorded once per transcript, as the ledger appends it once.
    let duplicate_row = match transcript {
        Some(t) => tx
            .query_row(
                "SELECT 1 FROM runs WHERE transcript = ?1",
                params![t],
                |_| Ok(()),
            )
            .optional()
            .map_err(db)?
            .is_some(),
        None => false,
    };
    let keep_row = row.is_some() && !duplicate_row;
    let verdict = row
        .as_ref()
        .and_then(|r| r.get("verdict"))
        .and_then(Value::as_str);
    let job = named_job(tx, node_id, p.job).map_err(db)?;
    tx.execute(
        "INSERT INTO runs (run_id, node_id, usage, row_line, transcript, verdict, received_at, job_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(run_id) DO UPDATE SET usage = excluded.usage, job_id = COALESCE(runs.job_id, excluded.job_id)",
        params![
            p.subject,
            node_id,
            p.usage.map(Value::to_string).unwrap_or_default(),
            keep_row.then_some(p.row_line).flatten(),
            keep_row.then_some(transcript).flatten(),
            verdict,
            now_utc(),
            job
        ],
    )
    .map_err(db)?;
    record_files(tx, p).map_err(db)?;
    touch(tx, Record::Run, &p.subject).map_err(db)?;
    if let Some(j) = &job {
        touch(tx, Record::Job, j).map_err(db)?;
    }
    Ok(())
}

/// A run's evidence files, once each.
fn record_files(tx: &Transaction, p: &Parsed) -> rusqlite::Result<()> {
      for f in &p.files {
        tx.execute(
            "INSERT OR IGNORE INTO run_files (run_id, path, sha256, bytes) VALUES (?1, ?2, ?3, ?4)",
            params![
                p.subject,
                f.path,
                f.sha256,
                i64::try_from(f.bytes).unwrap_or(i64::MAX)
            ],
        )?;
    }
    Ok(())
}

/// The job a run's report names, when it is one of this node's jobs: it links the run to that job (each trial's
/// run). A node's own dispatch id names no Director job and links nothing.
fn named_job(
    tx: &Transaction,
    node_id: i64,
    job: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    let Some(job) = job else {
        return Ok(None);
    };
    tx.query_row(
        "SELECT job_id FROM jobs WHERE job_id = ?1 AND node_id = ?2",
        params![job, node_id],
        |r| r.get(0),
    )
    .optional()
}

/// Store a short reason for the client failure classifier: `reason`, then `stop`, then `error.detail` / `error`, then `note`.
fn terminal_error(row: &Map<String, Value>) -> Option<String> {
    const MAX: usize = 2000;
    let take = |s: &str| -> Option<String> {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.chars().take(MAX).collect())
    };
    let as_str = |v: Option<&Value>| v.and_then(Value::as_str).and_then(take);
    as_str(row.get("reason"))
        .or_else(|| as_str(row.get("stop")))
        .or_else(|| match row.get("error") {
            Some(Value::Object(o)) => as_str(o.get("detail")),
            Some(Value::String(s)) => take(s),
            _ => None,
        })
        .or_else(|| as_str(row.get("note")))
}

/// §7.3: a job of this node is over; it leaves `GET jobs`.
fn record_terminal(
    tx: &Transaction,
    node_id: i64,
    p: &Parsed,
    verified: &VerifiedJob,
) -> Result<(), String> {
    let owned = tx
        .query_row(
            "SELECT 1 FROM jobs WHERE job_id = ?1 AND node_id = ?2",
            params![p.subject, node_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| format!("database: {e}"))?
        .is_some();
    if !owned {
        return Err(format!("job {} is not a job of this node", p.subject));
    }
    let row = &verified.row;
    let had_terminal = tx
        .query_row(
            "SELECT 1 FROM jobs WHERE job_id = ?1 AND terminal_row IS NOT NULL",
            params![p.subject],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| format!("database: {e}"))?
        .is_some();
    let state = if row.get("state").and_then(Value::as_str) == Some("done") {
        "done"
    } else {
        "error"
    };
    let run_id = row.get("run_id").and_then(Value::as_str);
    let error = (state == "error").then(|| terminal_error(row)).flatten();
    tx.execute(
        "UPDATE jobs SET state = ?1, finished_at = ?2, terminal_row = ?3, run_id = COALESCE(?4, run_id),
             error = ?5 WHERE job_id = ?6",
        params![state, now_utc(), p.row_line, run_id, error, p.subject],
    )
    .map_err(|e| format!("database: {e}"))?;
    touch(tx, Record::Job, &p.subject).map_err(|e| format!("database: {e}"))?;
    // A cancel the node never picked up is settled: the job ended first. A duplicate job_terminal must not
    // touch a re-listed cancel.
    if !had_terminal {
        crate::control::settle_on_terminal(tx, &p.subject).map_err(|e| format!("database: {e}"))?;
    }
    // The run now shows which job (and intent) it belongs to.
    if let Some(run) = run_id
        && tx
            .query_row("SELECT 1 FROM runs WHERE run_id = ?1", params![run], |_| {
                Ok(())
            })
            .optional()
            .map_err(|e| format!("database: {e}"))?
            .is_some()
    {
        touch(tx, Record::Run, run).map_err(|e| format!("database: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::register_node;
    use std::sync::{Arc, Mutex};
    use std::thread;

    static SERIAL: Mutex<()> = Mutex::new(());

    fn usage() -> Value {
        json!({"input_tokens": 1, "output_tokens": null, "est_usd": 0.1, "est_basis": "rate",
               "charged_usd": 0, "source": {"class": "api", "model": null}})
    }

    fn test_run_body(run_id: &str, extra: Option<(&str, &[u8])>) -> Result<(Value, Vec<u8>), Box<dyn std::error::Error>> {
        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::from([
            (
                "terminal.json".into(),
                format!(r#"{{"run_id":"{run_id}"}}"#).into_bytes(),
            ),
            ("transcript.md".into(), b"# t\n".to_vec()),
            ("check.json".into(), b"{}".to_vec()),
        ]);
        if let Some((path, data)) = extra {
            files.insert(path.to_string(), data.to_vec());
        }
        let row_line = json!({"run_id": run_id, "transcript": format!("ledger/runs/{run_id}"), "verdict": "pass"})
            .to_string();
        let listed: Vec<Value> = files
            .iter()
            .map(|(path, data)| {
                json!({"path": path, "sha256": sha256_hex(data), "bytes": data.len()})
            })
            .collect();
        let files: Vec<File> = listed
            .iter()
            .map(|f| {
                let (Some(path), Some(sha), Some(bytes)) = (
                    f["path"].as_str(),
                    f["sha256"].as_str(),
                    f["bytes"].as_u64(),
                ) else {
                    panic!("test fixture entry");
                };
                File {
                    path: path.to_string(),
                    sha256: sha.to_string(),
                    bytes,
                }
            })
            .collect();
        let envelope_sha = envelope_sha(&files);
        let envelope = json!({
            "envelope_version": 1,
            "event_id": format!("run_finalized:{run_id}"),
            "run_id": run_id,
            "files": listed,
            "envelope_sha256": envelope_sha,
            "row_sha256": sha256_hex(row_line.as_bytes()),
            "usage": usage(),
        });
        let body = json!({"envelope": envelope, "row_line": row_line});
        let bytes = serde_json::to_vec(&body).map_err(|e| format!("test fixture: {e}"))?;
        Ok((body, bytes))
    }

    #[test]
    fn usage_rules() {
        assert!(check_usage(Some(&usage())).is_ok());
        for (key, bad) in [
            ("input_tokens", json!(-1)),
            ("output_tokens", json!(1.5)),
            ("est_usd", json!("0.1")),
            ("charged_usd", json!(-0.01)),
            ("est_basis", json!(3)),
            ("source", json!({"class": "free", "model": null})),
            ("source", json!({"class": "api"})),
        ] {
            let mut u = usage();
            u[key] = bad;
            assert!(check_usage(Some(&u)).is_err(), "{key}");
        }
        let mut u = usage();
        if let Some(o) = u.as_object_mut() {
            o.remove("est_basis");
        }
        assert!(check_usage(Some(&u)).is_err(), "a key is required");
        assert!(check_usage(Some(&json!([1]))).is_err());
    }

    #[test]
    fn envelope_hash_is_sorted_lines() {
        let f = |p: &str| File {
            path: p.into(),
            sha256: sha256_hex(p.as_bytes()),
            bytes: 1,
        };
        let a = envelope_sha(&[f("b"), f("a")]);
        assert_eq!(a, envelope_sha(&[f("a"), f("b")]));
        assert_eq!(envelope_sha(&[]), sha256_hex(b""));
    }

    #[test]
    fn terminal_error_prefers_reason_then_stop_then_detail() {
        let row = |v: Value| v.as_object().cloned().unwrap_or_default();
        assert_eq!(
            terminal_error(&row(
                json!({"reason": " gateway refused ", "stop": "other"})
            )),
            Some("gateway refused".into())
        );
        assert_eq!(
            terminal_error(&row(
                json!({"stop": "check_error", "error": {"detail": "x"}})
            )),
            Some("check_error".into())
        );
        assert_eq!(
            terminal_error(&row(json!({"error": {"detail": "no provider"}}))),
            Some("no provider".into())
        );
        assert_eq!(
            terminal_error(&row(json!({"note": "preflight refused"}))),
            Some("preflight refused".into())
        );
        assert_eq!(terminal_error(&row(json!({}))), None);
        let long = "x".repeat(2500);
        assert_eq!(
            terminal_error(&row(json!({"reason": long}))).map(|r| r.chars().count()),
            Some(2000)
        );
    }

    /// A large object is hashed before the lock; another caller can use the database meanwhile (`test_seam::arm_verify_pause`).
    #[test]
    fn verification_does_not_hold_the_database_lock() -> Result<(), Box<dyn std::error::Error>> {
        let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir()?;
        let store = Arc::new(Store::open(&tmp.path().join("data"))?);
        register_node(&store, "n1")?;
        let run_id = "run-lock-free";
        let large = vec![b'x'; 4 * 1024 * 1024];
        for (_, data) in [
            ("terminal.json", format!(r#"{{"run_id":"{run_id}"}}"#).into_bytes()),
            ("transcript.md", b"# t\n".to_vec()),
            ("check.json", b"{}".to_vec()),
            ("evidence/large.bin", large.clone()),
        ] {
            store.put_object(&data)?;
        }
        let (_body, bytes) = test_run_body(run_id, Some(("evidence/large.bin", &large)))?;
        test_seam::arm_verify_pause();
        let store_recv = Arc::clone(&store);
        let verifier = thread::spawn(move || receive(&store_recv, 1, &bytes));
        test_seam::wait_until_verifying();
        let db = store.db();
        let n: i64 = db.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        assert_eq!(n, 1, "the database answered while verification was paused");
        drop(db);
        test_seam::release_verify_pause();
        let answer = verifier.join().map_err(|_| "verifier panicked")?;
        assert_eq!(answer["status"], "acked");
        test_seam::disarm_verify_pause();
        Ok(())
    }

    #[test]
    fn concurrent_posts_record_one_envelope() -> Result<(), Box<dyn std::error::Error>> {
        let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir()?;
        let store = Arc::new(Store::open(&tmp.path().join("data"))?);
        register_node(&store, "n1")?;
        let run_id = "run-once";
        for (_, data) in [
            ("terminal.json", format!(r#"{{"run_id":"{run_id}"}}"#).into_bytes()),
            ("transcript.md", b"# t\n".to_vec()),
            ("check.json", b"{}".to_vec()),
        ] {
            store.put_object(&data)?;
        }
        let (_body, bytes) = test_run_body(run_id, None)?;
        let ready = Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let store = Arc::clone(&store);
            let bytes = bytes.clone();
            let ready = Arc::clone(&ready);
            handles.push(thread::spawn(move || {
                ready.wait();
                receive(&store, 1, &bytes)
            }));
        }
        ready.wait();
        let mut answers = Vec::with_capacity(handles.len());
        for h in handles {
            answers.push(match h.join() {
                Ok(v) => v,
                Err(_) => return Err("poster panicked".into()),
            });
        }
        assert!(answers.iter().all(|a| a["status"] == "acked"));
        let n: i64 = store.db().query_row(
            "SELECT COUNT(*) FROM envelopes WHERE event_id = ?1",
            params![format!("run_finalized:{run_id}")],
            |r| r.get(0),
        )?;
        assert_eq!(n, 1);
        Ok(())
    }
}
