//! The node's layered provider catalog: embedded defaults, a host overlay directory
//! and an optional tenant policy file merge into one validated, revisioned [`Candidate`]. Policy is the
//! intersection of every layer and denial wins.
//!
//! Host overlay JSON (`csgateway.providers.v1` endpoints only; `csgateway.providers.v2` may also carry
//! `models`, `deployments` and `deployment_hosts` with the same shapes as `models.json`). Overlay models
//! merge into the static inventory; overlay deployments merge into admission at startup. A name that
//! collides with a shipped model or deployment refuses startup and names both files.
//!
//! Every layer is decoded strictly: unknown fields, duplicate keys and trailing data are rejected. The
//! revision is a hash of the merged content (the same canonical JSON shape the Go gateway hashes), so
//! identical input gives the identical revision across reloads and restarts.

use crate::admission::Deployment;
use crate::inventory::ModelEntry;
use crate::protocol::{Caps, Protocol};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::IpAddr,
    path::{Path, PathBuf},
};

const OVERLAY_SCHEMA: &str = "csgateway.providers.v1";
const OVERLAY_SCHEMA_V2: &str = "csgateway.providers.v2";
const POLICY_SCHEMA: &str = "csgateway.policy.v1";
const MAX_FILE_BYTES: u64 = 1 << 20;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(f: &f64) -> bool {
    *f == 0.0
}

/// One endpoint in any layer. `name` is the stable endpoint id entries merge by.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Endpoint id.
    #[serde(default)]
    pub name: String,
    /// Wire protocol name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,
    /// Whether calls need a credential (embedded only).
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_key: bool,
    /// Shadow price when models.json omits one (embedded only).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub default_cost_per_1k: f64,
    /// Base URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    /// Model when a request names none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_model: String,
    /// The machine serving it (reported).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// Declared capabilities; `None` inherits the protocol constant (embedded only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Caps>,
    /// Overlay tombstone.
    #[serde(default, skip_serializing_if = "is_false")]
    pub remove: bool,
}

impl Entry {
    /// The compiled protocol (validated at load).
    pub fn protocol(&self) -> Option<Protocol> {
        Protocol::from_name(&self.protocol)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddedFile {
    providers: Vec<Entry>,
}

/// The embedded layer (`src/catalog.json`, kept identical to Go's `providers/catalog.json`).
pub fn embedded() -> Vec<Entry> {
    serde_json::from_str::<EmbeddedFile>(include_str!("catalog.json"))
        .map(|f| f.providers)
        .unwrap_or_default()
}

/// The embedded entry for an id (`requires_key`, shadow prices).
pub fn embedded_entry(name: &str) -> Option<Entry> {
    embedded()
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
}

/// One policy layer. `None` does not constrain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    /// Host constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<Egress>,
    /// Capability constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CapRules>,
}

/// Egress patterns: an exact host, `*.suffix`, or `loopback`. A deny match denies; a present allow
/// list (even empty) with no match denies.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Egress {
    /// Allowed hosts; `None` allows all.
    #[serde(default)]
    pub allow_hosts: Option<Vec<String>>,
    /// Denied hosts.
    #[serde(default)]
    pub deny_hosts: Option<Vec<String>>,
}

/// Capability narrowing; `None` leaves one unconstrained.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapRules {
    /// Image blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
    /// Tool declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    /// Streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
}

/// The director-synced tenant layer, cached on the node as a file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "the field names are the policy file's schema"
)]
pub struct Tenant {
    /// `csgateway.policy.v1`.
    #[serde(default)]
    pub schema: String,
    /// Tenant id.
    #[serde(default)]
    pub tenant: String,
    /// Monotonic revision.
    #[serde(default)]
    pub revision: i64,
    /// RFC 3339.
    #[serde(default)]
    pub issued_at: String,
    /// RFC 3339; required.
    #[serde(default)]
    pub expires_at: String,
    /// Host constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<Egress>,
    /// Capability constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CapRules>,
}

