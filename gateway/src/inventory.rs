//! The model inventory: models.json entries with availability resolved from discovery, plus models
//! discovered on Ollama and vLLM that models.json does not name, and CLI executors found on the host.
//!
//! Named dispatch gates on membership, not availability (WIRE: unknown model refuses, never falls
//! through). The empty hint picks `local-coder`, else the lowest-tier `free_local` model; API models are
//! never chosen without a name.

use crate::pricing::ModelRates;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// The three cost classes.
pub const FREE_LOCAL: &str = "free_local";
/// Subscription-backed.
pub const SUBSCRIPTION: &str = "subscription";
/// Pay per token.
pub const API: &str = "api";

/// Optional client-facing metadata. Routing still uses `ModelEntry::provider`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Picker {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub provider_label: String,
    pub label: String,
    #[serde(default)]
    pub internal: bool,
    #[serde(default)]
    pub efforts: Vec<String>,
}

/// One model entry (models.json `models[]`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ModelEntry {
    /// Routing name.
    #[serde(default)]
    pub name: String,
    /// Catalog endpoint id.
    #[serde(default)]
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picker: Option<Picker>,
    /// 1..3.
    #[serde(default)]
    pub tier: i64,
    /// coding | reasoning | general.
    #[serde(default)]
    pub capability: String,
    /// Preferred within its tier.
    #[serde(default)]
    pub preferred: bool,
    /// A candidate (never auto-selected).
    #[serde(default)]
    pub candidate: bool,
    /// Candidate profiles.
    #[serde(default)]
    pub candidate_profiles: Vec<String>,
    /// "cli" for terminal tools.
    #[serde(default)]
    pub executor: String,
    /// CLI binary (default: the name).
    #[serde(default)]
    pub cli_binary: String,
    /// Blended price per 1k tokens when `rates` is absent.
    #[serde(default)]
    pub cost_per_1k: f64,
    /// Per-1M-token list prices; partial objects fall back per category to `cost_per_1k`.
    #[serde(default)]
    pub rates: ModelRates,
    /// Explicit cost class.
    #[serde(default)]
    pub cost_class: String,
    /// Resolved from discovery; never read from models.json.
    #[serde(skip)]
    pub available: bool,
}

impl ModelEntry {
    /// A CLI executor entry (never callable on model.call).
    pub fn is_cli(&self) -> bool {
        self.executor == "cli"
    }

    fn infer_class(&mut self) {
        if !matches!(self.cost_class.as_str(), FREE_LOCAL | SUBSCRIPTION | API) {
            self.cost_class = if self.is_cli() {
                SUBSCRIPTION
            } else if self.provider == "vllm" || self.provider == "ollama" {
                FREE_LOCAL
            } else {
                API
            }
            .into();
        }
    }
}

#[derive(Deserialize, Default)]
struct ModelsFile {
    #[serde(default)]
    models: Option<Vec<ModelEntry>>,
}

/// Read models.json's `models`; a missing file is `None` (an empty static inventory).
pub fn load_models(path: &std::path::Path) -> Result<Option<Vec<ModelEntry>>, String> {
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice::<ModelsFile>(&raw)
            .map(|f| Some(f.models.unwrap_or_default()))
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// What one assembly observed.
#[derive(Debug, Default)]
pub struct Observed {
    /// Ollama model names.
    pub ollama: Vec<String>,
    /// vLLM model ids.
    pub vllm: Vec<String>,
    /// Successful cloud and extra-endpoint listings by provider id.
    pub listed: HashMap<String, HashSet<String>>,
    /// Providers that stay selectable without a listing (store-backed, listing failed or deferred).
    pub deferred: Vec<String>,
    /// Ollama shadow price.
    pub ollama_shadow: f64,
}

/// A resolved inventory. Immutable once built; a refresh builds a new one.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    entries: Vec<ModelEntry>,
}

impl Inventory {
    /// Build: availability from discovery; extra Ollama/vLLM names auto-add at tier 2.
    pub fn build(configured: Option<Vec<ModelEntry>>, seen: &Observed) -> Self {
        let ollama: HashSet<&str> = seen.ollama.iter().map(String::as_str).collect();
        let vllm: HashSet<&str> = seen.vllm.iter().map(String::as_str).collect();
        let mut entries = configured.unwrap_or_default();
        let mut configured_ollama = HashSet::new();
        let mut configured_vllm = HashSet::new();
        let candidates: HashSet<String> = entries
            .iter()
            .filter(|e| e.candidate)
            .map(|e| e.name.clone())
            .collect();
        for e in &mut entries {
            if e.is_cli() {
                e.available = false;
                continue;
            }
            match e.provider.as_str() {
                "ollama" => {
                    configured_ollama.insert(e.name.clone());
                    e.available = ollama.contains(e.name.as_str());
                    if e.cost_per_1k == 0.0 {
                        e.cost_per_1k = seen.ollama_shadow;
                    }
                }
                "vllm" => {
                    configured_vllm.insert(e.name.clone());
                    e.available = vllm.contains(e.name.as_str());
                }
                "mock" => e.available = true,
                p => {
                    e.available = match seen.listed.get(p) {
                        Some(set) => set.contains(&e.name),
                        None => seen.deferred.iter().any(|d| d == p),
                    };
                }
            }
        }
        for name in &seen.ollama {
            if !configured_ollama.contains(name) && !candidates.contains(name) {
                entries.push(ModelEntry {
                    name: name.clone(),
                    provider: "ollama".into(),
                    tier: 2,
                    capability: "general".into(),
                    available: true,
                    cost_per_1k: seen.ollama_shadow,
                    ..ModelEntry::default()
                });
            }
        }
        for name in &seen.vllm {
            if !configured_vllm.contains(name) && !candidates.contains(name) {
                entries.push(ModelEntry {
                    name: name.clone(),
                    provider: "vllm".into(),
                    tier: 2,
                    capability: "coding".into(),
                    available: true,
                    ..ModelEntry::default()
                });
            }
        }
        for e in &mut entries {
            e.infer_class();
        }
        Self { entries }
    }

