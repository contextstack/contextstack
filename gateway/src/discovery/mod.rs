//! Discovery: ask each endpoint what it serves, build the inventory from the answers, and record per
//! endpoint health. An endpoint the snapshot's egress policy denies is never contacted. Failures log
//! and continue: the inventory records unavailability, health records which endpoint failed.

pub mod cache;
pub mod health;

use crate::catalog::{Candidate, root};
use crate::credentials::StoreError;
use crate::executables::{Identity, Resolver};
use crate::inventory::{Inventory, ModelEntry, Observed, SUBSCRIPTION, WELL_KNOWN_CLI};
use crate::protocol::Http;
use crate::settings::Settings;
use cache::Cache;
use health::{Failure, Health, Outcome};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const CLOUD_TIMEOUT: Duration = Duration::from_secs(15);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// A cloud model listing: (provider, URL, auth header, auth prefix, extra header, id prefix to strip).
struct CloudDef {
    name: &'static str,
    url: &'static str,
    header: &'static str,
    prefix: &'static str,
    extra: Option<(&'static str, &'static str)>,
    strip: &'static str,
}

const CLOUD: [CloudDef; 7] = [
    CloudDef {
        name: "anthropic",
        url: "https://api.anthropic.com/v1/models?limit=1000",
        header: "x-api-key",
        prefix: "",
        extra: Some(("anthropic-version", "2023-06-01")),
        strip: "",
    },
    CloudDef {
        name: "openai",
        url: "https://api.openai.com/v1/models",
        header: "Authorization",
        prefix: "Bearer ",
        extra: None,
        strip: "",
    },
    CloudDef {
        name: "groq",
        url: "https://api.groq.com/openai/v1/models",
        header: "Authorization",
        prefix: "Bearer ",
        extra: None,
        strip: "",
    },
    CloudDef {
        name: "together",
        url: "https://api.together.xyz/v1/models",
        header: "Authorization",
        prefix: "Bearer ",
        extra: None,
        strip: "",
    },
    CloudDef {
        name: "lambda",
        url: "https://api.lambda.ai/v1/models",
        header: "Authorization",
        prefix: "Bearer ",
        extra: None,
        strip: "",
    },
    CloudDef {
        name: "openrouter",
        url: "https://openrouter.ai/api/v1/models",
        header: "Authorization",
        prefix: "Bearer ",
        extra: None,
        strip: "",
    },
    CloudDef {
        name: "gemini",
        url: "https://generativelanguage.googleapis.com/v1beta/openai/models",
        header: "Authorization",
        prefix: "Bearer ",
        extra: None,
        strip: "models/",
    },
];

fn fail(class: &'static str, message: String) -> Failure {
    Failure { class, message }
}

fn request_failure(prefix: &str, e: &reqwest::Error) -> Failure {
    let class = if e.is_timeout() { "timeout" } else { "request" };
    let mut cause = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        cause = s.to_string();
        source = s.source();
    }
    fail(class, format!("{prefix}{cause}"))
}

/// A listing response: the body, or a non-2xx status.
enum Fetched {
    Body(bytes::Bytes),
    Status(u16),
}

async fn get(
    http: &Http,
    url: &str,
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<Fetched, reqwest::Error> {
    let mut req = http.client().get(url).timeout(timeout);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    if status >= 300 {
        return Ok(Fetched::Status(status));
    }
    resp.bytes().await.map(Fetched::Body)
}

/// Ollama `/api/tags`.
async fn ollama(http: &Http, root: &str) -> Result<Vec<String>, Failure> {
    #[derive(Deserialize)]
    struct Tags {
        #[serde(default)]
        models: Vec<Tag>,
    }
    #[derive(Deserialize)]
    struct Tag {
        #[serde(default)]
        name: String,
    }
    let body = match get(http, &format!("{root}/api/tags"), &[], DISCOVERY_TIMEOUT).await {
        Err(e) => return Err(request_failure("ollama discovery failed: ", &e)),
        Ok(Fetched::Status(s)) => {
            return Err(fail(
                "status",
                format!("ollama /api/tags returned status {s}"),
            ));
        }
        Ok(Fetched::Body(b)) => b,
    };
    let tags: Tags = crate::protocol::openai::first_value(&body)
        .map_err(|e| fail("decode", format!("ollama discovery decode failed: {e}")))?;
    Ok(tags.models.into_iter().map(|m| m.name).collect())
}

#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    #[serde(default)]
    id: String,
}

