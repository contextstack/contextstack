//! Deployment admission: model calls and Lab work orders share capacity declared in
//! models.json. A [`Permit`] is owned by the request it admits and released exactly once, on drop, on
//! every path: success, error, panic, cancellation by a dropped request, a stream's end. A queued
//! caller holds a [`QueueSlot`] that leaves the queue when dropped, so a disconnect while queued frees
//! its place at once.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// One deployment: model names on one provider bound to a physical serving instance.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Deployment {
    /// Identity.
    #[serde(default)]
    pub id: String,
    /// Reported basis.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub capacity_basis: String,
    /// The serving endpoint (must match the adapter's).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    /// Catalog endpoint id, or `cli` for work orders.
    #[serde(default)]
    pub provider: String,
    /// Bound model names (or executors).
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// Shared-host key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// Estate shares of `max_concurrent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<BTreeMap<String, i64>>,
    /// Concurrency.
    #[serde(default)]
    pub max_concurrent: i64,
    /// Queue length.
    #[serde(default)]
    pub max_queued: i64,
    /// Queue deadline.
    #[serde(default)]
    pub max_wait_ms: i64,
}

/// The admission section of models.json (echoed on /v1/admission as `configuration`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    /// This node's estate.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub estate: String,
    /// The Lab work-order script.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lab_executor: String,
    /// Host-configured executable locations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executables: Option<BTreeMap<String, String>>,
    /// Deployments.
    #[serde(default)]
    pub deployments: Option<Vec<Deployment>>,
    /// Shared-host capacities.
    #[serde(default)]
    pub deployment_hosts: Option<BTreeMap<String, i64>>,
}

impl Config {
    /// Read models.json's admission section; a missing file is the empty configuration.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(raw) => {
                serde_json::from_slice(&raw).map_err(|e| format!("decode deployment config: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("read deployment config: {e}")),
        }
    }

