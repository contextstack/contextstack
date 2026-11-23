//! Persist the runner's terminal row, usage, and trace identity for finalization.
use super::{Cell, Clock, Utc, Value, json, text, wire_float};
use chrono::SecondsFormat;
use std::{collections::BTreeMap, fs, path::Path};

use crate::process::Control;

impl Cell<'_> {
    pub(super) fn receipt(
        &self,
        clock: &Clock,
        terminal_state: &str,
        control: &Control,
        lane: Option<Value>,
    ) -> Value {
        let ended = Clock::start();
        let (sp, task, trial, started) = (self.spec, self.task, self.trial, clock.0);
        let id = &self.trace.run;
        let mut trace_fields = self.receipt_trace(started);
        let mut state = json!({
            "run_id": id,
            "runner_implementation": "rust",
            "runner_build": self.stimulus["runner_build"],
            "run_dir": self.dir,
            "transcript": format!("ledger/runs/{id}"),
            "workspace": self.workspace,
            "stimulus_dir": self.dir.join("stimulus"),
            "spec": sp.id,
            "spec_sha": sp.sha,
            "spec_path": sp.path,
            "task": task,
            "trial": trial,
            "task_sha": text(&self.stimulus, "task_sha"),
            "env_sha": text(&self.stimulus, "env_sha"),
            "stimulus_sha": text(&self.stimulus, "stimulus_sha"),
            "model": self.model,
            "stop": self.row.stop,
            "verdict": self.row.verdict,
            "turns": self.row.turns,
            "tool_calls": self.row.tool_calls,
            "cost_usd": wire_float(self.row.cost),
            "est_usd": wire_float(self.row.estimated),
            "est_basis": "rate",
            "input_tokens": self.row.tokens[0],
            "output_tokens": self.row.tokens[1],
            "cost_class": self.row.class,
            "terminal_state": terminal_state,
            "cancel_reason": (terminal_state == "cancelled").then(|| control.cause.get().map_or("cancelled", |c| c)),
            "started": started.to_rfc3339_opts(SecondsFormat::Nanos, true),
            "ended": ended.0.to_rfc3339_opts(SecondsFormat::Nanos, true),
            "seconds": wire_float(ended.elapsed_from(clock)),
            "check_window": sp.check_window.max(60)
        });
        if self.metrics.provider_reported_usd > 0.0 {
            let amount = json!(self.metrics.provider_reported_usd);
            state["provider_reported_usd"] = amount.clone();
            trace_fields["identity"]["terminal"]["provider_reported_usd"] = amount;
        }
        if let Some(lane) = lane {
            state["harness_lane"] = lane;
        }
        if let Some(intent) = &sp.intent {
            state["intent"] = json!(intent);
        }
        let trusted_hung = self.row.error.as_deref().is_some_and(|e| {
            ["capture-stimulus:", "setup-workspace:", "hook "]
                .iter()
                .any(|p| e.starts_with(p))
                && (e.contains("deadline") || e.contains("signal:") || e.contains("watchdog"))
        });
        json!({
            "version": 1, "adapter": "harness", "run_id": id,
            "job_id": self.trace.job, "execution_id": null,
            "trial": trial, "spec_id": sp.id, "task_ref": task,
            "task_name": Path::new(task).file_name().unwrap_or_default(),
            "started": started.timestamp(),
            "native": {"stop": state["stop"], "exit": null},
            "stimulus": {"manifest": "stimulus/manifest.json", "sha256": state["stimulus_sha"]},
            "workspace": state["workspace"], "row": state,
            "usage": {
                "turns": state["turns"], "tool_calls": state["tool_calls"],
                "input_tokens": state["input_tokens"], "output_tokens": state["output_tokens"],
                "cost_usd": state["cost_usd"], "est_usd": state["est_usd"], "est_basis": state["est_basis"],
            },
            "finish": {"mode": "harness", "check_window": sp.check_window.max(60), "defer": trusted_hung},
            "sandbox": null, "trace": {"harness_trace": trace_fields},
        })
    }

    fn receipt_trace(&self, started: chrono::DateTime<Utc>) -> Value {
        let deliverables: BTreeMap<_, _> = self
            .deliverables
            .iter()
            .map(|(path, before)| {
                let after = crate::deliverable::stamp(&self.workspace, path);
                (path, after.is_some() && &after != before)
            })
            .collect();
        let mut identity = json!({
            "run_id": self.trace.run, "coverage": "full",
            "terminal": {"turns": self.row.turns, "tool_calls": self.row.tool_calls,
                         "charged_usd": self.row.cost, "accounted_settlements": self.metrics.settlements,
                         "tool_errors": self.metrics.tool_errors, "deliverables": deliverables},
        });
        if self.final_turn_retry {
            identity["terminal"]["final_turn_retry"] = json!(true);
        }
        if let Some(job) = &self.trace.job {
            identity["job_id"] = json!(job);
            identity["task"] = json!(self.task);
            identity["trial"] = json!(self.trial);
        }
        json!({
            "identity": identity,
            "machine": self.trace.machine,
            "started": started.to_rfc3339_opts(SecondsFormat::Micros, true),
        })
    }
}

// The binary retains this lock until process exit. Recovery can see the
// receipt as soon as it is renamed, but cannot finish while execution lives.
pub(super) fn write_receipt(dir: &Path, receipt: &Value) -> std::io::Result<fs::File> {
    use std::io::Write as _;

    let lock = fs::File::create_new(dir.join("execution.lock"))?;
    lock.lock()?;
    let tmp = dir.join("execution-receipt.json.tmp");
    let mut file = fs::File::create_new(&tmp)?;
    file.write_all(receipt.to_string().as_bytes())?;
    file.sync_all()?;
    fs::rename(tmp, dir.join("execution-receipt.json"))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(lock)
}
