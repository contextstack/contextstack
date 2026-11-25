//! Process state and dispatch.
//!
//! Ownership: the [`Gateway`] owns process-long state (settings, the HTTP client, the admission
//! manager, discovery health, the model cache, wire counters). Configuration is a published
//! [`Snapshot`]: an `Arc` swapped whole under a short lock. A request takes the current snapshot once
//! and uses its endpoints, inventory and policy for its whole life, so in-flight calls finish on the
//! revision they started with. Discovery replaces a snapshot's inventory (also an `Arc` swap).
//!
//! A call passes, in order: the snapshot's policy guard, admission (an owned permit), the credential
//! (read from the store per call, never kept), then the protocol.

use crate::admission::{Manager, Permit, Scope};
use crate::catalog::{self, Candidate, Entry, Options};
use crate::credentials::StoreError;
use crate::discovery::{self, cache::Cache, health::Health};
use crate::errors::{CallError, FailureClass};
use crate::executables::Resolver;
use crate::inventory::{self, Inventory};
use crate::protocol::{
    Call, Event, Http, Protocol, Reply, anthropic::Anthropic, history, mock, openai::OpenAi,
    typesafe,
};
use crate::settings::Settings;
use crate::workorder::Executor;
use chrono::{DateTime, Local, Utc};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use tokio::time::Instant;

/// One registered endpoint of a snapshot.
#[derive(Debug, Clone)]
struct Endpoint {
    entry: Entry,
    protocol: Protocol,
    /// Adapter base URL.
    base: String,
    /// Credential read from the store per call.
    store_backed: bool,
}

/// One published catalog revision.
pub struct Snapshot {
    /// Publication sequence (1 at startup).
    pub seq: i64,
    /// Revision id.
    pub revision: String,
    /// When published.
    pub loaded_at: DateTime<Utc>,
    /// The merged layers and policy.
    pub candidate: Arc<Candidate>,
    endpoints: HashMap<String, Endpoint>,
    inventory: RwLock<Arc<Inventory>>,
}

