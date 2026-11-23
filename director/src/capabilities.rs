//! Whether a node's reported status fits a job's spec `model` (executor template or harness).

use crate::jobs::BundleFile;
use crate::store::{Record, Store, touch};
use crate::util;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

type Res<T> = Result<T, String>;

#[derive(Debug, Clone)]
pub struct CapabilityWait {
    pub reason: &'static str,
    pub detail: String,
}

const TEMPLATES_PATH: &str = "ops/bin/executor-templates.json";

fn templates_path() -> PathBuf {
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

fn templates() -> HashSet<String> {
    let path = templates_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return HashSet::new();
    };
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(&bytes) else {
        return HashSet::new();
    };
    m.keys().cloned().collect()
}

pub fn is_cli_template(model: &str) -> bool {
    templates().contains(model)
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

fn template_ready(model: &str, status: &Value) -> bool {
    let now = util::now_utc();
    status["agent_capabilities"]
        .get("templates")
        .and_then(Value::as_array)
        .is_some_and(|rows| rows.iter().any(|v| v.as_str() == Some(model)))
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
        template_ready(model, status)
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

pub fn hold_capability(tx: &Connection, job_id: &str, wait: &CapabilityWait) -> Result<(), String> {
    tx.execute(
        "UPDATE jobs SET state = 'waiting' WHERE job_id = ?1",
        params![job_id],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE job_budgets SET state = 'waiting', reason = ?1, detail = ?2 WHERE job_id = ?3",
        params![wait.reason, wait.detail, job_id],
    )
    .map_err(|e| e.to_string())?;
    touch(tx, Record::Job, job_id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Offer jobs that were waiting only for executor fit. Returns how many were offered.
pub fn offer_waiting(store: &Store, db: &Connection, node_id: i64, node_name: &str) -> Res<usize> {
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
             WHERE j.node_id = ?1 AND j.state = 'waiting' AND b.state = 'waiting'
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
    let mut offered = 0;
    for (job_id, spec_path, trials, reserved) in rows {
        let files = crate::jobs::bundle_of_job(db, &job_id).map_err(|e| e.to_string())?;
        let Some(model) = model_in_spec(store, &files, &spec_path) else {
            continue;
        };
        if !can_run(&model, &status) {
            let detail = if is_cli_template(&model) {
                format!("no node can run executor template {model}; {node_name} does not report it")
            } else {
                format!(
                    "no node can run the native harness for model {model}; {node_name} does not report harness"
                )
            };
            db.execute(
                "UPDATE job_budgets SET detail = ?1 WHERE job_id = ?2",
                params![detail, job_id],
            )
            .map_err(|e| e.to_string())?;
            continue;
        }
        crate::reservations::offer_job(store, db, &job_id, &spec_path, trials, reserved)?;
        offered += 1;
    }
    Ok(offered)
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
