//! CLI work orders the gateway is running, by execution id, so a caller can cancel one
//! (`POST /v1/agent/cli.run/{id}/cancel`). A run is also known by its job: the work order's
//! `CS_JOB_ID` (the node's dispatch id, the Director's job id). It names the run when no
//! `execution_id` was given and it is free, and a cancel by job id stops every running trial of it. A run is registered before admission and stays until its
//! supervisor has reaped the process group and dropped the permit; then its outcome is kept among the
//! recently finished, so a late cancel answers `already_finished` rather than 404.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// How many finished runs are remembered.
const KEEP_FINISHED: usize = 1024;
/// How long a cancel waits for the process group to be gone and the permit released.
pub(crate) const CANCEL_WAIT: Duration = crate::workorder::RECEIPT_STOP_GRACE
    .saturating_add(crate::workorder::WAIT_DELAY)
    .saturating_add(Duration::from_secs(5));

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The child exited on its own (any exit code).
    Completed,
    /// Stopped by `POST .../cancel`.
    Cancelled,
    /// The deadline passed, admission refused, the child could not start, or its output never closed.
    Failed,
    /// The caller went away before it ended.
    Abandoned,
}

impl Outcome {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }
}

struct Running {
    cancel: CancellationToken,
    caller: String,
    job: String,
    done: watch::Receiver<bool>,
}

struct Finished {
    outcome: Outcome,
    caller: String,
    job: String,
}

#[derive(Default)]
struct Runs {
    running: HashMap<String, Running>,
    finished: HashMap<String, Finished>,
    order: VecDeque<String>,
}

/// Every run the gateway owns.
#[derive(Default)]
pub struct Registry {
    runs: Arc<Mutex<Runs>>,
    seq: AtomicU64,
}

/// What a cancel request found.
#[derive(Debug, PartialEq, Eq)]
pub enum Cancel {
    /// The run was stopped: its process group is gone and its permit released.
    Cancelled,
    /// It had already ended, with this outcome.
    AlreadyFinished(Outcome),
    /// Stopping is under way but took longer than the cancel waits.
    Cancelling,
    /// No such execution id.
    Unknown,
    /// Started by a different caller.
    NotOwner,
}

/// A registered run. Dropping it records the outcome (`Abandoned` unless one was set) and wakes cancels.
pub struct Handle {
    runs: Arc<Mutex<Runs>>,
    id: String,
    caller: String,
    job: String,
    /// Fired by a cancel request.
    pub cancel: CancellationToken,
    done: watch::Sender<bool>,
    outcome: Outcome,
}

impl Handle {
    /// The execution id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Record how the run ended (applied on drop).
    pub fn set_outcome(&mut self, outcome: Outcome) {
        self.outcome = outcome;
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        {
            let mut r = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            r.running.remove(&self.id);
            if r.finished.contains_key(&self.id) {
                r.order.retain(|i| i != &self.id);
            }
            let done = Finished {
                outcome: self.outcome,
                caller: std::mem::take(&mut self.caller),
                job: std::mem::take(&mut self.job),
            };
            r.finished.insert(self.id.clone(), done);
            r.order.push_back(self.id.clone());
            while r.order.len() > KEEP_FINISHED {
                if let Some(old) = r.order.pop_front() {
                    r.finished.remove(&old);
                }
            }
        }
        let _ = self.done.send(true);
    }
}

/// A caller-chosen id: 1–128 of `A-Z a-z 0-9 . _ : -`.
pub fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

