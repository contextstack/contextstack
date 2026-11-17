//! Per-endpoint discovery health. Records are facts; state is evaluated against
//! the clock on every read, so a stopped refresh loop still crosses the freshness threshold. Records
//! persist to a file under the gateway state root and survive a restart.

use crate::executables::Identity;
use crate::log::rfc3339;
use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// The report and file schema.
pub const SCHEMA: &str = "csgateway.discovery_health.v1";

mod rfc {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(t: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&crate::log::rfc3339(*t)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let v: Option<String> = Option::deserialize(d)?;
        Ok(v.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&Utc)))
    }
}

/// One endpoint's recorded history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    /// Endpoint id.
    #[serde(rename = "endpoint_id", default)]
    pub id: String,
    /// refresh | static | disabled | unconfigured.
    #[serde(default)]
    pub mode: String,
    /// Why not refresh.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// When discovery was first expected.
    #[serde(default, with = "rfc", skip_serializing_if = "Option::is_none")]
    pub expected_since: Option<DateTime<Utc>>,
    /// Last attempt.
    #[serde(default, with = "rfc", skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<DateTime<Utc>>,
    /// Last success (never manufactured by a failure).
    #[serde(default, with = "rfc", skip_serializing_if = "Option::is_none")]
    pub last_success: Option<DateTime<Utc>>,
    /// Failures since the last success.
    #[serde(default)]
    pub consecutive_failures: i64,
    /// Last error text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error: String,
    /// request | timeout | status | decode | credential.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error_class: String,
    /// Models in the last successful listing.
    #[serde(default)]
    pub model_count: i64,
}

/// A classed discovery failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// request | timeout | status | decode | credential.
    pub class: &'static str,
    /// Text.
    pub message: String,
}

/// One endpoint's outcome in an assembly.
pub type Outcome = Result<usize, Failure>;

#[derive(Serialize, Deserialize)]
struct File {
    schema: String,
    stale_after_s: i64,
    #[serde(with = "rfc")]
    written_at: Option<DateTime<Utc>>,
    #[serde(default, with = "rfc", skip_serializing_if = "Option::is_none")]
    stopped_at: Option<DateTime<Utc>>,
    #[serde(default)]
    endpoints: Vec<Record>,
    #[serde(default)]
    executables: Vec<Identity2>,
}

/// An executable identity as read back from a file (kept, not interpreted).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Identity2(serde_json::Value);

#[derive(Debug, Default)]
struct Inner {
    endpoints: HashMap<String, Record>,
    exes: Vec<Identity>,
}

/// Discovery health, safe for concurrent assemblies.
#[derive(Debug)]
pub struct Health {
    path: Option<PathBuf>,
    stale_after: Duration,
    disabled: HashSet<String>,
    inner: Mutex<Inner>,
}