impl Tenant {
    fn rules(&self) -> Rules {
        Rules {
            egress: self.egress.clone(),
            capabilities: self.capabilities.clone(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayFile {
    #[serde(default)]
    schema: String,
    #[serde(default)]
    providers: Vec<Entry>,
    #[serde(default)]
    policy: Option<Rules>,
    #[serde(default)]
    models: Vec<ModelEntry>,
    #[serde(default)]
    deployments: Vec<Deployment>,
    #[serde(default)]
    deployment_hosts: BTreeMap<String, i64>,
}

fn overlay_schema_ok(schema: &str) -> bool {
    schema == OVERLAY_SCHEMA || schema == OVERLAY_SCHEMA_V2
}

/// Shipped model names, deployment ids and deployment hosts, for overlay collision checks.
type ShippedKeys = (HashSet<String>, HashSet<String>, HashSet<String>);

fn shipped_overlay_keys(path: Option<&Path>) -> Result<ShippedKeys, String> {
    #[derive(Deserialize)]
    struct Shipped {
        #[serde(default)]
        models: Option<Vec<ModelEntry>>,
        #[serde(default)]
        deployments: Option<Vec<Deployment>>,
        #[serde(default)]
        deployment_hosts: Option<BTreeMap<String, i64>>,
    }
    let Some(path) = path else {
        return Ok((HashSet::new(), HashSet::new(), HashSet::new()));
    };
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((HashSet::new(), HashSet::new(), HashSet::new()));
        }
        Err(e) => return Err(format!("models config: {e}")),
    };
    let f: Shipped = serde_json::from_slice(&raw).map_err(|e| format!("models config: {e}"))?;
    let models = f
        .models
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.name)
        .filter(|n| !n.is_empty())
        .collect();
    let deployments = f
        .deployments
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.id)
        .filter(|id| !id.is_empty())
        .collect();
    let hosts = f.deployment_hosts.unwrap_or_default().into_keys().collect();
    Ok((models, deployments, hosts))
}

fn valid_overlay_model(m: &ModelEntry) -> Result<(), String> {
    if m.name.is_empty() {
        return Err("model name is required".into());
    }
    if m.provider.is_empty() {
        return Err(format!("model {:?}: provider is required", m.name));
    }
    if !(1..=3).contains(&m.tier) {
        return Err(format!("model {:?}: tier must be 1..=3", m.name));
    }
    Ok(())
}

/// Overlay models appended to models.json's `models` for inventory assembly.
pub fn merge_overlay_models(
    base: Option<Vec<ModelEntry>>,
    overlay: &[ModelEntry],
) -> Option<Vec<ModelEntry>> {
    if base.is_none() && overlay.is_empty() {
        return None;
    }
    let mut out = base.unwrap_or_default();
    out.extend(overlay.iter().cloned());
    Some(out)
}

/// Where the host layers live.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Overlay `*.json` directory; missing is an empty layer.
    pub overlay_dir: Option<PathBuf>,
    /// Tenant policy; set means the node requires one (fail closed without it).
    pub policy_path: Option<PathBuf>,
    /// Process-level roots for the legacy local ids (`OLLAMA_ENDPOINT`, `VLLM_ENDPOINT`).
    pub endpoints: Vec<(String, String)>,
    /// Shipped `models.json` for overlay model/deployment collision checks.
    pub models_path: Option<PathBuf>,
}

/// One validated load of every layer.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    /// Merged endpoints, sorted by name.
    pub entries: Vec<Entry>,
    /// Which layers defined each id.
    pub sources: BTreeMap<String, String>,
    /// Overlay files read.
    pub files: Vec<String>,
    /// One per overlay policy block.
    pub local: Vec<Rules>,
    /// The tenant layer, when valid at load.
    pub tenant: Option<Tenant>,
    /// Configured but unusable at load ("missing", "invalid: ...").
    pub tenant_error: String,
    /// Host overlay model entries (merged into models.json at assembly).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlay_models: Vec<ModelEntry>,
    /// Host overlay deployments (merged into admission at startup).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlay_deployments: Vec<Deployment>,
    /// Host overlay shared-host keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overlay_deployment_hosts: BTreeMap<String, i64>,
    /// `r-` and 12 hex.
    #[serde(skip)]
    pub revision: String,
}

/// Tenant states; only `none` and `valid` let a non-loopback endpoint serve.
pub const TENANT_NONE: &str = "none";
const TENANT_VALID: &str = "valid";
const TENANT_EXPIRED: &str = "expired";