    /// Mark CLI entries available when their binary resolves.
    pub fn resolve_cli(&mut self, mut look: impl FnMut(&str) -> bool) {
        for e in self.entries.iter_mut().filter(|e| e.is_cli()) {
            let binary = if e.cli_binary.is_empty() {
                e.name.clone()
            } else {
                e.cli_binary.clone()
            };
            e.available = look(&binary);
        }
    }

    /// Add discovered CLI executors not already present by name.
    pub fn append_cli(&mut self, found: Vec<ModelEntry>) {
        for mut e in found {
            if !self.entries.iter().any(|x| x.name == e.name) {
                e.infer_class();
                self.entries.push(e);
            }
        }
    }

    /// Every entry.
    pub fn all(&self) -> &[ModelEntry] {
        &self.entries
    }

    /// Available entries.
    pub fn available(&self) -> impl Iterator<Item = &ModelEntry> {
        self.entries.iter().filter(|e| e.available)
    }

    /// The first entry with this name.
    pub fn by_name(&self, name: &str) -> Option<&ModelEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Providers with an available model, in first-seen order.
    pub fn providers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in self.available() {
            if !out.contains(&e.provider) {
                out.push(e.provider.clone());
            }
        }
        out
    }

    /// Available CLI executors, optionally at one tier.
    pub fn cli(&self, tier: i64) -> impl Iterator<Item = &ModelEntry> {
        self.available()
            .filter(move |e| e.is_cli() && (tier == 0 || e.tier == tier))
    }

    /// Selectable API models at a tier: available, not candidates; `free_local`, then subscription, then
    /// api; preferred first; then by name.
    pub fn for_tier(&self, tier: i64) -> Vec<&ModelEntry> {
        let rank = |c: &str| match c {
            FREE_LOCAL => 0,
            SUBSCRIPTION => 1,
            _ => 2,
        };
        let mut out: Vec<&ModelEntry> = self
            .available()
            .filter(|e| e.tier == tier && !e.candidate && !e.is_cli())
            .collect();
        out.sort_by(|a, b| {
            rank(&a.cost_class)
                .cmp(&rank(&b.cost_class))
                .then(b.preferred.cmp(&a.preferred))
                .then(a.name.cmp(&b.name))
        });
        out
    }

    /// Per-model differences between two inventories (`added`, `unavailable`, `removed`).
    pub fn changes(old: &Self, new: &Self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for e in &new.entries {
            match old.by_name(&e.name) {
                None => out.push(("added".into(), e.name.clone(), e.provider.clone())),
                Some(o) if !o.available && e.available => {
                    out.push(("added".into(), e.name.clone(), e.provider.clone()));
                }
                Some(o) if o.available && !e.available => {
                    out.push(("unavailable".into(), e.name.clone(), e.provider.clone()));
                }
                Some(_) => {}
            }
        }
        for o in &old.entries {
            if new.by_name(&o.name).is_none() {
                out.push(("removed".into(), o.name.clone(), o.provider.clone()));
            }
        }
        out
    }
}

/// The well-known CLI executors auto-discovered on the host: (name, binary, provider).
pub const WELL_KNOWN_CLI: [(&str, &str, &str); 3] = [
    ("claude-code", "claude", "anthropic"),
    ("codex", "codex", "openai"),
    ("gemini-cli", "gemini", "gemini"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, provider: &str, tier: i64) -> ModelEntry {
        ModelEntry {
            name: name.into(),
            provider: provider.into(),
            tier,
            ..ModelEntry::default()
        }
    }

    #[test]
    fn availability_and_auto_add() {
        let seen = Observed {
            ollama: vec!["o1".into(), "o2".into()],
            vllm: vec!["v1".into()],
            ollama_shadow: 0.002,
            ..Observed::default()
        };
        let inv = Inventory::build(
            Some(vec![
                entry("o1", "ollama", 1),
                entry("v-missing", "vllm", 2),
                entry("gpt", "openai", 1),
            ]),
            &seen,
        );
        let avail: Vec<&str> = inv.available().map(|e| e.name.as_str()).collect();
        assert_eq!(avail, vec!["o1", "o2", "v1"]);
        assert_eq!(
            inv.by_name("o2")
                .map(|e| (e.tier, e.cost_class.as_str(), e.cost_per_1k)),
            Some((2, FREE_LOCAL, 0.002))
        );
        assert_eq!(inv.by_name("gpt").map(|e| e.cost_class.as_str()), Some(API));
    }
}