impl Snapshot {
    /// The current inventory.
    pub fn inventory(&self) -> Arc<Inventory> {
        Arc::clone(
            &self
                .inventory
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    fn set_inventory(&self, inv: Inventory) {
        *self
            .inventory
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(inv);
    }
}

/// The outcome of the latest reload that found new input.
#[derive(Debug, Clone, Default)]
struct ReloadState {
    at: Option<DateTime<Utc>>,
    ok: bool,
    revision: String,
    error: String,
    last_fail: String,
}

/// The process.
pub struct Gateway {
    /// Settings.
    pub settings: Settings,
    /// Outbound HTTP.
    pub http: Http,
    /// Capacity owner.
    pub admission: Arc<Manager>,
    /// Discovery health.
    pub health: Health,
    /// Model cache.
    pub cache: Cache,
    /// Executables policy.
    pub exe: Resolver,
    /// The CLI executor, when configured.
    pub executor: Option<Executor>,
    /// CLI work orders running now and recently finished (cancel by execution id).
    pub runs: crate::clirun::Registry,
    /// Wire cutover counters.
    pub wire: Mutex<crate::server::WireUsage>,
    /// Pay-as-you-go spend guard and per-source limits.
    pub spend: Arc<crate::spend::Guard>,
    /// Process start.
    pub started: std::time::Instant,
    opts: Options,
    current: RwLock<Arc<Snapshot>>,
    reload_lock: tokio::sync::Mutex<()>,
    reload_state: Mutex<ReloadState>,
    last_refresh: Mutex<DateTime<Local>>,
}

/// An open stream that holds its permit until the stream is dropped.
pub struct OpenStream {
    inner: Upstream,
    _permit: Option<Permit>,
}

/// One adapter's open stream.
enum Upstream {
    Anthropic(crate::protocol::anthropic::Stream),
    OpenAi(crate::protocol::openai::Stream),
    Mock(mock::Stream),
}

impl OpenStream {
    /// A permitless open Anthropic stream (unit tests drive the stream state directly).
    #[cfg(test)]
    pub fn from_anthropic(stream: crate::protocol::anthropic::Stream) -> Self {
        Self {
            inner: Upstream::Anthropic(stream),
            _permit: None,
        }
    }

    /// Tokens and charge the upstream reported before the stream ends (for partial settlement).
    pub fn metered_usage(&self) -> crate::protocol::MeteredUsage {
        match &self.inner {
            Upstream::Anthropic(s) => s.metered_usage(),
            Upstream::OpenAi(s) => s.metered_usage(),
            Upstream::Mock(s) => s.metered_usage(),
        }
    }

    /// The next event: deltas, then exactly one terminal; `Ok(None)` after it.
    pub async fn next(&mut self) -> Result<Option<Event>, CallError> {
        match &mut self.inner {
            Upstream::Anthropic(s) => s.next().await,
            Upstream::OpenAi(s) => s.next().await,
            Upstream::Mock(s) => Ok(s.next()),
        }
    }
}

/// An inventory refresh's report.
pub struct Refreshed {
    /// Entries before.
    pub before: usize,
    /// Entries after.
    pub after: usize,
    /// (action, model, provider).
    pub changes: Vec<(String, String, String)>,
}

impl Gateway {
    /// Compose the process and build the first snapshot. Errors are startup refusals.
    pub async fn new(
        settings: Settings,
        admission: Manager,
        exe: Resolver,
        executor: Option<Executor>,
        health: Health,
    ) -> Result<Self, String> {
        let opts = options(&settings);
        let first = Candidate::load(&opts).map_err(|e| format!("catalog: {e}"))?;
        let gw = Self {
            http: Http::new()?,
            admission: Arc::new(admission),
            health,
            cache: Cache::default(),
            exe,
            executor,
            runs: crate::clirun::Registry::default(),
            wire: Mutex::new(crate::server::WireUsage::default()),
            spend: Arc::new(crate::spend::Guard::new(settings.home.as_deref())),
            started: std::time::Instant::now(),
            current: RwLock::new(Arc::new(Snapshot {
                seq: 0,
                revision: String::new(),
                loaded_at: Utc::now(),
                candidate: Arc::new(first.clone()),
                endpoints: HashMap::new(),
                inventory: RwLock::new(Arc::new(Inventory::default())),
            })),
            opts,
            reload_lock: tokio::sync::Mutex::new(()),
            reload_state: Mutex::new(ReloadState::default()),
            last_refresh: Mutex::new(Local::now()),
            settings,
        };
        if !first.tenant_error.is_empty() {
            crate::logf!(
                "catalog: tenant policy {}: failing closed to loopback endpoints",
                first.tenant_error
            );
        }
        let snap = gw
            .build(first, 1)
            .await
            .map_err(|e| format!("catalog: {e}"))?;
        *gw.reload_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = ReloadState {
            at: Some(snap.loaded_at),
            ok: true,
            revision: snap.revision.clone(),
            ..ReloadState::default()
        };
        *gw.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(snap);
        *gw.last_refresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Local::now();
        Ok(gw)
    }

    /// The published snapshot.
    pub fn current(&self) -> Arc<Snapshot> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Take the snapshot for one request and log the binding.
    pub fn bind(&self, request_id: &str) -> Arc<Snapshot> {
        let snap = self.current();
        crate::log::row(&serde_json::json!({
            "event": "catalog_binding", "request_id": request_id,
            "catalog_revision": snap.revision, "catalog_seq": snap.seq,
        }));
        snap
    }

    /// When the inventory last refreshed.
    pub fn last_refresh(&self) -> DateTime<Local> {
        *self
            .last_refresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, cand: &Candidate) -> Result<HashMap<String, Endpoint>, String> {
        let mut out = HashMap::new();
        let mut errors = Vec::new();
        for e in &cand.entries {
            let Some(protocol) = e.protocol() else {
                errors.push(format!(
                    "construct {}: unknown protocol {:?} for provider {:?}",
                    e.name, e.protocol, e.name
                ));
                continue;
            };
            let store_backed = e.requires_key && self.settings.store_backed(&e.name);
            if e.requires_key && !store_backed {
                continue;
            }
            let base = match protocol {
                Protocol::OpenAiChatCompletions if e.name == "ollama" || e.name == "vllm" => {
                    format!("{}/v1", catalog::root(&e.endpoint))
                }
                Protocol::OpenAiChatCompletions if e.endpoint.is_empty() => {
                    errors.push(format!(
                        "construct {}: catalog provider {:?} has no endpoint",
                        e.name, e.name
                    ));
                    continue;
                }
                Protocol::AnthropicMessages if e.endpoint.is_empty() => {
                    "https://api.anthropic.com".into()
                }
                _ => e.endpoint.clone(),
            };
            out.insert(
                e.name.clone(),
                Endpoint {
                    entry: e.clone(),
                    protocol,
                    base,
                    store_backed,
                },
            );
        }
        if errors.is_empty() {
            Ok(out)
        } else {
            Err(format!("providers: {}", errors.join("\n")))
        }
    }

    async fn build(&self, cand: Candidate, seq: i64) -> Result<Snapshot, String> {
        let endpoints = self.register(&cand)?;
        let adapters: HashMap<String, Option<String>> = endpoints
            .iter()
            .map(|(k, e)| {
                let verifiable = match e.protocol {
                    Protocol::Mock => None,
                    _ if e.store_backed => Some(e.entry.endpoint.clone()),
                    _ => Some(e.base.clone()),
                };
                (k.clone(), verifiable)
            })
            .collect();
        let keyless: std::collections::HashSet<String> = cand
            .entries
            .iter()
            .filter(|e| e.requires_key && !self.settings.store_backed(&e.name))
            .map(|e| e.name.clone())
            .collect();
        self.admission.verify(&adapters, &keyless)?;
        let inv = self.assemble(&cand).await?;
        Ok(Snapshot {
            seq,
            revision: cand.revision.clone(),
            loaded_at: Utc::now(),
            candidate: Arc::new(cand),
            endpoints,
            inventory: RwLock::new(Arc::new(inv)),
        })
    }

    async fn assemble(&self, cand: &Candidate) -> Result<Inventory, String> {
        let configured = match inventory::load_models(&self.settings.models_path) {
            Ok(None) => {
                crate::logf!(
                    "warning: models config {} not found, starting with empty static inventory (copy models.example.json to models.json to configure)",
                    self.settings.models_path.display()
                );
                None
            }
            Ok(m) => m,
            Err(e) => return Err(format!("failed to load models config: {e}")),
        };
        let configured = catalog::merge_overlay_models(configured, &cand.overlay_models);
        let cx = discovery::Context {
            settings: &self.settings,
            http: &self.http,
            health: &self.health,
            cache: &self.cache,
            exe: &self.exe,
        };
        let inv = discovery::assemble(&cx, cand, configured).await;
        let names: Vec<String> = inv
            .available()
            .map(|m| format!("{}(tier={})", m.name, m.tier))
            .collect();
        crate::logf!("model inventory: {} available {:?}", names.len(), names);
        Ok(inv)
    }

    /// Re-run discovery for the current snapshot.
    pub async fn refresh(&self) -> Result<Refreshed, String> {
        let snap = self.current();
        let old = snap.inventory();
        let new = self.assemble(&snap.candidate).await?;
        let changes = Inventory::changes(&old, &new);
        let r = Refreshed {
            before: old.all().len(),
            after: new.all().len(),
            changes,
        };
        crate::logf!(
            "inventory refreshed: {} models, {} available",
            new.all().len(),
            new.available().count()
        );
        snap.set_inventory(new);
        *self
            .last_refresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Local::now();
        Ok(r)
    }

    /// Load every layer and publish a new snapshot when the revision changed. Any failure keeps the
    /// previous snapshot whole. `Ok(true)` only when a swap happened.
    pub async fn reload(&self) -> Result<bool, String> {
        let _serial = self.reload_lock.lock().await;
        let cur = self.current();
        let result = match Candidate::load(&self.opts) {
            Ok(c) if c.revision == cur.revision => {
                let mut st = self
                    .reload_state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if !st.ok {
                    // The input is back to what is published: the failure no longer holds.
                    *st = ReloadState {
                        at: Some(Utc::now()),
                        ok: true,
                        revision: cur.revision.clone(),
                        ..ReloadState::default()
                    };
                    crate::logf!(
                        "catalog: layers load cleanly again at {}; reload error cleared",
                        cur.revision
                    );
                }
                return Ok(false);
            }
            Ok(c) => match catalog::admit_tenant(&cur.candidate, &c) {
                Ok(()) => self.build(c, cur.seq + 1).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        let mut st = self
            .reload_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match result {
            Ok(snap) => {
                crate::logf!(
                    "catalog: published {} (seq {}, {} endpoints, overlay {:?}, tenant {})",
                    snap.revision,
                    snap.seq,
                    snap.candidate.entries.len(),
                    snap.candidate.files,
                    snap.candidate.tenant_state(Utc::now())
                );
                *st = ReloadState {
                    at: Some(snap.loaded_at),
                    ok: true,
                    revision: snap.revision.clone(),
                    ..ReloadState::default()
                };
                *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(snap);
                Ok(true)
            }
            Err(e) => {
                let msg = format!("catalog reload rejected, keeping {}: {e}", cur.revision);
                if msg != st.last_fail {
                    crate::log::line(&msg);
                    st.last_fail.clone_from(&msg);
                }
                st.at = Some(Utc::now());
                st.ok = false;
                st.revision.clear();
                st.error.clone_from(&msg);
                Err(msg)
            }
        }
    }

    /// GET /v1/catalog.
    pub fn catalog_status(&self) -> serde_json::Value {
        let st = self
            .reload_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let s = self.current();
        let c = &s.candidate;
        let now = Utc::now();
        let mut policy =
            serde_json::json!({"state": c.tenant_state(now), "local_layers": c.local.len()});
        if let Some(p) = &self.opts.policy_path {
            policy["path"] = serde_json::json!(p.display().to_string());
        }
        if let Some(t) = &c.tenant {
            policy["tenant"] = serde_json::json!(t.tenant);
            policy["revision"] = serde_json::json!(t.revision);
            policy["expires_at"] = serde_json::json!(t.expires_at);
        }
        let endpoints: Vec<serde_json::Value> = c
            .entries
            .iter()
            .map(|e| {
                let mut v = serde_json::json!({
                    "id": e.name, "protocol": e.protocol,
                    "source": c.sources.get(&e.name).cloned().unwrap_or_default(),
                    "capabilities": c.effective(e, now),
                    "egress": c.egress(&e.endpoint, now).err().unwrap_or_else(|| "allowed".into()),
                });
                if !e.endpoint.is_empty() {
                    v["endpoint"] = serde_json::json!(e.endpoint);
                }
                if !e.host.is_empty() {
                    v["host"] = serde_json::json!(e.host);
                }
                v
            })
            .collect();
        let mut last = serde_json::json!({"at": st.at.map(crate::log::rfc3339), "ok": st.ok});
        if !st.revision.is_empty() {
            last["revision"] = serde_json::json!(st.revision);
        }
        if !st.error.is_empty() {
            last["error"] = serde_json::json!(st.error);
        }
        serde_json::json!({
            "schema": "csgateway.catalog_status.v1",
            "revision": s.revision,
            "seq": s.seq,
            "loaded_at": crate::log::rfc3339(s.loaded_at),
            "overlay_dir": self.opts.overlay_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            "overlay_files": c.files,
            "policy": policy,
            "endpoints": endpoints,
            "last_reload": last,
            "model_cache": self.cache.report(now, self.health.stale_after()),
        })
    }

    fn endpoint<'s>(
        snap: &'s Snapshot,
        provider: &str,
        model: &str,
    ) -> Result<&'s Endpoint, CallError> {
        snap.endpoints.get(provider).ok_or_else(|| CallError::Routing {
            class: FailureClass::ProviderNotConfigured,
            message: format!(
                "model {model:?} is served by provider {provider:?}, which is not registered; check catalog overlays and credentials"
            ),
        })
    }

    /// The policy guard: egress and declared capability, before admission or any connection. A stream
    /// the endpoint cannot serve (protocol ∩ declaration) is `StreamingUnsupported`; one it can serve
    /// but policy forbids is `PolicyDenied`.
    fn guard(snap: &Snapshot, ep: &Endpoint, call: &Call, stream: bool) -> Result<(), CallError> {
        let now = Utc::now();
        let deny = |m: String| CallError::classified(FailureClass::PolicyDenied, m);
        let host = if ep.protocol == Protocol::Mock {
            ""
        } else {
            ep.entry.endpoint.as_str()
        };
        snap.candidate.egress(host, now).map_err(deny)?;
        if stream && !streams(&ep.entry) {
            return Err(CallError::classified(
                FailureClass::StreamingUnsupported,
                format!(
                    "{}: streaming unsupported (protocol or endpoint declaration); call unary",
                    ep.entry.name
                ),
            ));
        }
        let p = snap.candidate.permitted(ep.entry.capabilities, now);
        for (on, ok, what) in [
            (history::has_images(&call.messages), p.images, "images"),
            (!call.tools.is_empty(), p.tools, "tools"),
            (stream, p.streaming, "streaming"),
        ] {
            if on && !ok {
                return Err(deny(format!(
                    "{what} not permitted for endpoint {} (declaration or policy)",
                    ep.entry.name
                )));
            }
        }
        Ok(())
    }

    async fn admit(
        &self,
        ep: &Endpoint,
        model: &str,
        request_id: &str,
        deadline: Instant,
    ) -> Result<Option<Permit>, CallError> {
        match tokio::time::timeout_at(
            deadline,
            self.admission
                .acquire(&ep.entry.name, model, Scope::ModelCall, request_id),
        )
        .await
        {
            Err(_) => Err(CallError::classified(
                FailureClass::CapacityExhausted,
                "context deadline exceeded",
            )),
            Ok(Err(r)) => Err(CallError::classified(FailureClass::CapacityExhausted, r.0)),
            Ok(Ok(p)) => Ok(p),
        }
    }

    async fn credential(&self, ep: &Endpoint) -> Result<String, CallError> {
        if !ep.store_backed {
            return Ok(String::new());
        }
        let name = &ep.entry.name;
        match self.settings.store.lookup(name).await {
            Ok(k) if !k.is_empty() => Ok(k),
            Err(StoreError::NotFound) => Err(CallError::classified(
                FailureClass::ProviderAuth,
                format!("no credential configured for {name} (csgateway credentials set {name})"),
            )),
            _ => Err(CallError::classified(
                FailureClass::ProviderAuth,
                format!("keychain credential unavailable for {name}"),
            )),
        }
    }

    /// A unary call to `provider` on this snapshot.
    pub async fn invoke(
        &self,
        snap: &Snapshot,
        provider: &str,
        call: &Call,
        deadline: Instant,
        trace: &mut crate::trace::Exchange,
    ) -> Result<Reply, CallError> {
        let ep = Self::endpoint(snap, provider, &call.model)?;
        Self::guard(snap, ep, call, false)?;
        let _permit = self
            .admit(ep, &call.model, &call.request_id, deadline)
            .await?;
        let key = self.credential(ep).await?;
        match ep.protocol {
            Protocol::Mock => {
                if call.tool_choice.is_some() {
                    trace.drop_tool_choice();
                }
                trace.begin_attempt(provider, &[], false);
                Ok(mock::reply(&ep.entry.name, call))
            }
            Protocol::TypesafeSystemOne => Err(not_chat(ep)),
            Protocol::OpenAiChatCompletions => {
                let a = OpenAi {
                    name: &ep.entry.name,
                    base: &ep.base,
                    default_model: &ep.entry.default_model,
                };
                a.invoke(&self.http, Some(&key), call, deadline, trace)
                    .await
            }
            Protocol::AnthropicMessages => {
                let a = Anthropic {
                    base: &ep.base,
                    default_model: &ep.entry.default_model,
                };
                a.invoke(&self.http, &key, call, deadline, trace).await
            }
        }
    }

    /// A `TypeSafe` typed-question request (`body` unchanged) to the catalog's `provider` endpoint:
    /// egress policy, admission bound to `model`, the stored key, then `TypeSafe`'s answer, any status.
    pub async fn typed_question(
        &self,
        snap: &Snapshot,
        provider: &str,
        model: &str,
        request_id: &str,
        body: reqwest::Body,
        deadline: Instant,
    ) -> Result<typesafe::Answer, CallError> {
        let ep = Self::endpoint(snap, provider, model)?;
        if ep.protocol != Protocol::TypesafeSystemOne {
            return Err(CallError::classified(
                FailureClass::PolicyDenied,
                format!("{provider} is not a typed-question endpoint"),
            ));
        }
        snap.candidate
            .egress(&ep.entry.endpoint, Utc::now())
            .map_err(|m| CallError::classified(FailureClass::PolicyDenied, m))?;
        let _permit = self.admit(ep, model, request_id, deadline).await?;
        let key = self.credential(ep).await?;
        typesafe::post(&self.http, &ep.base, &key, body, deadline).await
    }

    /// Open a stream to `provider`; the returned stream owns its permit.
    pub async fn stream(
        &self,
        snap: &Snapshot,
        provider: &str,
        call: &Call,
        deadline: Instant,
        trace: &mut crate::trace::Exchange,
    ) -> Result<OpenStream, CallError> {
        let ep = Self::endpoint(snap, provider, &call.model)?;
        Self::guard(snap, ep, call, true)?;
        let permit = self
            .admit(ep, &call.model, &call.request_id, deadline)
            .await?;
        let key = self.credential(ep).await?;
        let inner = match ep.protocol {
            Protocol::AnthropicMessages => {
                let a = Anthropic {
                    base: &ep.base,
                    default_model: &ep.entry.default_model,
                };
                Upstream::Anthropic(a.stream(&self.http, &key, call, deadline, trace).await?)
            }
            Protocol::OpenAiChatCompletions => {
                let a = OpenAi {
                    name: &ep.entry.name,
                    base: &ep.base,
                    default_model: &ep.entry.default_model,
                };
                Upstream::OpenAi(
                    a.stream(&self.http, Some(&key), call, deadline, trace)
                        .await?,
                )
            }
            Protocol::Mock => {
                if call.tool_choice.is_some() {
                    trace.drop_tool_choice();
                }
                trace.begin_attempt(provider, &[], true);
                Upstream::Mock(mock::Stream::open(&ep.entry.name, call))
            }
            Protocol::TypesafeSystemOne => return Err(not_chat(ep)),
        };
        Ok(OpenStream {
            inner,
            _permit: permit,
        })
    }
}

/// A chat call that reached a typed-question endpoint (no models.json chat model should name one).
fn not_chat(ep: &Endpoint) -> CallError {
    CallError::classified(
        FailureClass::PolicyDenied,
        format!(
            "{} answers typed questions, not chat: use POST /v1/ai/typed-question",
            ep.entry.name
        ),
    )
}

/// Whether an endpoint can stream at all: its protocol constant ∩ its own declaration (policy layers
/// aside). The capability `/v1/models` reports is this ∩ policy.
fn streams(e: &Entry) -> bool {
    e.protocol().is_some_and(|p| p.caps().streaming) && e.capabilities.is_none_or(|c| c.streaming)
}

/// The catalog layer locations from settings.
pub fn catalog_options(s: &Settings) -> Options {
    options(s)
}

fn options(s: &Settings) -> Options {
    Options {
        overlay_dir: s.providers_dir.clone(),
        policy_path: s.policy_path.clone(),
        models_path: Some(s.models_path.clone()),
        endpoints: vec![
            ("ollama".into(), s.ollama_endpoint.clone()),
            ("vllm".into(), s.vllm_endpoint.clone()),
        ],
    }
}

/// The ids every catalog layer defines at startup (discovery-disabled names are checked against them).
pub fn known_ids(s: &Settings) -> Result<Vec<String>, String> {
    Candidate::load(&options(s))
        .map(|c| c.entries.into_iter().map(|e| e.name).collect())
        .map_err(|e| format!("catalog: {e}"))
}
