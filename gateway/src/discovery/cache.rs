//! The model cache: each endpoint's last successful discovery, kept across assemblies and
//! catalog revisions. A failed discovery never erases it; a valid empty list replaces it. It is
//! reported, never routed on: availability follows the latest attempt.

use super::health::Failure;
use crate::log::rfc3339;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

#[derive(Debug, Clone)]
struct Observation {
    models: Vec<String>,
    observed_at: DateTime<Utc>,
    revision: String,
    last_error: String,
    last_failed: bool,
}

/// The process-long cache.
#[derive(Debug, Default)]
pub struct Cache {
    obs: Mutex<BTreeMap<String, Observation>>,
}

impl Cache {
    /// Record one endpoint's outcome.
    pub fn observe(&self, id: &str, result: &Result<Vec<String>, Failure>, revision: &str) {
        let mut obs = self.obs.lock().unwrap_or_else(PoisonError::into_inner);
        match result {
            Err(f) => {
                if let Some(o) = obs.get_mut(id) {
                    o.last_failed = true;
                    o.last_error.clone_from(&f.message);
                }
            }
            Ok(ids) => {
                let mut models = ids.clone();
                models.sort();
                obs.insert(
                    id.to_string(),
                    Observation {
                        models,
                        observed_at: Utc::now(),
                        revision: revision.to_string(),
                        last_error: String::new(),
                        last_failed: false,
                    },
                );
            }
        }
    }

    /// Every observation evaluated at `now`: fresh, stale (last attempt failed) or expired.
    pub fn report(&self, now: DateTime<Utc>, stale_after: Duration) -> serde_json::Value {
        let obs = self.obs.lock().unwrap_or_else(PoisonError::into_inner);
        let stale_after = chrono::Duration::from_std(stale_after).unwrap_or(chrono::Duration::MAX);
        let views: Vec<serde_json::Value> = obs
            .iter()
            .map(|(id, o)| {
                let state = if now >= o.observed_at + stale_after {
                    "expired"
                } else if o.last_failed {
                    "stale"
                } else {
                    "fresh"
                };
                let mut v = serde_json::json!({
                    "source": id,
                    "models": o.models,
                    "observed_at": rfc3339(o.observed_at),
                    "catalog_revision": o.revision,
                    "state": state,
                });
                if !o.last_error.is_empty() {
                    v["last_error"] = serde_json::json!(o.last_error);
                }
                v
            })
            .collect();
        serde_json::Value::Array(views)
    }
}