impl Health {
    /// Load persisted records; disabled ids must name catalog endpoints.
    pub fn new(
        path: Option<PathBuf>,
        stale_after: Duration,
        disabled: &[String],
        known: &[String],
    ) -> Result<Self, String> {
        let mut set = HashSet::new();
        for id in disabled {
            if !known.contains(id) {
                return Err(format!(
                    "CSGATEWAY_DISCOVERY_DISABLED: unknown endpoint {id:?}"
                ));
            }
            set.insert(id.clone());
        }
        let mut inner = Inner::default();
        if let Some(p) = &path {
            match std::fs::read(p) {
                Ok(raw) => match serde_json::from_slice::<File>(&raw) {
                    Ok(f) if f.schema == SCHEMA => {
                        for r in f.endpoints {
                            inner.endpoints.insert(r.id.clone(), r);
                        }
                    }
                    _ => crate::logf!(
                        "warning: discovery health {} not {SCHEMA}, starting fresh",
                        p.display()
                    ),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => crate::logf!(
                    "warning: discovery health {} unreadable, starting fresh: {e}",
                    p.display()
                ),
            }
        }
        Ok(Self {
            path,
            stale_after,
            disabled: set,
            inner: Mutex::new(inner),
        })
    }

    /// The freshness threshold.
    pub fn stale_after(&self) -> Duration {
        self.stale_after
    }

    /// Whether host config turned discovery off for an id.
    pub fn disabled(&self, id: &str) -> bool {
        self.disabled.contains(id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Apply one assembly; anything not named is no longer configured.
    pub fn record(
        &self,
        attempted: &HashMap<String, Outcome>,
        static_: &HashMap<String, String>,
        unconfigured: &HashMap<String, String>,
        exes: Vec<Identity>,
    ) {
        let now = Utc::now().trunc_subsecs(0);
        let mut inner = self.lock();
        let mut next = HashMap::new();
        for (id, outcome) in attempted {
            let mut rec = inner
                .endpoints
                .get(id)
                .filter(|r| r.mode == "refresh")
                .cloned()
                .unwrap_or_else(|| Record {
                    id: id.clone(),
                    mode: "refresh".into(),
                    expected_since: Some(now),
                    ..Record::default()
                });
            rec.reason.clear();
            rec.last_attempt = Some(now);
            match outcome {
                Ok(n) => {
                    rec.last_success = Some(now);
                    rec.consecutive_failures = 0;
                    rec.last_error.clear();
                    rec.last_error_class.clear();
                    rec.model_count = i64::try_from(*n).unwrap_or(i64::MAX);
                }
                Err(f) => {
                    rec.consecutive_failures += 1;
                    rec.last_error.clone_from(&f.message);
                    rec.last_error_class = f.class.into();
                }
            }
            next.insert(id.clone(), rec);
        }
        for (mode, reasons) in [("static", static_), ("unconfigured", unconfigured)] {
            for (id, reason) in reasons {
                next.insert(
                    id.clone(),
                    Record {
                        id: id.clone(),
                        mode: mode.into(),
                        reason: reason.clone(),
                        ..Record::default()
                    },
                );
            }
        }
        for id in &self.disabled {
            next.insert(
                id.clone(),
                Record {
                    id: id.clone(),
                    mode: "disabled".into(),
                    reason: "CSGATEWAY_DISCOVERY_DISABLED".into(),
                    ..Record::default()
                },
            );
        }
        inner.endpoints = next;
        inner.exes = exes;
        self.persist(&inner, now, None);
    }

    /// Record a clean shutdown.
    pub fn stop(&self) {
        let now = Utc::now().trunc_subsecs(0);
        let inner = self.lock();
        self.persist(&inner, now, Some(now));
    }

    fn sorted(inner: &Inner) -> Vec<Record> {
        let map: BTreeMap<&String, &Record> = inner.endpoints.iter().collect();
        map.into_values().cloned().collect()
    }

    fn persist(&self, inner: &Inner, now: DateTime<Utc>, stopped: Option<DateTime<Utc>>) {
        let Some(path) = &self.path else { return };
        let file = serde_json::json!({
            "schema": SCHEMA,
            "stale_after_s": self.stale_after.as_secs(),
            "written_at": rfc3339(now),
            "endpoints": Self::sorted(inner),
            "executables": inner.exes,
        });
        let mut file = file;
        if let Some(s) = stopped {
            file["stopped_at"] = serde_json::json!(rfc3339(s));
        }
        let text = serde_json::to_string_pretty(&file).unwrap_or_default() + "\n";
        if let Err(e) = write_atomic(path, text.as_bytes()) {
            crate::logf!("warning: discovery health write {}: {e}", path.display());
        }
    }

    /// GET /v1/discovery/health, evaluated at `now`.
    pub fn report(&self, now: DateTime<Utc>) -> serde_json::Value {
        let inner = self.lock();
        let stale_after =
            chrono::Duration::from_std(self.stale_after).unwrap_or(chrono::Duration::MAX);
        let mut endpoints = Vec::new();
        let mut conditions = Vec::new();
        for rec in Self::sorted(&inner) {
            let mut view = serde_json::to_value(&rec).unwrap_or_default();
            let mut state = rec.mode.clone();
            if rec.mode == "refresh" {
                let since = rec
                    .last_success
                    .or(rec.expected_since)
                    .map(|b| b + stale_after);
                if let Some(since) = since {
                    view["stale_since"] = serde_json::json!(rfc3339(since));
                }
                state = match since {
                    Some(since) if now >= since => {
                        let mut c = serde_json::json!({
                            "kind": "discovery_stale",
                            "id": format!("discovery-stale-{}", rec.id),
                            "endpoint_id": rec.id,
                            "first_observed": rfc3339(since),
                            "age_s": (now - since).num_seconds(),
                            "consecutive_failures": rec.consecutive_failures,
                            "recovery": format!(
                                "bring {} back (it clears on its next successful discovery), or set CSGATEWAY_DISCOVERY_DISABLED={}",
                                rec.id, rec.id
                            ),
                        });
                        if let Some(ls) = rec.last_success {
                            c["last_success"] = serde_json::json!(rfc3339(ls));
                        }
                        if !rec.last_error.is_empty() {
                            c["last_error"] = serde_json::json!(rec.last_error);
                            c["last_error_class"] = serde_json::json!(rec.last_error_class);
                        }
                        conditions.push(c);
                        "stale".into()
                    }
                    _ if rec.last_success.is_none() => "unknown".into(),
                    _ => "fresh".into(),
                };
            }
            view["state"] = serde_json::json!(state);
            endpoints.push(view);
        }
        serde_json::json!({
            "schema": SCHEMA,
            "stale_after_s": self.stale_after.as_secs(),
            "evaluated_at": rfc3339(now),
            "endpoints": endpoints,
            "conditions": conditions,
            "executables": inner.exes,
        })
    }

    /// Raised condition ids (for /v1/health).
    pub fn stale_ids(&self, now: DateTime<Utc>) -> Vec<String> {
        let r = self.report(now);
        r["conditions"]
            .as_array()
            .map(|cs| {
                cs.iter()
                    .filter_map(|c| c["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::DirBuilderExt;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = dir.join(format!(
        ".discovery-health-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let result = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    if let Err(e) = result.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_from_a_persisted_record_is_evaluated_on_read() {
        let Ok(dir) = tempfile::tempdir() else { return };
        let path = dir.path().join("h.json");
        let old = "2026-01-01T00:00:00Z";
        let body = serde_json::json!({"schema": SCHEMA, "stale_after_s": 60, "written_at": old, "endpoints": [
            {"endpoint_id": "vllm", "mode": "refresh", "expected_since": old, "last_attempt": old, "last_success": old,
             "consecutive_failures": 0, "model_count": 5}], "executables": []});
        assert!(std::fs::write(&path, body.to_string()).is_ok());
        let Ok(h) = Health::new(Some(path), Duration::from_secs(3600), &[], &[]) else {
            return;
        };
        let mut attempted = HashMap::new();
        attempted.insert(
            "vllm".to_string(),
            Err(Failure {
                class: "status",
                message: "x".into(),
            }),
        );
        h.record(&attempted, &HashMap::new(), &HashMap::new(), vec![]);
        let r = h.report(Utc::now());
        assert_eq!(r["conditions"][0]["first_observed"], "2026-01-01T01:00:00Z");
        assert_eq!(r["conditions"][0]["consecutive_failures"], 1);
    }
}