impl Registry {
    /// Register a run under `id`; else under `job` when that is a valid id not running now; else a fresh
    /// id. `Err` when an explicit `id` is running now. `job` (when valid) also names the run for cancel.
    pub fn register(
        &self,
        id: Option<String>,
        job: Option<&str>,
        caller: &str,
    ) -> Result<Handle, String> {
        let job = job.filter(|j| valid_id(j)).unwrap_or("").to_string();
        let mut r = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let id = match id {
            Some(id) if r.running.contains_key(&id) => return Err(id),
            Some(id) => id,
            None if !job.is_empty() && !r.running.contains_key(&job) => job.clone(),
            None => {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis());
                format!("lx-{ms:x}-{}", self.seq.fetch_add(1, Ordering::Relaxed))
            }
        };
        let (tx, rx) = watch::channel(false);
        let cancel = CancellationToken::new();
        r.running.insert(
            id.clone(),
            Running {
                cancel: cancel.clone(),
                caller: caller.to_string(),
                job: job.clone(),
                done: rx,
            },
        );
        Ok(Handle {
            runs: Arc::clone(&self.runs),
            id,
            caller: caller.to_string(),
            job,
            cancel,
            done: tx,
            outcome: Outcome::Abandoned,
        })
    }

    /// Cancel every running run named `id` (its execution id or its job) for `caller`: a run started with
    /// a caller name is cancelled only by the same name. Waits until each run's supervisor has finished
    /// (process group gone, permit released). With none running, the most recent finished one answers.
    pub async fn cancel(&self, id: &str, caller: &str) -> Cancel {
        let waits: Vec<(String, watch::Receiver<bool>)> = {
            let r = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            let hits: Vec<(&String, &Running)> = r
                .running
                .iter()
                .filter(|(k, run)| *k == id || run.job == id)
                .collect();
            if hits.is_empty() {
                let last = r.finished.get(id).or_else(|| {
                    r.order
                        .iter()
                        .rev()
                        .filter_map(|k| r.finished.get(k))
                        .find(|f| f.job == id)
                });
                return match last {
                    None => Cancel::Unknown,
                    Some(f) if !f.caller.is_empty() && f.caller != caller => Cancel::NotOwner,
                    Some(f) => Cancel::AlreadyFinished(f.outcome),
                };
            }
            if hits
                .iter()
                .any(|(_, run)| !run.caller.is_empty() && run.caller != caller)
            {
                return Cancel::NotOwner;
            }
            hits.iter()
                .map(|(k, run)| {
                    run.cancel.cancel();
                    ((*k).clone(), run.done.clone())
                })
                .collect()
        };
        let mut any_cancelled = false;
        for (key, mut done) in waits {
            if tokio::time::timeout(CANCEL_WAIT, done.wait_for(|d| *d))
                .await
                .is_err()
            {
                return Cancel::Cancelling;
            }
            let r = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            any_cancelled |= r
                .finished
                .get(&key)
                .is_none_or(|f| f.outcome == Outcome::Cancelled);
        }
        if any_cancelled {
            Cancel::Cancelled
        } else {
            Cancel::AlreadyFinished(Outcome::Completed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_cancel_and_remember() {
        let reg = Registry::default();
        assert!(valid_id("job-1.a:b_c") && !valid_id("") && !valid_id("a/b"));
        let mut h = reg
            .register(Some("x".into()), None, "node")
            .unwrap_or_else(|_| unreachable!());
        assert!(reg.register(Some("x".into()), None, "node").is_err());
        assert_eq!(reg.cancel("x", "other").await, Cancel::NotOwner);
        assert_eq!(reg.cancel("nope", "node").await, Cancel::Unknown);
        let token = h.cancel.clone();
        let ender = tokio::spawn(async move {
            token.cancelled().await;
            h.set_outcome(Outcome::Cancelled);
            drop(h);
        });
        assert_eq!(reg.cancel("x", "node").await, Cancel::Cancelled);
        let _ = ender.await;
        assert_eq!(
            reg.cancel("x", "node").await,
            Cancel::AlreadyFinished(Outcome::Cancelled)
        );
        let mut done = reg
            .register(None, Some("job-7"), "")
            .unwrap_or_else(|_| unreachable!());
        let id = done.id().to_string();
        assert_eq!(id, "job-7");
        done.set_outcome(Outcome::Completed);
        drop(done);
        assert_eq!(
            reg.cancel(&id, "anyone").await,
            Cancel::AlreadyFinished(Outcome::Completed)
        );
    }
}