/// vLLM `/v1/models`.
async fn vllm(http: &Http, root: &str) -> Result<Vec<String>, Failure> {
    let body = match get(http, &format!("{root}/v1/models"), &[], DISCOVERY_TIMEOUT).await {
        Err(e) => return Err(request_failure("vllm discovery failed: ", &e)),
        Ok(Fetched::Status(s)) => {
            return Err(fail(
                "status",
                format!("vllm /v1/models returned status {s}"),
            ));
        }
        Ok(Fetched::Body(b)) => b,
    };
    let l: Listing = crate::protocol::openai::first_value(&body)
        .map_err(|e| fail("decode", format!("vllm discovery decode failed: {e}")))?;
    Ok(l.data.into_iter().map(|m| m.id).collect())
}

/// A cloud (or extra keyless endpoint) listing: `{data:[{id}]}` or a bare `[{id}]`.
async fn listing(
    http: &Http,
    url: String,
    headers: Vec<(&str, String)>,
    strip: &str,
) -> Result<Vec<String>, Failure> {
    let body = match get(http, &url, &headers, CLOUD_TIMEOUT).await {
        Err(e) => return Err(request_failure("request failed: ", &e)),
        Ok(Fetched::Status(s)) => return Err(fail("status", format!("status {s}"))),
        Ok(Fetched::Body(b)) => b,
    };
    let v: serde_json::Value = crate::protocol::openai::first_value(&body)
        .map_err(|e| fail("decode", format!("decode: {e}")))?;
    let items: Vec<ListedModel> = if v.is_array() {
        serde_json::from_value(v).map_err(|e| fail("decode", format!("decode array: {e}")))?
    } else {
        serde_json::from_value::<Listing>(v)
            .map_err(|e| fail("decode", format!("decode envelope: {e}")))?
            .data
    };
    Ok(items
        .into_iter()
        .map(|m| m.id.strip_prefix(strip).map(String::from).unwrap_or(m.id))
        .collect())
}

/// Whether Ollama answers `/api/tags` with 200 (for /v1/health).
pub async fn ollama_reachable(http: &Http, root: &str) -> bool {
    http.client()
        .get(format!("{root}/api/tags"))
        .timeout(HEALTH_TIMEOUT)
        .send()
        .await
        .is_ok_and(|r| r.status().as_u16() == 200)
}

/// What an assembly needs from the process.
pub struct Context<'a> {
    /// Settings (store, store-backed providers).
    pub settings: &'a Settings,
    /// Outbound client.
    pub http: &'a Http,
    /// Per-endpoint health.
    pub health: &'a Health,
    /// The model cache.
    pub cache: &'a Cache,
    /// The executables policy.
    pub exe: &'a Resolver,
}

/// One assembly's working state: what was attempted, denied and left unconfigured.
struct Assembly<'a> {
    cx: &'a Context<'a>,
    cand: &'a Candidate,
    now: chrono::DateTime<chrono::Utc>,
    attempted: HashMap<String, Outcome>,
    denied: HashMap<String, String>,
    unconfigured: HashMap<String, String>,
    seen: Observed,
}