fn id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
}

/// Strip a trailing slash and `/v1`: the root form legacy discovery takes.
pub fn root(endpoint: &str) -> String {
    let t = endpoint.trim_end_matches('/');
    t.strip_suffix("/v1").unwrap_or(t).to_string()
}

fn decode_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {MAX_FILE_BYTES} bytes"));
    }
    let raw = std::fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&raw).map_err(|e| e.to_string())
}

impl Candidate {
    /// Read and merge every layer. An invalid overlay fails the whole load; a missing or invalid tenant
    /// policy is recorded (fail closed), not fatal.
    pub fn load(o: &Options) -> Result<Self, String> {
        let embedded = embedded();
        let mut c = Self {
            entries: Vec::new(),
            sources: BTreeMap::new(),
            files: Vec::new(),
            local: Vec::new(),
            tenant: None,
            tenant_error: String::new(),
            overlay_models: Vec::new(),
            overlay_deployments: Vec::new(),
            overlay_deployment_hosts: BTreeMap::new(),
            revision: String::new(),
        };
        let mut by_id: HashMap<String, Entry> = HashMap::new();
        for e in &embedded {
            by_id.insert(e.name.clone(), e.clone());
            c.sources.insert(e.name.clone(), "embedded".into());
        }
        if let Some(dir) = &o.overlay_dir {
            let (shipped_models, shipped_deployments, shipped_hosts) =
                shipped_overlay_keys(o.models_path.as_deref())?;
            c.load_overlay(
                dir,
                &mut by_id,
                &shipped_models,
                &shipped_deployments,
                &shipped_hosts,
            )?;
        }
        for (id, value) in &o.endpoints {
            let default = embedded
                .iter()
                .find(|e| &e.name == id)
                .map(|e| root(&e.endpoint))
                .unwrap_or_default();
            let Some(e) = by_id.get_mut(id) else { continue };
            if value.is_empty() || root(value) == default {
                continue;
            }
            e.endpoint = format!("{}/v1", root(value));
            if let Some(s) = c.sources.get_mut(id) {
                s.push_str("+env");
            }
        }
        let mut entries: Vec<Entry> = by_id.into_values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in &entries {
            valid_entry(e).map_err(|err| format!("endpoint {}: {err}", e.name))?;
        }
        c.entries = entries;
        if let Some(path) = &o.policy_path {
            match load_tenant(path) {
                Ok(t) => c.tenant = Some(t),
                Err(e) => c.tenant_error = e,
            }
        }
        let raw = serde_json::to_vec(&c).map_err(|e| e.to_string())?;
        let digest = crate::log::sha256_hex(&raw);
        c.revision = format!("r-{}", &digest[..12]);
        Ok(c)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "endpoints, models, deployments and hosts load in one pass; split in the 0.2 refactor"
    )]
    fn load_overlay(
        &mut self,
        dir: &Path,
        by_id: &mut HashMap<String, Entry>,
        shipped_models: &HashSet<String>,
        shipped_deployments: &HashSet<String>,
        shipped_hosts: &HashSet<String>,
    ) -> Result<(), String> {
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("overlay dir: {e}")),
        };
        let mut items: Vec<(String, bool)> = read
            .filter_map(Result::ok)
            .map(|d| {
                (
                    d.file_name().to_string_lossy().into_owned(),
                    d.path().is_dir(),
                )
            })
            .collect();
        items.sort();
        let mut defined_in: HashMap<String, String> = HashMap::new();
        let mut models_in: HashMap<String, String> = HashMap::new();
        let mut deployments_in: HashMap<String, String> = HashMap::new();
        let mut hosts_in: HashMap<String, String> = HashMap::new();
        for (name, is_dir) in items {
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if name.starts_with('.') || is_dir {
                continue;
            }
            if ext == "yaml" || ext == "yml" {
                return Err(format!(
                    "overlay {name}: YAML is not read by the gateway; write JSON ({OVERLAY_SCHEMA})"
                ));
            }
            if ext != "json" {
                continue;
            }
            let f: OverlayFile =
                decode_file(&dir.join(&name)).map_err(|e| format!("overlay {name}: {e}"))?;
            if !overlay_schema_ok(&f.schema) {
                return Err(format!(
                    "overlay {name}: schema {:?}, want {:?} or {OVERLAY_SCHEMA_V2:?}",
                    f.schema, OVERLAY_SCHEMA
                ));
            }
            self.files.push(name.clone());
            if let Some(policy) = f.policy {
                policy
                    .validate()
                    .map_err(|e| format!("overlay {name} policy: {e}"))?;
                self.local.push(policy);
            }
            for e in f.providers {
                if let Some(prior) = defined_in.get(&e.name) {
                    return Err(format!(
                        "overlay {name}: endpoint {:?} is already defined in {prior}",
                        e.name
                    ));
                }
                defined_in.insert(e.name.clone(), name.clone());
                let id = e.name.clone();
                self.merge(&name, e, by_id)
                    .map_err(|err| format!("overlay {name}: endpoint {id:?}: {err}"))?;
            }
            for m in f.models {
                valid_overlay_model(&m).map_err(|e| format!("overlay {name}: {e}"))?;
                if shipped_models.contains(&m.name) {
                    return Err(format!(
                        "overlay {name}: model {:?} is already defined in models.json",
                        m.name
                    ));
                }
                if let Some(prior) = models_in.get(&m.name) {
                    return Err(format!(
                        "overlay {name}: model {:?} is already defined in {prior}",
                        m.name
                    ));
                }
                models_in.insert(m.name.clone(), name.clone());
                self.overlay_models.push(m);
            }
            for d in f.deployments {
                if d.id.is_empty() {
                    return Err(format!("overlay {name}: deployment id is required"));
                }
                if shipped_deployments.contains(&d.id) {
                    return Err(format!(
                        "overlay {name}: deployment {:?} is already defined in models.json",
                        d.id
                    ));
                }
                if let Some(prior) = deployments_in.get(&d.id) {
                    return Err(format!(
                        "overlay {name}: deployment {:?} is already defined in {prior}",
                        d.id
                    ));
                }
                deployments_in.insert(d.id.clone(), name.clone());
                self.overlay_deployments.push(d);
            }
            for (host, cap) in f.deployment_hosts {
                if cap <= 0 {
                    return Err(format!("overlay {name}: invalid host capacity: {host}"));
                }
                if shipped_hosts.contains(&host)
                    || self.overlay_deployment_hosts.contains_key(&host)
                    || hosts_in.contains_key(&host)
                {
                    let prior = if shipped_hosts.contains(&host) {
                        "models.json"
                    } else {
                        hosts_in.get(&host).map_or("models.json", String::as_str)
                    };
                    return Err(format!(
                        "overlay {name}: deployment_hosts {host:?} is already defined in {prior}"
                    ));
                }
                hosts_in.insert(host.clone(), name.clone());
                self.overlay_deployment_hosts.insert(host, cap);
            }
        }
        Ok(())
    }

    /// Apply one overlay entry: add, patch (identity fixed) or remove.
    fn merge(
        &mut self,
        file: &str,
        e: Entry,
        by_id: &mut HashMap<String, Entry>,
    ) -> Result<(), String> {
        if !id_ok(&e.name) {
            return Err("name must match ^[a-z0-9][a-z0-9._-]{0,63}$".into());
        }
        if e.requires_key || e.default_cost_per_1k != 0.0 {
            return Err("requires_key and default_cost_per_1k are not settable by an overlay (credential references for overlay endpoints are not built)".into());
        }
        if e.remove {
            if e != (Entry {
                name: e.name.clone(),
                remove: true,
                ..Entry::default()
            }) {
                return Err("remove takes no other fields".into());
            }
            if by_id.remove(&e.name).is_none() {
                return Err("remove names no endpoint".into());
            }
            self.sources.remove(&e.name);
            return Ok(());
        }
        let Some(base) = by_id.get_mut(&e.name) else {
            if e.capabilities.is_none() {
                return Err(
                    "a new endpoint must declare capabilities (unknown is not permission)".into(),
                );
            }
            self.sources
                .insert(e.name.clone(), format!("overlay:{file}"));
            by_id.insert(e.name.clone(), e);
            return Ok(());
        };
        if !e.protocol.is_empty() && e.protocol != base.protocol {
            return Err(format!(
                "protocol {:?} differs from {:?}: an endpoint id keeps its protocol",
                e.protocol, base.protocol
            ));
        }
        if !e.endpoint.is_empty() {
            base.endpoint = e.endpoint;
        }
        if !e.default_model.is_empty() {
            base.default_model = e.default_model;
        }
        if !e.host.is_empty() {
            base.host = e.host;
        }
        if e.capabilities.is_some() {
            base.capabilities = e.capabilities;
        }
        if let Some(s) = self.sources.get_mut(&e.name) {
            s.push_str("+overlay:");
            s.push_str(file);
        }
        Ok(())
    }

    /// The tenant layer's state against the clock, evaluated on every call so a stopped sync expires.
    pub fn tenant_state(&self, now: DateTime<Utc>) -> String {
        match &self.tenant {
            None if self.tenant_error.is_empty() => TENANT_NONE.into(),
            None => self.tenant_error.clone(),
            Some(t) => {
                let expired =
                    DateTime::parse_from_rfc3339(&t.expires_at).map_or(true, |exp| now >= exp);
                if expired {
                    TENANT_EXPIRED.into()
                } else {
                    TENANT_VALID.into()
                }
            }
        }
    }

    /// Whether a call or discovery may connect to `endpoint`. Local rules and the tenant rule both apply;
    /// a configured but invalid tenant layer admits loopback only.
    pub fn egress(&self, endpoint: &str, now: DateTime<Utc>) -> Result<(), String> {
        if endpoint.is_empty() {
            return Ok(());
        }
        let host = reqwest::Url::parse(endpoint)
            .ok()
            .and_then(|u| {
                u.host_str()
                    .map(|h| h.trim_matches(['[', ']']).to_lowercase())
            })
            .unwrap_or_default();
        if host.is_empty() {
            return Err(format!("endpoint {endpoint:?} has no host"));
        }
        for r in &self.local {
            check_egress(r.egress.as_ref(), &host).map_err(|e| format!("local policy: {e}"))?;
        }
        match self.tenant_state(now).as_str() {
            TENANT_NONE => Ok(()),
            TENANT_VALID => {
                let t = self.tenant.as_ref().ok_or("tenant policy missing")?;
                check_egress(t.egress.as_ref(), &host)
                    .map_err(|e| format!("tenant policy {} r{}: {e}", t.tenant, t.revision))
            }
            st if loopback(&host) => {
                let _ = st;
                Ok(())
            }
            st => Err(format!(
                "tenant policy {st}: only loopback endpoints serve, {host} refused"
            )),
        }
    }

    /// Declaration ∩ every policy layer (the protocol constant applies separately, in each adapter).
    pub fn permitted(&self, decl: Option<Caps>, now: DateTime<Utc>) -> Caps {
        let mut p = decl.unwrap_or(Caps {
            images: true,
            tools: true,
            streaming: true,
        });
        let mut layers = self.local.clone();
        if self.tenant_state(now) == TENANT_VALID
            && let Some(t) = &self.tenant
        {
            layers.push(t.rules());
        }
        for r in layers {
            let Some(c) = r.capabilities else { continue };
            p.images &= c.images.unwrap_or(true);
            p.tools &= c.tools.unwrap_or(true);
            p.streaming &= c.streaming.unwrap_or(true);
        }
        p
    }

    /// The full intersection for one endpoint: protocol ∩ declaration ∩ local ∩ tenant.
    pub fn effective(&self, e: &Entry, now: DateTime<Utc>) -> Caps {
        let proto = e.protocol().map(Protocol::caps).unwrap_or_default();
        let p = self.permitted(e.capabilities, now);
        Caps {
            images: p.images && proto.images,
            tools: p.tools && proto.tools,
            streaming: p.streaming && proto.streaming,
        }
    }

    /// Whether the merged catalog has this id.
    pub fn has(&self, id: &str) -> bool {
        self.entries.iter().any(|e| e.name == id)
    }

    /// The merged entry for an id.
    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == id)
    }
}

