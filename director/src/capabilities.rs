//! Whether a node's reported status fits a job's spec `model` (executor template or harness).

use crate::jobs::BundleFile;
use crate::store::{Record, Store, touch};
use crate::util;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::PathBuf;

type Res<T> = Result<T, String>;

#[derive(Debug, Clone)]
pub struct CapabilityWait {
    pub reason: &'static str,
    pub detail: String,
}

const TEMPLATES_PATH: &str = "ops/bin/executor-templates.json";

pub(crate) fn templates_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CS_CODE_ROOT") {
        let root = PathBuf::from(p);
        let path = root.join(TEMPLATES_PATH);
        if path.is_file() {
            return path;
        }
        // Also accept a repository or release root containing the kit.
        return root.join("cs").join(TEMPLATES_PATH);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(release) = exe.parent().and_then(std::path::Path::parent)
    {
        let path = release.join("cs").join(TEMPLATES_PATH);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../cs")
        .join(TEMPLATES_PATH)
}

pub(crate) fn templates() -> serde_json::Map<String, Value> {
    std::fs::read(templates_path())
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

pub fn is_cli_template(model: &str) -> bool {
    let templates = templates();
    if templates.contains_key(model) {
        return true;
    }
    let Some((name, selected)) = model.split_once(':') else {
        return false;
    };
    templates.get(name).is_some_and(|template| {
        template["model_flag"].is_string()
            && template["models"].get(selected).is_some()
            && template["args"].is_array()
    })
}

pub fn model_in_spec(store: &Store, files: &[BundleFile], spec_path: &str) -> Option<String> {
    let sha = files.iter().find(|f| f.path == spec_path)?.sha256.clone();
    let bytes = store.read_object(&sha)?;
    serde_yaml::from_slice::<serde_yaml::Value>(&bytes)
        .ok()?
        .get("model")
        .and_then(serde_yaml::Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
}

pub(crate) fn node_status(db: &Connection, node_id: i64) -> Option<Value> {
    let raw: Option<String> = db
        .query_row(
            "SELECT status FROM nodes WHERE id = ?1",
            params![node_id],
            |r| r.get(0),
        )
        .optional()
        .ok()?
        .flatten();
    raw.and_then(|s| serde_json::from_str(&s).ok())
}

pub(crate) fn base_template(value: &str) -> &str {
    value.split_once(':').map_or(value, |(name, _)| name)
}

pub(crate) fn reports_template(model: &str, status: &Value) -> bool {
    status["agent_capabilities"]["templates"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|v| v == model))
}

pub(crate) fn template_ready(model: &str, status: &Value) -> bool {
    let now = util::now_utc();
    reports_template(model, status)
        && !status["provider_limits"].as_array().is_some_and(|rows| {
            rows.iter().any(|r| {
                r["template"].as_str() == Some(model)
                    && r["resets_at"].as_str().is_none_or(|at| at > now.as_str())
            })
        })
}

fn harness_ready(status: &Value) -> bool {
    status["agent_capabilities"]
        .get("harness")
        .and_then(Value::as_bool)
        .is_some_and(|v| v)
}

pub fn validate_harness_model(model: &str, status: &Value, node: &str) -> Res<()> {
    if is_cli_template(model) {
        return Err(format!(
            "gateway model {model} conflicts with an executor template"
        ));
    }
    if !harness_ready(status) {
        return Err(format!(
            "node {node} cannot run the native harness for model {model}"
        ));
    }
    let catalog = status["agent_capabilities"]["harness_models"]
        .as_array()
        .ok_or_else(|| format!("node {node} has not reported its harness model catalog"))?;
    if !catalog.iter().any(|row| row.as_str() == Some(model)) {
        return Err(format!(
            "unknown harness model {model} in node {node}'s gateway catalog"
        ));
    }
    Ok(())
}

/// A node can run this spec model when its last status fits. With no `agent_capabilities`, nothing is assumed.
pub fn can_run(model: &str, status: &Value) -> bool {
    let Some(caps) = status.get("agent_capabilities") else {
        return false;
    };
    if !caps.is_object() {
        return false;
    }
    if is_cli_template(model) {
        template_ready(base_template(model), status)
    } else {
        harness_ready(status)
    }
}

fn capability_known(status: &Value) -> bool {
    status
        .get("agent_capabilities")
        .is_some_and(Value::is_object)
}

/// Refuse client submission when the named node's status already rules the model out.
pub fn refuse_dispatch(db: &Connection, node: &str, model: &str, harness: bool) -> Res<()> {
    let node_id: i64 = db
        .query_row(
            "SELECT id FROM nodes WHERE name = ?1 AND revoked_at IS NULL",
            params![node],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no live node named {node}"))?;
    if harness {
        return validate_harness_model(
            model,
            &node_status(db, node_id).unwrap_or(Value::Null),
            node,
        );
    }
    let Some(status) = node_status(db, node_id) else {
        return Ok(());
    };
    if !capability_known(&status) {
        return Ok(());
    }
    if can_run(model, &status) {
        return Ok(());
    }
    if is_cli_template(model) {
        Err(format!("node {node} cannot run executor template {model}"))
    } else {
        Err(format!(
            "node {node} cannot run the native harness for model {model}"
        ))
    }
}

pub fn capability_wait(
    store: &Store,
    db: &Connection,
    node_id: i64,
    files: &[BundleFile],
    spec_path: &str,
    node_name: &str,
) -> Option<CapabilityWait> {
    let model = model_in_spec(store, files, spec_path)?;
    let status = node_status(db, node_id)?;
    if !capability_known(&status) || can_run(&model, &status) {
        return None;
    }
    let detail = if is_cli_template(&model) {
        format!("no node can run executor template {model}; {node_name} does not report it")
    } else {
        format!(
            "no node can run the native harness for model {model}; {node_name} does not report harness"
        )
    };
    Some(CapabilityWait {
        reason: "executor_unavailable",
        detail,
    })
}

/// A new job its node cannot run waits for executor fit. Its budget row keeps its state (`held` or `free`): what it
/// reserved still counts against its budgets, and `reservations::release` (which offers only `waiting` rows) leaves
/// it alone.
pub fn hold_capability(tx: &Connection, job_id: &str, wait: &CapabilityWait) -> Result<(), String> {
    tx.execute(
        "UPDATE jobs SET state = 'waiting', offered_at = NULL WHERE job_id = ?1",
        params![job_id],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE job_budgets SET reason = ?1, detail = ?2 WHERE job_id = ?3",
        params![wait.reason, wait.detail, job_id],
    )
    .map_err(|e| e.to_string())?;
    touch(tx, Record::Job, job_id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Offer jobs that were waiting only for executor fit, each in its own transaction, with the reservation they
/// already hold (it never stopped counting, so no budget is checked again). Returns how many were offered; a job
/// that fails is rolled back on its own and its error returned once the rest are done.
pub fn offer_waiting(
    store: &Store,
    db: &mut Connection,
    node_id: i64,
    node_name: &str,
) -> Res<usize> {
    let Some(status) = node_status(db, node_id) else {
        return Ok(0);
    };
    if !capability_known(&status) {
        return Ok(0);
    }
    let mut stmt = db
        .prepare_cached(
            "SELECT j.job_id, j.spec, b.trials, b.reserved_micros
             FROM jobs j JOIN job_budgets b ON b.job_id = j.job_id
             WHERE j.node_id = ?1 AND j.state = 'waiting' AND b.state IN ('held', 'free')
               AND b.reason = 'executor_unavailable' ORDER BY j.rowid",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, u32, i64)> = stmt
        .query_map(params![node_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    let mut offered = 0;
    let mut failed = Vec::new();
    for (job_id, spec_path, trials, reserved) in rows {
        let mut one = || -> Res<bool> {
            let tx = crate::store::write_tx(db).map_err(|e| e.to_string())?;
            // Still waiting for executor fit, now the transaction holds the lock.
            let still = tx
                .query_row(
                    "SELECT 1 FROM jobs j JOIN job_budgets b ON b.job_id = j.job_id
                     WHERE j.job_id = ?1 AND j.state = 'waiting' AND b.state IN ('held', 'free')
                       AND b.reason = 'executor_unavailable'",
                    params![job_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|e| e.to_string())?
                .is_some();
            let sent = still
                && crate::reservations::offer_job(
                    store, &tx, &job_id, &spec_path, trials, reserved,
                )?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(sent)
        };
        match one() {
            Ok(sent) => offered += usize::from(sent),
            Err(e) => {
                crate::logf!(
                    "job {job_id}: not offered after {node_name}'s capability report: {e}"
                );
                failed.push(format!("{job_id}: {e}"));
            }
        }
    }
    if failed.is_empty() {
        Ok(offered)
    } else {
        Err(format!("offered {offered}; failed {}", failed.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cli_template_needs_templates_list() {
        let status = json!({"agent_capabilities": {"templates": ["codex"], "harness": false}});
        assert!(can_run("codex", &status));
        assert!(!can_run("claude-code", &status));
    }

    #[test]
    fn harness_model_needs_harness_flag() {
        let status = json!({"agent_capabilities": {"templates": ["codex"], "harness": true}});
        assert!(can_run("local-coder", &status));
        let none = json!({"agent_capabilities": {"templates": ["codex"], "harness": false}});
        assert!(!can_run("local-coder", &none));
    }

    #[test]
    fn provider_limits_block_templates() {
        let status = json!({
            "agent_capabilities": {"templates": ["codex"], "harness": false},
            "provider_limits": [{"template": "codex", "resets_at": "2099-01-01T00:00:00Z"}]
        });
        assert!(!can_run("codex", &status));
    }
}