impl Assembly<'_> {
    /// Whether discovery may contact `id` at `endpoint` (enabled, in this catalog, egress allowed).
    fn may(&mut self, id: &str, endpoint: &str) -> bool {
        if self.cx.health.disabled(id) || !self.cand.has(id) {
            return false;
        }
        match self.cand.egress(endpoint, self.now) {
            Ok(()) => true,
            Err(e) => {
                self.denied
                    .insert(id.to_string(), format!("egress denied by policy: {e}"));
                false
            }
        }
    }

    fn note(&mut self, id: &str, result: &Result<Vec<String>, Failure>) {
        self.cx.cache.observe(id, result, &self.cand.revision);
        if let Err(f) = result {
            crate::logf!("warning: {id} discovery failed: {}", f.message);
        }
        self.attempted.insert(
            id.to_string(),
            result.as_ref().map(Vec::len).map_err(Clone::clone),
        );
    }

    /// Ollama and vLLM, concurrently.
    async fn local(&mut self) {
        let root_of = |id: &str, fallback: &str| {
            self.cand
                .entry(id)
                .map_or_else(|| fallback.to_string(), |e| root(&e.endpoint))
        };
        let ollama_root = root_of("ollama", &self.cx.settings.ollama_endpoint);
        let vllm_root = root_of("vllm", &self.cx.settings.vllm_endpoint);
        let http = self.cx.http;
        let o = self
            .may("ollama", &ollama_root)
            .then(|| ollama(http, &ollama_root));
        let v = self.may("vllm", &vllm_root).then(|| vllm(http, &vllm_root));
        let (o, v) = tokio::join!(maybe(o), maybe(v));
        if let Some(r) = o {
            self.note("ollama", &r);
            self.seen.ollama = r.unwrap_or_default();
        }
        if let Some(r) = v {
            self.note("vllm", &r);
            self.seen.vllm = r.unwrap_or_default();
        }
    }

    /// Store-backed credentials for this assembly (resolved once per start, never re-read per pass):
    /// absent is unconfigured (no alert); a refusing store is a `credential`-class failure. Only
    /// endpoints in this revision's catalog are looked up.
    async fn credentials(&mut self) -> BTreeMap<&'static str, String> {
        let mut keys = BTreeMap::new();
        let settings = self.cx.settings;
        if !settings.store.is_on() {
            return keys;
        }
        for p in &settings.store_providers {
            let Some(def) = CLOUD.iter().find(|d| d.name == p) else {
                continue;
            };
            if self.cx.health.disabled(p) || !self.cand.has(p) {
                continue;
            }
            match settings.store.lookup(p).await {
                Ok(k) if !k.is_empty() => {
                    keys.insert(def.name, k);
                }
                Ok(_) | Err(StoreError::NotFound) => {
                    self.unconfigured.insert(
                        p.clone(),
                        format!("no credential in the store; run: csgateway credentials set {p}"),
                    );
                }
                Err(StoreError::Refused(msg)) => {
                    self.note(p, &Err(fail("credential", msg)));
                    self.seen.deferred.push(p.clone());
                }
            }
        }
        keys
    }

    /// Cloud listings for providers with a credential, and every extra keyless OpenAI-compatible
    /// endpoint, concurrently. A store-backed provider whose listing failed stays selectable.
    async fn listings(&mut self, mut keys: BTreeMap<&'static str, String>) {
        let http = self.cx.http;
        let mut jobs = Vec::new();
        for def in &CLOUD {
            let Some(key) = keys.get(def.name) else {
                continue;
            };
            if !self.may(def.name, def.url) {
                continue;
            }
            let mut headers = vec![(def.header, format!("{}{key}", def.prefix))];
            if let Some((k, v)) = def.extra {
                headers.push((k, v.to_string()));
            }
            jobs.push((
                def.name.to_string(),
                listing(http, def.url.to_string(), headers, def.strip),
            ));
        }
        keys.clear();
        let extra: Vec<(String, String)> = self
            .cand
            .entries
            .iter()
            .filter(|e| {
                !e.requires_key
                    && e.protocol == "openai_chat_completions"
                    && e.name != "vllm"
                    && e.name != "ollama"
            })
            .map(|e| (e.name.clone(), e.endpoint.clone()))
            .collect();
        for (name, endpoint) in extra {
            if self.may(&name, &endpoint) {
                let url = format!("{}/models", endpoint.trim_end_matches('/'));
                jobs.push((name, listing(http, url, Vec::new(), "")));
            }
        }
        let names: Vec<String> = jobs.iter().map(|(n, _)| n.clone()).collect();
        let results = futures_util::future::join_all(jobs.into_iter().map(|(_, f)| f)).await;
        for (name, r) in names.into_iter().zip(results) {
            self.note(&name, &r);
            match r {
                Ok(ids) => {
                    crate::logf!("provider discovery: {name} returned {} models", ids.len());
                    self.seen
                        .listed
                        .insert(name, ids.into_iter().collect::<HashSet<_>>());
                }
                Err(_) if self.cx.settings.store_backed(&name) => self.seen.deferred.push(name),
                Err(_) => {}
            }
        }
    }

    /// Every catalog endpoint with no discovery in this process, and why.
    fn static_reasons(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for e in &self.cand.entries {
            if self.attempted.contains_key(&e.name)
                || self.cx.health.disabled(&e.name)
                || self.unconfigured.contains_key(&e.name)
            {
                continue;
            }
            let reason = if let Some(d) = self.denied.get(&e.name) {
                d.clone()
            } else if self.cx.settings.store_backed(&e.name) {
                "credential stored but no discovery protocol; models come from models.json".into()
            } else if e.requires_key {
                "no credential in the gateway process; models come from models.json".into()
            } else {
                "no discovery protocol".into()
            };
            out.insert(e.name.clone(), reason);
        }
        out
    }
}