/// An absolute http(s) URL with a host and no credentials, query or fragment: the one rule for every
/// endpoint the gateway may connect to (catalog entries and deployment identities).
pub fn plain_http_url(raw: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(raw).ok().filter(|u| {
        u.host_str().is_some_and(|h| !h.is_empty())
            && matches!(u.scheme(), "http" | "https")
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    })
}

fn valid_entry(e: &Entry) -> Result<(), String> {
    match e.protocol() {
        None => Err(format!("unknown protocol {:?}", e.protocol)),
        Some(Protocol::Mock) => Ok(()),
        Some(_) => {
            if plain_http_url(&e.endpoint).is_some() {
                Ok(())
            } else {
                Err(format!(
                    "endpoint {:?}: want an absolute http(s) URL without credentials, query or fragment",
                    e.endpoint
                ))
            }
        }
    }
}

impl Rules {
    fn validate(&self) -> Result<(), String> {
        let Some(eg) = &self.egress else {
            return Ok(());
        };
        let all = eg
            .allow_hosts
            .iter()
            .flatten()
            .chain(eg.deny_hosts.iter().flatten());
        for p in all {
            let h = p.strip_prefix("*.").unwrap_or(p);
            if p.is_empty()
                || *p != p.to_lowercase()
                || h.is_empty()
                || h.contains(['/', ':', '*', '@', ' '])
            {
                return Err(format!(
                    "host pattern {p:?}: want a lowercase host, *.suffix or loopback"
                ));
            }
        }
        Ok(())
    }
}