    /// Append host-overlay deployments and `deployment_hosts` (already collision-checked at catalog load).
    pub fn merge_overlay(&mut self, cand: &crate::catalog::Candidate) {
        if !cand.overlay_deployments.is_empty() {
            let deps = self.deployments.get_or_insert_with(Vec::new);
            deps.extend(cand.overlay_deployments.iter().cloned());
        }
        if !cand.overlay_deployment_hosts.is_empty() {
            let hosts = self.deployment_hosts.get_or_insert_with(BTreeMap::new);
            for (k, v) in &cand.overlay_deployment_hosts {
                hosts.insert(k.clone(), *v);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Limit {
    host: String,
    max_concurrent: i64,
    max_queued: i64,
    max_wait: Option<Duration>,
}

/// Live state of one deployment.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct State {
    /// Admitted and running.
    pub active: i64,
    /// Waiting.
    pub queued: i64,
}

#[derive(Debug, Default)]
struct Counts {
    states: HashMap<String, State>,
    host_active: HashMap<String, i64>,
}

/// A refusal: `queue_full`, `queue_timeout`, `deployment_unbound`, or the caller went away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

/// The process-long capacity owner, shared by every catalog revision.
#[derive(Debug)]
pub struct Manager {
    estate: String,
    limits: HashMap<String, Limit>,
    host_caps: HashMap<String, i64>,
    bindings: HashMap<String, HashMap<String, String>>,
    provider_endpoints: HashMap<String, String>,
    configuration: serde_json::Value,
    config_sha: String,
    counts: Mutex<Counts>,
    changed: Notify,
}

/// This estate's concurrency for a deployment: its share when capacity is split, else `max_concurrent`.
fn concurrency(d: &Deployment, estate: &str) -> Result<i64, String> {
    let Some(shares) = &d.capacity else {
        return Ok(d.max_concurrent);
    };
    let mut total = 0;
    for (name, share) in shares {
        if name.trim().is_empty() || *share <= 0 || *share > d.max_concurrent - total {
            return Err(format!("invalid estate capacity allocation: {}", d.id));
        }
        total += share;
    }
    let concurrent = shares.get(estate).copied().unwrap_or(0);
    if estate.is_empty() || concurrent <= 0 {
        return Err(format!(
            "deployment {} has no capacity for estate {estate:?}",
            d.id
        ));
    }
    Ok(concurrent)
}

/// Deployment identity: which deployment serves each (provider, model), and each provider's one endpoint.
#[derive(Default)]
struct Bindings {
    models: HashMap<String, HashMap<String, String>>,
    endpoint_owner: HashMap<String, String>,
    endpoint_provider: HashMap<String, String>,
    provider_endpoints: HashMap<String, String>,
}

impl Bindings {
    fn add(&mut self, d: &Deployment) -> Result<(), String> {
        let models = d.models.as_deref().unwrap_or_default();
        if d.provider.is_empty()
            || models.is_empty()
            || d.max_wait_ms < 0
            || d.max_wait_ms > i64::MAX / 1_000_000
        {
            return Err(format!(
                "invalid deployment binding or queue deadline: {}",
                d.id
            ));
        }
        if !d.endpoint.is_empty() {
            let ep = normalize_endpoint(&d.endpoint)
                .map_err(|e| format!("deployment {} endpoint: {e}", d.id))?;
            if self.endpoint_owner.get(&ep).is_some_and(|o| *o != d.id) {
                return Err("one serving endpoint must have one deployment identity".into());
            }
            if self
                .endpoint_provider
                .get(&ep)
                .is_some_and(|p| *p != d.provider)
            {
                return Err("endpoint assigned to different providers".into());
            }
            if self
                .provider_endpoints
                .get(&d.provider)
                .is_some_and(|p| *p != ep)
            {
                return Err(format!("provider {} spans different endpoints", d.provider));
            }
            self.endpoint_owner.insert(ep.clone(), d.id.clone());
            self.endpoint_provider
                .insert(ep.clone(), d.provider.clone());
            self.provider_endpoints.insert(d.provider.clone(), ep);
        }
        let bound = self.models.entry(d.provider.clone()).or_default();
        for m in models {
            if m.is_empty() || bound.contains_key(m) {
                return Err(format!(
                    "duplicate or empty deployment model: {}/{m}",
                    d.provider
                ));
            }
            bound.insert(m.clone(), d.id.clone());
        }
        Ok(())
    }
}

/// Normalise an endpoint for identity comparison: absolute http(s), no credentials, query or fragment.
pub fn normalize_endpoint(raw: &str) -> Result<String, String> {
    let u = crate::catalog::plain_http_url(raw)
        .ok_or("expected an absolute HTTP endpoint without credentials, query or fragment")?;
    Ok(u.as_str().trim_end_matches('/').to_string())
}

impl Manager {
    /// Validate and build. Errors are the startup refusal's text.
    pub fn new(c: &Config) -> Result<Self, String> {
        let mut b = Bindings::default();
        let mut limits = HashMap::new();
        let mut counts = Counts::default();
        let mut order = Vec::new();
        for d in c.deployments.iter().flatten() {
            let concurrent = concurrency(d, &c.estate)?;
            b.add(d)?;
            order.push((d, concurrent));
        }
        let mut host_caps = HashMap::new();
        for (host, n) in c.deployment_hosts.iter().flatten() {
            if *n <= 0 {
                return Err(format!("invalid host capacity: {host}"));
            }
            host_caps.insert(host.clone(), *n);
        }
        for (d, max_concurrent) in order {
            let max_wait = u64::try_from(d.max_wait_ms)
                .ok()
                .filter(|w| *w > 0)
                .map(Duration::from_millis);
            if d.id.is_empty()
                || limits.contains_key(&d.id)
                || max_concurrent <= 0
                || d.max_queued < 0
                || (d.max_queued > 0 && max_wait.is_none())
                || (!d.host.is_empty() && host_caps.get(&d.host).copied().unwrap_or(0) <= 0)
            {
                return Err(format!("invalid deployment limit: {}", d.id));
            }
            counts.states.insert(d.id.clone(), State::default());
            let limit = Limit {
                host: d.host.clone(),
                max_concurrent,
                max_queued: d.max_queued,
                max_wait,
            };
            limits.insert(d.id.clone(), limit);
        }
        let configuration = serde_json::to_value(c).map_err(|e| e.to_string())?;
        let raw = serde_json::to_vec(&configuration).map_err(|e| e.to_string())?;
        Ok(Self {
            estate: c.estate.clone(),
            limits,
            host_caps,
            bindings: b.models,
            provider_endpoints: b.provider_endpoints,
            configuration,
            config_sha: crate::log::sha256_hex(&raw),
            counts: Mutex::new(counts),
            changed: Notify::new(),
        })
    }

    /// Whether admission is configured (any deployment turns it on for every provider).
    pub fn configured(&self) -> bool {
        !self.bindings.is_empty()
    }

    /// Check that each bound provider's deployment endpoint is the adapter's actual endpoint.
    /// `adapters` maps provider id to its verifiable endpoint (`None`: registered without one).
    /// `keyless` names keyed catalog providers left unregistered because this host has no credential
    /// store (`CSGATEWAY_CREDENTIALS=off`, as on a node): their deployments stay bound, so a call to one
    /// of their models is refused, but they do not stop startup. Any other unregistered provider does.
    pub fn verify(
        &self,
        adapters: &HashMap<String, Option<String>>,
        keyless: &std::collections::HashSet<String>,
    ) -> Result<(), String> {
        let mut names: Vec<&String> = self.bindings.keys().filter(|n| *n != "cli").collect();
        names.sort();
        for name in names {
            let Some(actual) = adapters.get(name) else {
                if keyless.contains(name) {
                    continue;
                }
                return Err(format!("deployment provider {name} is not configured"));
            };
            let declared = self.provider_endpoints.get(name);
            let (Some(declared), Some(actual)) = (declared, actual) else {
                return Err(format!(
                    "deployment provider {name} has no verifiable endpoint"
                ));
            };
            if normalize_endpoint(actual).ok().as_ref() != Some(declared) {
                return Err(format!(
                    "deployment provider {name} endpoint differs from configured adapter"
                ));
            }
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admit one call. `None` when admission is not configured (nothing to hold).
    pub async fn acquire(
        self: &Arc<Self>,
        binding: &str,
        model: &str,
        scope: Scope,
        request_id: &str,
    ) -> Result<Option<Permit>, Refusal> {
        if !self.configured() {
            return Ok(None);
        }
        let started = Instant::now();
        let Some(id) = self
            .bindings
            .get(binding)
            .and_then(|b| b.get(model))
            .cloned()
        else {
            return Err(Refusal(format!("deployment_unbound: {binding}/{model}")));
        };
        let Some(limit) = self.limits.get(&id).cloned() else {
            return Err(Refusal(format!("unknown_deployment: {id}")));
        };
        let wait_until = limit.max_wait.map(|w| tokio::time::Instant::now() + w);
        let mut slot: Option<QueueSlot> = None;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut c = self.lock();
                let host_free = limit.host.is_empty()
                    || c.host_active.get(&limit.host).copied().unwrap_or(0)
                        < self.host_caps.get(&limit.host).copied().unwrap_or(0);
                let st = c.states.entry(id.clone()).or_default();
                if st.active < limit.max_concurrent && host_free {
                    st.active += 1;
                    if !limit.host.is_empty() {
                        *c.host_active.entry(limit.host.clone()).or_default() += 1;
                    }
                    drop(c);
                    if let Some(s) = slot.take() {
                        s.leave();
                    }
                    return Ok(Some(Permit {
                        manager: Arc::clone(self),
                        id,
                        host: limit.host,
                        model: model.to_string(),
                        request_id: request_id.to_string(),
                        scope,
                        queue_wait: started.elapsed(),
                        admitted: Instant::now(),
                    }));
                }
                if slot.is_none() {
                    if st.queued >= limit.max_queued {
                        return Err(Refusal(format!("queue_full: {id}")));
                    }
                    st.queued += 1;
                    slot = Some(QueueSlot {
                        manager: Arc::clone(self),
                        id: id.clone(),
                        left: false,
                    });
                }
            }
            match wait_until {
                Some(t) => {
                    if tokio::time::timeout_at(t, notified).await.is_err() {
                        return Err(Refusal(format!("queue_timeout: {id}")));
                    }
                }
                None => notified.await,
            }
        }
    }

    /// GET /v1/admission.
    pub fn report(&self) -> serde_json::Value {
        let capacity: BTreeMap<&String, i64> = self
            .limits
            .iter()
            .map(|(k, l)| (k, l.max_concurrent))
            .collect();
        let states: BTreeMap<String, State> = self
            .lock()
            .states
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        serde_json::json!({
            "estate": self.estate,
            "capacity": capacity,
            "config_sha": self.config_sha,
            "configuration": self.configuration,
            "deployments": states,
        })
    }

    /// This estate and per-deployment capacity (for /v1/models).
    pub fn capacity(&self) -> (String, serde_json::Value) {
        let capacity: BTreeMap<&String, i64> = self
            .limits
            .iter()
            .map(|(k, l)| (k, l.max_concurrent))
            .collect();
        (self.estate.clone(), serde_json::json!(capacity))
    }
}

/// Which kind of work a permit admits (named in the timing row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A model call or stream.
    ModelCall,
    /// A Lab work order.
    LabWorkOrder,
}

/// A place in a deployment's queue; dropping it leaves the queue.
struct QueueSlot {
    manager: Arc<Manager>,
    id: String,
    left: bool,
}

impl QueueSlot {
    fn leave(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if !self.left {
            self.left = true;
            if let Some(st) = self.manager.lock().states.get_mut(&self.id) {
                st.queued -= 1;
            }
        }
    }
}

impl Drop for QueueSlot {
    fn drop(&mut self) {
        self.release();
    }
}

/// Admitted capacity, released exactly once when dropped, with its `admission_timing` row.
#[derive(Debug)]
pub struct Permit {
    manager: Arc<Manager>,
    id: String,
    host: String,
    model: String,
    request_id: String,
    scope: Scope,
    queue_wait: Duration,
    admitted: Instant,
}

impl Drop for Permit {
    fn drop(&mut self) {
        {
            let mut c = self.manager.lock();
            if let Some(st) = c.states.get_mut(&self.id) {
                st.active -= 1;
            }
            if !self.host.is_empty()
                && let Some(n) = c.host_active.get_mut(&self.host)
            {
                *n -= 1;
            }
        }
        self.manager.changed.notify_waiters();
        let elapsed = self.admitted.elapsed().as_secs_f64() * 1000.0;
        let (scope, latency_key) = match self.scope {
            Scope::ModelCall => ("model_call", "per_call_latency_ms"),
            Scope::LabWorkOrder => ("lab_work_order", "work_order_latency_ms"),
        };
        let mut row = serde_json::json!({
            "event": "admission_timing",
            "deployment_id": self.id,
            "model": self.model,
            "request_id": self.request_id,
            "config_sha": self.manager.config_sha,
            "scope": scope,
            "queue_wait_ms": self.queue_wait.as_secs_f64() * 1000.0,
        });
        row[latency_key] = serde_json::json!(elapsed);
        crate::log::row(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(max_queued: i64, wait_ms: i64) -> Option<Arc<Manager>> {
        let c = Config {
            deployments: Some(vec![Deployment {
                id: "d".into(),
                provider: "vllm".into(),
                models: Some(vec!["m".into()]),
                max_concurrent: 1,
                max_queued,
                max_wait_ms: wait_ms,
                ..Deployment::default()
            }]),
            ..Config::default()
        };
        Manager::new(&c).ok().map(Arc::new)
    }

    fn state(m: &Manager) -> State {
        m.lock().states.get("d").copied().unwrap_or_default()
    }

    #[tokio::test]
    async fn queue_full_timeout_and_release_on_drop() {
        let Some(m) = manager(1, 50) else { return };
        let first = m.acquire("vllm", "m", Scope::ModelCall, "a").await;
        assert!(matches!(first, Ok(Some(_))));
        let waiter = {
            let m = Arc::clone(&m);
            tokio::spawn(async move {
                m.acquire("vllm", "m", Scope::ModelCall, "b")
                    .await
                    .map(|p| p.is_some())
            })
        };
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(
            m.acquire("vllm", "m", Scope::ModelCall, "c").await.err(),
            Some(Refusal("queue_full: d".into()))
        );
        assert_eq!(
            waiter.await.ok(),
            Some(Err(Refusal("queue_timeout: d".into())))
        );
        assert_eq!(
            state(&m),
            State {
                active: 1,
                queued: 0
            }
        );
        drop(first);
        assert_eq!(state(&m), State::default());
        assert_eq!(
            m.acquire("vllm", "x", Scope::ModelCall, "").await.err(),
            Some(Refusal("deployment_unbound: vllm/x".into()))
        );
    }

    #[tokio::test]
    async fn a_dropped_waiter_leaves_the_queue() {
        let Some(m) = manager(1, 10_000) else { return };
        let held = m.acquire("vllm", "m", Scope::ModelCall, "a").await;
        let waiting = {
            let m = Arc::clone(&m);
            tokio::spawn(async move { m.acquire("vllm", "m", Scope::ModelCall, "b").await.is_ok() })
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(state(&m).queued, 1);
        waiting.abort();
        let _ = waiting.await;
        assert_eq!(
            state(&m),
            State {
                active: 1,
                queued: 0
            }
        );
        drop(held);
        assert_eq!(state(&m), State::default());
    }

    #[test]
    fn invalid_configurations() {
        let dep = |f: &dyn Fn(&mut Deployment)| {
            let mut d = Deployment {
                id: "d".into(),
                provider: "vllm".into(),
                models: Some(vec!["m".into()]),
                max_concurrent: 1,
                ..Deployment::default()
            };
            f(&mut d);
            Manager::new(&Config {
                deployments: Some(vec![d]),
                ..Config::default()
            })
            .err()
        };
        assert_eq!(
            dep(&|d| d.max_queued = 2),
            Some("invalid deployment limit: d".into())
        );
        assert_eq!(
            dep(&|d| d.max_concurrent = 0),
            Some("invalid deployment limit: d".into())
        );
        assert_eq!(
            dep(&|d| d.capacity = Some(BTreeMap::from([("other".into(), 1)]))),
            Some("deployment d has no capacity for estate \"\"".into())
        );
        assert!(dep(&|_| {}).is_none());
    }
}