async fn maybe<T>(f: Option<impl Future<Output = T>>) -> Option<T> {
    match f {
        Some(f) => Some(f.await),
        None => None,
    }
}

/// Resolve configured CLI executors and auto-discover the well-known ones through the executables
/// policy; returns every identity looked up (for the health report).
fn resolve_cli(inventory: &mut Inventory, exe: &Resolver) -> Vec<Identity> {
    let mut resolved: BTreeMap<String, Identity> = BTreeMap::new();
    let mut look = |name: &str| {
        let r = exe.resolve(name);
        let ok = r.is_ok();
        resolved.insert(name.to_string(), r.unwrap_or_else(|id| id));
        ok
    };
    inventory.resolve_cli(&mut look);
    let configured: HashSet<String> = inventory
        .all()
        .iter()
        .filter(|e| e.is_cli())
        .map(|e| e.name.clone())
        .collect();
    let mut found = Vec::new();
    for (name, binary, provider) in WELL_KNOWN_CLI {
        if configured.contains(name) || !look(binary) {
            continue;
        }
        crate::logf!("cli discovery: auto-discovered {name} (binary: {binary})");
        found.push(ModelEntry {
            name: name.into(),
            provider: provider.into(),
            tier: 1,
            capability: "coding".into(),
            executor: "cli".into(),
            cli_binary: binary.into(),
            cost_class: SUBSCRIPTION.into(),
            available: true,
            ..ModelEntry::default()
        });
    }
    inventory.append_cli(found);
    resolved.into_values().collect()
}

/// Discover every endpoint of one catalog revision and build its inventory.
pub async fn assemble(
    cx: &Context<'_>,
    cand: &Candidate,
    configured: Option<Vec<ModelEntry>>,
) -> Inventory {
    let ollama_shadow =
        crate::catalog::embedded_entry("ollama").map_or(0.0, |e| e.default_cost_per_1k);
    let mut a = Assembly {
        cx,
        cand,
        now: chrono::Utc::now(),
        attempted: HashMap::new(),
        denied: HashMap::new(),
        unconfigured: HashMap::new(),
        seen: Observed {
            ollama_shadow,
            ..Observed::default()
        },
    };
    a.local().await;
    let keys = a.credentials().await;
    a.listings(keys).await;
    let mut inventory = Inventory::build(configured, &a.seen);
    let identities = resolve_cli(&mut inventory, cx.exe);
    cx.health.record(
        &a.attempted,
        &a.static_reasons(),
        &a.unconfigured,
        identities,
    );
    inventory
}