fn load_tenant(path: &Path) -> Result<Tenant, String> {
    if !path.exists() {
        return Err("missing".into());
    }
    let t: Tenant = decode_file(path).map_err(|e| format!("invalid: {e}"))?;
    if t.schema != POLICY_SCHEMA {
        return Err(format!(
            "invalid: schema {:?}, want {POLICY_SCHEMA:?}",
            t.schema
        ));
    }
    if t.tenant.is_empty() || t.revision <= 0 {
        return Err("invalid: tenant and a positive revision are required".into());
    }
    DateTime::parse_from_rfc3339(&t.expires_at).map_err(|e| format!("invalid: expires_at: {e}"))?;
    DateTime::parse_from_rfc3339(&t.issued_at).map_err(|e| format!("invalid: issued_at: {e}"))?;
    t.rules().validate().map_err(|e| format!("invalid: {e}"))?;
    Ok(t)
}

fn check_egress(e: Option<&Egress>, host: &str) -> Result<(), String> {
    let Some(e) = e else { return Ok(()) };
    for p in e.deny_hosts.iter().flatten() {
        if matches_host(p, host) {
            return Err(format!("host {host} denied by {p:?}"));
        }
    }
    match &e.allow_hosts {
        None => Ok(()),
        Some(allow) if allow.iter().any(|p| matches_host(p, host)) => Ok(()),
        Some(_) => Err(format!("host {host} is not in allow_hosts")),
    }
}

