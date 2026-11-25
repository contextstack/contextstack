//! Executor-template aliases and model context windows for compaction (see `thread.rs`).
//!
//! Template names mirror `cs/ops/bin/executor-templates.json` (`--model` targets and conservative
//! windows where the harness picks the model). Prefix order is longest-first so specific families win.

use crate::logf;
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex, PoisonError};

/// Context size when nothing matches (local models and the unknown).
pub const DEFAULT_WINDOW: i64 = 32_768;

/// How a spec `model` (often an executor template) is interpreted for window lookup.
enum TemplateRule {
    /// Size compaction with this window (`window_source`: `table`).
    Window(i64),
    /// Resolve to this model id, then use the model tables below.
    Model(&'static str),
}

/// Executor template → underlying model or a fixed conservative window. Kept in sync with
/// `executor-templates.json` when templates change.
const TEMPLATES: &[(&str, TemplateRule)] = &[
    ("claude-code", TemplateRule::Window(200_000)),
    (
        "claude-code-sonnet",
        TemplateRule::Model("claude-sonnet-5-5"),
    ),
    (
        "claude-code-sonnet-planner",
        TemplateRule::Model("claude-sonnet-5-5"),
    ),
    (
        "claude-code-sonnet-patch",
        TemplateRule::Model("claude-sonnet-5-5"),
    ),
    (
        "claude-auth-context-probe",
        TemplateRule::Model("claude-sonnet-5-5"),
    ),
    ("claude-code-opus", TemplateRule::Model("claude-opus-5-5")),
    (
        "claude-code-opus-judge",
        TemplateRule::Model("claude-opus-5-5"),
    ),
    ("codex", TemplateRule::Model("codex")),
    ("gemini-cli", TemplateRule::Model("gemini-cli")),
    ("cursor-agent", TemplateRule::Window(128_000)),
    ("cursor-sol", TemplateRule::Model("gpt-5.6-sol-high")),
    ("cursor-codex", TemplateRule::Model("gpt-5.3-codex-high")),
    ("cursor-composer", TemplateRule::Model("composer-2.5")),
    ("cursor-grok", TemplateRule::Model("grok-4.7-high")),
    (
        "cursor-sonnet",
        TemplateRule::Model("claude-sonnet-5-thinking-high"),
    ),
    ("antigravity", TemplateRule::Window(1_000_000)),
    (
        "antigravity-hydrogen",
        TemplateRule::Model("gemini-4-hydrogen-high"),
    ),
    ("opencode-go", TemplateRule::Window(128_000)),
    ("pi", TemplateRule::Window(128_000)),
    ("goose", TemplateRule::Window(128_000)),
    ("grok", TemplateRule::Window(131_072)),
];

/// Short aliases some CLIs report instead of a full model id.
const MODEL_ALIASES: &[(&str, &str)] =
    &[("sonnet", "claude-sonnet-5-5"), ("opus", "claude-opus-5-5")];

/// Exact model ids (and a few template names tested explicitly).
const EXACT: &[(&str, i64)] = &[
    ("claude-opus-5-5", 200_000),
    ("claude-sonnet-5-5", 200_000),
    ("claude-haiku-5-5", 200_000),
    ("gpt-6.1-sol", 272_000),
    ("gpt-5.6-sol", 272_000),
    ("grok-4.7", 131_072),
    ("composer-2.5", 200_000),
    ("kimi-k3", 128_000),
    ("kimi-k3.1-pro", 256_000),
    ("glm-5.3", 128_000),
    ("deepseek-v4-pro", 128_000),
    ("qwen3.7-plus", 128_000),
    ("cursor-agent", 128_000),
    ("antigravity", 1_000_000),
    ("pi", 128_000),
    ("goose", 128_000),
    ("grok", 131_072),
    ("opencode-go", 128_000),
];

/// Longest prefixes first so `gpt-5.6-sol` beats `gpt-`.
const PREFIXES: &[(&str, i64)] = &[
    ("gpt-6.1-sol", 272_000), // Cursor Sol family; 272K published for 5.6
    ("gpt-5.6-sol", 272_000),
    ("gpt-5.3-codex", 200_000),
    ("claude-sonnet-5", 200_000),
    ("claude-opus-5", 200_000),
    ("claude-haiku-5", 200_000),
    ("kimi-k3.1-pro", 256_000),
    ("kimi-k3", 128_000), // floor: provider docs vary by tier
    ("qwen3.7-plus", 128_000),
    ("qwen3.7-", 128_000),
    ("deepseek-v4", 128_000),
    ("deepseek-", 128_000),
    ("grok-4.7", 131_072), // floor: xAI does not publish a single window for 4.7
    ("grok-", 131_072),
    ("composer-2.5", 200_000), // floor: Cursor does not document composer window
    ("composer-", 200_000),
    ("glm-5.3", 128_000),
    ("glm-", 128_000),
    ("claude-", 200_000),
    ("gpt-", 128_000),
    ("codex", 200_000),
    ("gemini", 1_000_000),
    ("z-ai/glm", 128_000),
    ("local-coder", 32_768),
    ("local-reasoner", 32_768),
    ("qwen3-coder", 32_768),
    ("muse-code", 32_768),
];

static LOGGED_DEFAULTS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// After template and alias resolution: either a fixed window or a model id to match in [`EXACT`]/[`PREFIXES`].
pub(crate) struct Resolved<'a> {
    pub lookup: &'a str,
    pub template_window: Option<i64>,
}

/// Map a spec `model` (template name or model id) to what [`window_for_lookup`] should match.
pub fn resolve(spec_model: &str) -> Resolved<'_> {
    if let Some((_, model)) = spec_model.split_once(':')
        && crate::capabilities::is_cli_template(spec_model)
    {
        return Resolved {
            lookup: model,
            template_window: None,
        };
    }
    if let Some((_, rule)) = TEMPLATES.iter().find(|(name, _)| *name == spec_model) {
        return match rule {
            TemplateRule::Window(w) => Resolved {
                lookup: spec_model,
                template_window: Some(*w),
            },
            TemplateRule::Model(id) => Resolved {
                lookup: id,
                template_window: None,
            },
        };
    }
    let lookup = MODEL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == spec_model)
        .map_or(spec_model, |(_, id)| *id);
    Resolved {
        lookup,
        template_window: None,
    }
}

fn log_unknown_once(log_name: &str) {
    let mut set = LOGGED_DEFAULTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if set.insert(log_name.to_string()) {
        logf!("context window unknown for model {log_name:?}; using {DEFAULT_WINDOW}");
    }
}

/// Context size for a resolved name. `log_name` is the spec's `model` (for one-time default logging).
pub fn window_for_lookup(
    lookup: &str,
    log_name: &str,
    template_window: Option<i64>,
) -> (i64, &'static str) {
    if let Some(w) = template_window {
        return (w, "table");
    }
    if let Some((_, w)) = EXACT.iter().find(|(name, _)| *name == lookup) {
        return (*w, "table");
    }
    if let Some((_, w)) = PREFIXES
        .iter()
        .find(|(prefix, _)| lookup.starts_with(prefix))
    {
        return (*w, "table");
    }
    log_unknown_once(log_name);
    (DEFAULT_WINDOW, "default")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_map_to_models_or_conservative_windows() {
        let r = resolve("codex:gpt-6.2-sol");
        assert_eq!(r.lookup, "gpt-6.2-sol");
        assert!(r.template_window.is_none());
        let r = resolve("cursor-composer");
        assert_eq!(r.lookup, "composer-2.5");
        assert!(r.template_window.is_none());
        let r = resolve("cursor-agent");
        assert_eq!(r.template_window, Some(128_000));
        let r = resolve("claude-code-opus");
        assert_eq!(r.lookup, "claude-opus-5-5");
    }
}