fn matches_host(pattern: &str, host: &str) -> bool {
    if pattern == "loopback" {
        return loopback(host);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return host.ends_with(suffix);
    }
    pattern == host
}

/// `localhost` or a loopback IP.
pub fn loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A rejected reload that would lose or roll back held tenant authority.
pub fn admit_tenant(prev: &Candidate, next: &Candidate) -> Result<(), String> {
    let Some(p) = &prev.tenant else { return Ok(()) };
    match &next.tenant {
        None if !next.tenant_error.is_empty() => Err(format!(
            "tenant policy {}; the held revision {} stays until its own expiry",
            next.tenant_error, p.revision
        )),
        Some(n) if n.revision < p.revision => Err(format!(
            "tenant policy rollback from revision {} to {}",
            p.revision, n.revision
        )),
        Some(n) if n.revision == p.revision && n != p => Err(format!(
            "tenant policy revision {} changed content without a new revision",
            n.revision
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_endpoints() {
        // Embedded catalog size is stable; bump deliberately when defaults change.
        assert_eq!(embedded().len(), 11);
        assert!(
            embedded()
                .iter()
                .any(|e| e.name == "typesafe" && e.requires_key)
        );
    }

    #[test]
    fn overlay_patch_add_remove_and_revision_stability() {
        let Ok(dir) = tempfile::tempdir() else { return };
        let body = r#"{"schema":"csgateway.providers.v1","providers":[
            {"name":"vllm","endpoint":"http://127.0.0.1:8000/v1","host":"spark"},
            {"name":"vllm-spark2","protocol":"openai_chat_completions","endpoint":"http://127.0.0.1:8001/v1","capabilities":{"tools":true}},
            {"name":"groq","remove":true}]}"#;
        assert!(std::fs::write(dir.path().join("dgx.json"), body).is_ok());
        let o = Options {
            overlay_dir: Some(dir.path().into()),
            ..Options::default()
        };
        let Ok(a) = Candidate::load(&o) else {
            panic!("overlay load failed");
        };
        assert!(a.has("vllm-spark2") && !a.has("groq"));
        assert_eq!(
            a.sources.get("vllm").map(String::as_str),
            Some("embedded+overlay:dgx.json")
        );
        assert_eq!(a.revision.len(), 14);
        assert_eq!(
            Candidate::load(&o).map(|b| b.revision).ok(),
            Some(a.revision.clone())
        );
        assert!(
            std::fs::write(
                dir.path().join("bad.json"),
                r#"{"schema":"csgateway.providers.v1","extra":1}"#
            )
            .is_ok()
        );
        assert!(Candidate::load(&o).is_err());
    }

    #[test]
    fn overlay_merge_models_and_shipped_collision() {
        let Ok(dir) = tempfile::tempdir() else { return };
        let shipped = dir.path().join("models.json");
        let body = r#"{"schema":"csgateway.providers.v2","providers":[
            {"name":"vllm-spark2","protocol":"openai_chat_completions","endpoint":"http://127.0.0.1:8001/v1","host":"box","capabilities":{"tools":true}}],
            "models":[{"name":"overlay-only","provider":"vllm-spark2","tier":2,"capability":"reasoning"}],
            "deployments":[{"id":"ov-dep","provider":"vllm-spark2","endpoint":"http://127.0.0.1:8001/v1","models":["overlay-only"],"max_concurrent":1,"max_queued":0,"max_wait_ms":0}],
            "deployment_hosts":{"box":1}}"#;
        assert!(std::fs::write(dir.path().join("dgx.json"), body).is_ok());
        let o = Options {
            overlay_dir: Some(dir.path().into()),
            models_path: Some(shipped.clone()),
            ..Options::default()
        };
        let Ok(c) = Candidate::load(&o) else {
            panic!("overlay with models should load");
        };
        assert_eq!(c.overlay_models.len(), 1);
        assert_eq!(c.overlay_models[0].name, "overlay-only");
        assert_eq!(c.overlay_deployments.len(), 1);
        let Some(merged) = merge_overlay_models(Some(vec![]), &c.overlay_models) else {
            panic!("overlay models should merge");
        };
        assert_eq!(merged.len(), 1);
        assert!(std::fs::write(
            shipped,
            r#"{"models":[{"name":"overlay-only","provider":"vllm","tier":1,"capability":"coding"}]}"#
        )
        .is_ok());
        let Err(err) = Candidate::load(&o) else {
            panic!("a shipped-name collision must be refused");
        };
        assert!(err.contains("models.json") && err.contains("overlay-only"));
    }

    #[test]
    fn egress_patterns_and_fail_closed() {
        let c = Candidate {
            entries: vec![],
            sources: BTreeMap::new(),
            files: vec![],
            local: vec![Rules {
                egress: Some(Egress {
                    allow_hosts: Some(vec!["loopback".into(), "*.anthropic.com".into()]),
                    deny_hosts: None,
                }),
                capabilities: None,
            }],
            tenant: None,
            tenant_error: "missing".into(),
            overlay_models: Vec::new(),
            overlay_deployments: Vec::new(),
            overlay_deployment_hosts: BTreeMap::new(),
            revision: String::new(),
        };
        let now = Utc::now();
        assert!(c.egress("http://127.0.0.1:8000/v1", now).is_ok());
        assert!(
            c.egress("https://api.anthropic.com", now).is_err(),
            "tenant missing: loopback only"
        );
        assert!(c.egress("https://api.openai.com/v1", now).is_err());
    }
}
