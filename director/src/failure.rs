//! Client-facing failure meaning, derived from stored evidence without changing it.
use serde_json::{Value, json};

// code, actionable message, whether switching model may help, whether retrying may succeed.
const KINDS: &[(&str, &str, bool, bool)] = &[
    ("provider_unavailable", "Try again when the model provider is available or choose another model.", true, true),
    ("auth_expired", "Sign in to this model again or choose another model.", true, false),
    ("provider_limit", "Wait for this model's provider usage limit to reset or choose another model.", true, true),
    ("rate_limit", "Wait a moment before retrying or choose another model.", true, true),
    ("model_error", "Retry the model request or choose another model.", true, true),
    ("timeout", "Retry with more time for the model to finish.", true, true),
    ("check_failed", "Fix the task's failing check before retrying.", false, false),
    ("check_timeout", "Give the task's check more time before retrying.", false, false),
    ("check_error", "Fix the task's check command before retrying.", false, false),
    ("budget", "Increase the exhausted budget before retrying.", false, false),
    ("stuck", "Clarify the task or choose another model before retrying.", true, false),
    ("cancelled", "Start the work again when you want it to continue.", false, false),
    ("memory_cap", "Increase the job's memory allowance before retrying.", false, false),
    ("workspace", "Fix the workspace setup before retrying.", false, false),
    ("preflight", "Fix the job's preflight setup before retrying.", false, false),
    ("preflight_timeout", "Give workspace preparation more time before retrying.", false, false),
    ("grant_paths", "Update the job's allowed paths before retrying.", false, false),
    ("login_file_tampered", "Restore the model's login files before retrying.", false, false),
    ("publish_evidence", "Fix evidence publication on the node before retrying.", false, false),
    ("evidence_incomplete", "Restore the missing run evidence before retrying.", false, false),
    ("harvest", "Fix workspace harvesting on the node before retrying.", false, false),
    ("gateway_unreachable", "Restore the node's gateway connection before retrying.", false, true),
    ("gateway_config", "Fix the gateway's model configuration before retrying.", false, false),
    ("gateway_protocol", "Fix the gateway connection's protocol error before retrying.", false, false),
    ("node_config", "Fix the node's configuration before retrying.", false, false),
    ("node_restart", "Retry the job after the node has restarted.", false, true),
    ("hook_rejected", "Fix the hook's refusal before retrying.", false, false),
    ("sandbox_unenforceable", "Restore sandbox support on the node before retrying.", false, false),
    ("sandbox_denied", "Update the workspace's sandbox permissions before retrying.", false, false),
    ("spool_full", "Free disk space on the node before retrying.", false, true),
    ("tool_timeout", "Give the timed out tool more time before retrying.", false, true),
    ("unknown", "Inspect the job's logs for the failure reason before retrying.", false, false),
];

fn code(reason: &str) -> Option<&'static str> {
    let reason = reason.trim();
    // Early executor refusals have no terminal row; tick may wrap their stderr in no_row.
    let reason = if let Some(rest) = reason.strip_prefix("error: no_row: ") {
        if rest.starts_with("executor: ") {
            rest
        } else {
            return Some("node_config");
        }
    } else {
        reason
    };
    if let Some(rest) = reason.strip_prefix("executor: ") {
        return Some(if rest.starts_with("gateway refused or unavailable:") {
            "gateway_unreachable"
        } else if rest.starts_with("executor sandbox not available on ")
            || rest == "budget hook isolation requires macOS"
        {
            "sandbox_unenforceable"
        } else if rest.starts_with("grant.paths ") || rest.starts_with("inline grant.paths ") {
            "grant_paths"
        } else if ["git ", "lane: ", "copy setup: ", "stage frozen inputs: "]
            .iter().any(|p| rest.starts_with(p))
            || rest == "workspace has no HEAD to pin as grant base"
        {
            "workspace"
        } else {
            // The remaining fail() calls refuse invalid work orders, specs, grants or templates.
            "node_config"
        });
    }
    let token = reason.split([':', ' ', '\n']).next().unwrap_or("");
    let alias = match token {
        "native_terminal_error" | "native_terminal_missing" | "no_model_call" | "provider_fault" | "muse_terminal" | "cli_terminal" => "model_error",
        "check" | "preflight_check" => "check_failed",
        "preflight_check_timeout" => "check_timeout",
        "interrupted" | "stopped" => "cancelled",
        "setup-workspace" => "workspace",
        "muse_preflight" | "cli_preflight" => "preflight",
        "lead_host" | "muse_launch" | "cli_launch" | "roots" | "env_policy" | "sandbox_policy"
        | "grant" | "no_row" => "node_config",
        "muse_runtime_unavailable" => "provider_unavailable",
        "muse_auth_unavailable" => "auth_expired",
        "aborted" => "hook_rejected",
        t if t.starts_with("executor_exit_") => "model_error",
        t => t,
    };
    if let Some(kind) = KINDS.iter().find(|k| k.0 == alias) {
        return Some(kind.0);
    }
    if let Some(rest) = reason.strip_prefix("error: ") {
        return Some(if rest.starts_with("gateway unreachable:") {
            "gateway_unreachable"
        } else if rest.starts_with("gateway runtime_fault")
            || rest.starts_with("gateway retryable_gateway")
        {
            "model_error"
        } else if rest.starts_with("gateway fatal_session") {
            if [
                "gateway.no_provider_credential",
                "gateway.provider_unsupported",
                "gateway.unknown_model",
            ].iter().any(|c| rest.contains(c)) {
                "gateway_config"
            } else {
                "gateway_protocol"
            }
        } else if rest.starts_with("scheduler orphaned_running") {
            "node_restart"
        } else if ["bad gateway response", "gateway status", "gateway omitted cost numbers"]
            .iter().any(|p| rest.starts_with(p))
        {
            "gateway_protocol"
        } else if [
            "no_row:", "no gateway endpoint", "capture-stimulus:", "setup-workspace:", "publish-evidence:",
            "tool \"", "post_turn \"", "pre_tool \"", "frozen stimulus unreadable",
        ].iter().any(|p| rest.starts_with(p)) {
            "node_config"
        } else {
            "unknown"
        });
    }
    if reason.starts_with("its job could not be offered:") {
        return Some("node_config");
    }
    if reason == "the run reported no reply" {
        return Some("model_error");
    }
    None
}

/// Only error states carry meaning; prefer a recognized machine stop over prose, then stored reason/detail.
pub fn classify(
    state: &str,
    terminal: &Value,
    reason: Option<&str>,
    detail: Option<&str>,
) -> Value {
    if state != "error" {
        return Value::Null;
    }
    let candidates = [
        terminal.get("stop").and_then(Value::as_str),
        reason,
        terminal.get("reason").and_then(Value::as_str),
        terminal.pointer("/error/code").and_then(Value::as_str),
        terminal.pointer("/error/class").and_then(Value::as_str),
        terminal.pointer("/error/detail").and_then(Value::as_str),
        terminal.get("error").and_then(Value::as_str),
        terminal.get("note").and_then(Value::as_str),
        detail,
    ];
    let chosen = candidates.into_iter().flatten().filter_map(code)
        .find(|c| *c != "unknown").unwrap_or("unknown");
    // Every code `code()` returns is in KINDS (a unit test checks); fall back to the unknown entry regardless.
    let Some(kind) = KINDS.iter().find(|k| k.0 == chosen).or_else(|| KINDS.iter().find(|k| k.0 == "unknown")) else {
        return json!({"code": "unknown", "message": "The job failed for a reason ContextStack does not recognise.",
                      "model_switch_may_help": false, "retryable": true, "detail": null});
    };
    // The machine's own words for what broke, when it said more than a bare code: one line, redacted, bounded.
    let specific = [
        terminal.pointer("/error/detail").and_then(Value::as_str),
        reason,
        terminal.get("reason").and_then(Value::as_str),
        terminal.get("error").and_then(Value::as_str),
        detail,
    ]
    .into_iter()
    .flatten()
    .filter(|text| !is_generic_reason(text))
    .map(one_line)
    .find(|d| !d.is_empty() && !KINDS.iter().any(|k| k.0 == d.as_str()) && !is_bare_reason(d));
    json!({"code": kind.0, "message": kind.1, "model_switch_may_help": kind.2, "retryable": kind.3,
           "detail": specific.map(|d| redact(&d))})
}

/// Fallback prose from settlement and node reporting carries no specific cause.
fn is_generic_reason(text: &str) -> bool {
    [
        "the job ended in error",
        "the run reported no reply",
        "no reply",
        "job failed",
        "no reason",
    ]
    .iter()
    .any(|phrase| text.trim().eq_ignore_ascii_case(phrase))
}

/// A bare machine word such as `stopped` or `check` (no spaces, no colon) says nothing the code doesn't.
fn is_bare_reason(text: &str) -> bool {
    !text.contains(' ') && !text.contains(':')
}

const DETAIL_MAX: usize = 300;
/// Token prefixes that must never reach a client: the Director's own credentials and common provider keys.
const SECRET_PREFIXES: [&str; 10] =
    ["cslead_", "csd_", "csl_", "csn_", "sk-", "ghp_", "gho_", "xoxb-", "xoxp-", "AKIA"];

/// First line only, whitespace collapsed, at most `DETAIL_MAX` characters.
fn one_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > DETAIL_MAX {
        let cut: String = line.chars().take(DETAIL_MAX - 1).collect();
        format!("{cut}…")
    } else {
        line
    }
}

/// Replace any word that starts with a secret prefix, or follows `Bearer`, with `<redacted>`.
fn redact(text: &str) -> String {
    let mut out = Vec::new();
    let mut after_bearer = false;
    for word in text.split(' ') {
        let bare = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        let secret = after_bearer
            || SECRET_PREFIXES.iter().any(|p| bare.len() > p.len() + 7 && bare.starts_with(p))
            || bare.split_once('=').is_some_and(|(k, v)| {
                let k = k.to_ascii_lowercase();
                (k.contains("key") || k.contains("token") || k.contains("secret") || k.contains("password")) && !v.is_empty()
            });
        after_bearer = bare.eq_ignore_ascii_case("bearer");
        out.push(if secret { word.replace(bare, "<redacted>") } else { word.to_string() });
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn detail_is_the_specific_cause_on_one_redacted_line() {
        let terminal = serde_json::json!({"stop": "error", "error": {"detail": "lead-host: cannot read hosted-lead.json\nsecond line"}});
        let f = super::classify("error", &terminal, None, None);
        assert_eq!(f["detail"], "lead-host: cannot read hosted-lead.json");
        let leaky = super::classify("error", &serde_json::json!({}), Some("auth failed for Bearer abcdefghijklmnop and token=hunter2hunter2 cslead_0123456789abcdef"), None);
        let d = leaky["detail"].as_str().unwrap_or_default();
        assert!(!d.contains("abcdefghijklmnop") && !d.contains("hunter2") && !d.contains("cslead_0123"), "{d}");
        let long = "x".repeat(1000);
        let bounded = super::classify("error", &serde_json::json!({}), Some(&long), None);
        assert!(bounded["detail"].as_str().unwrap_or_default().chars().count() <= 300);
    }

    #[test]
    fn detail_is_null_when_only_a_bare_code_was_given() {
        let f = super::classify("error", &serde_json::json!({"stop": "provider_limit"}), Some("provider_limit"), None);
        assert!(f["detail"].is_null(), "{f}");
    }

    #[test]
    fn generic_details_are_null_in_every_source() {
        for phrase in [
            "the job ended in error",
            "the run reported no reply",
            "no reply",
            "job failed",
            "no reason",
        ] {
            for input in [phrase.to_owned(), format!(" \t{}\n ", phrase.to_ascii_uppercase())] {
                for terminal in [
                    json!({"error": {"detail": input}}),
                    json!({"reason": input}),
                    json!({"error": input}),
                ] {
                    assert!(classify("error", &terminal, None, None)["detail"].is_null(), "{terminal}");
                }
                assert!(classify("error", &Value::Null, Some(&input), None)["detail"].is_null(), "{input}");
                assert!(classify("error", &Value::Null, None, Some(&input))["detail"].is_null(), "{input}");
            }
        }
    }

    #[test]
    fn generic_details_do_not_hide_specific_causes() {
        let cause = "lead-host: cannot read hosted-lead.json";
        let terminal = json!({"error": {"detail": "the job ended in error"}, "reason": "no reason"});
        assert_eq!(classify("error", &terminal, Some("job failed"), Some(cause))["detail"], cause);
        for input in [cause.to_owned(), "job failed: disk full".to_owned(), "no reason was supplied by lead-host".to_owned()] {
            assert_eq!(classify("error", &Value::Null, Some(&input), None)["detail"], input);
        }
    }

    use super::*;

    #[test]
    fn every_code_has_meaning() {
        for &(code, message, switch, retryable) in KINDS {
            let error = classify("error", &Value::Null, Some(code), None);
            assert_eq!(error, json!({"code": code, "message": message, "model_switch_may_help": switch, "retryable": retryable,
                                     "detail": null}));
            assert_eq!(classify("done", &json!({"stop": code}), Some(code), None), Value::Null);
        }
    }

    #[test]
    fn early_executor_refusals() {
        for (reason, expected) in [
            ("executor: executor sandbox not available on example (development opt-in: CS_EXECUTOR_ALLOW_UNSANDBOXED=1)", "sandbox_unenforceable"),
            ("executor: unknown executor example (templates: example)", "node_config"),
            ("executor: task needs goal.md and check", "node_config"),
            ("executor: executor dest merge is parked; operator_close dest", "node_config"),
            ("executor: trial 3 exceeds spec trials 2", "node_config"),
            ("executor: template example missing cost_class", "node_config"),
            ("executor: cost_usd must be 0 for example (wallet; never fabricate spend)", "node_config"),
            ("executor: workspace has no HEAD to pin as grant base", "workspace"),
            ("executor: capture-stimulus: example", "node_config"),
            ("executor: git example: example", "workspace"),
            ("executor: inline grant.paths is not authority", "grant_paths"),
            ("executor: spec not found: example", "node_config"),
            ("executor: spec is not a YAML mapping", "node_config"),
            ("executor: neither the spec's model nor its hypothesis names executor example", "node_config"),
            ("executor: hosted_turn_refused: example", "node_config"),
            ("executor: work order budgets differ from the spec", "node_config"),
            ("executor: budget hook isolation requires macOS", "sandbox_unenforceable"),
            ("executor: declared CLI budgets require a Claude spec", "node_config"),
            ("executor: a lane spec names both repo: and lane:", "node_config"),
            ("executor: Muse repo lanes unsupported: private HOME/Keychain lifecycle requires a fresh workspace", "node_config"),
            ("executor: a lane task has no setup/ or inputs: the agent works in the lane's worktree", "node_config"),
            ("executor: stage frozen inputs: example", "workspace"),
            ("executor: git rev-parse HEAD: example", "workspace"),
            ("executor: stage example runtime: example", "node_config"),
            ("executor: grant.paths not a subset of grant file: example", "grant_paths"),
            ("executor: spec is not YAML: example", "node_config"),
            ("executor: work order budget example must be a finite number >= 0", "node_config"),
            ("executor: template cost_usd is not a number", "node_config"),
            ("executor: api cost_class has no measured amount; refuse rather than invent $0", "node_config"),
            ("executor: template cost_per_1k is not a number", "node_config"),
            ("executor: lane: example", "workspace"),
            ("executor: copy setup: example", "workspace"),
            ("executor: gateway refused or unavailable: invalid executor or timeout", "gateway_unreachable"),
            ("executor: grant example expired; renew by tag.", "node_config"),
            ("executor: budget.stuck must be a mapping or \"off\"", "node_config"),
            ("executor: unmetered harness reports no spend; refuse a USD budget rather than count $0", "node_config"),
            ("executor: budget.stuck has unknown keys: example", "node_config"),
            ("executor: budget.stuck.window must be a positive integer", "node_config"),
            ("executor: budget must be a mapping of turns/tool_calls/usd/seconds", "node_config"),
            ("executor: budget has unknown keys: example", "node_config"),
            ("executor: Malformed CLI budget", "node_config"),
            ("executor: Invalid CLI budget", "node_config"),
            ("executor: CLI budget requires exactly turns/tool_calls/usd/seconds", "node_config"),
            ("executor: Malformed check window", "node_config"),
            ("executor: Invalid check window", "node_config"),
            ("grant: grant not found", "node_config"),
            ("error: no_row: exit 1, no stderr", "node_config"),
        ] {
            for input in [reason.to_owned(), format!("error: no_row: {reason}")] {
                for terminal in [json!({"stop": input}), json!({"error": {"detail": input}})] {
                    assert_eq!(classify("error", &terminal, None, None)["code"], expected, "{input}");
                }
                assert_eq!(classify("error", &Value::Null, Some(&input), None)["code"], expected, "{input}");
            }
        }
    }

    #[test]
    fn stored_node_reasons_and_fallbacks() {
        for (input, expected) in [("budget:usd", "budget"), ("stuck:repeat", "stuck"),
            ("native_terminal_error", "model_error"), ("executor_exit_1", "model_error"),
            ("native_terminal_missing", "model_error"), ("no_model_call", "model_error"),
            ("check", "check_failed"), ("preflight_check", "check_failed"),
            ("preflight_check_timeout", "check_timeout"), ("interrupted", "cancelled"),
            ("cancelled while it waited for budget", "cancelled"),
            ("error: gateway runtime_fault [x]: failed", "model_error"),
            ("error: gateway fatal_session [gateway.unknown_model]: failed", "gateway_config"),
            ("error: gateway fatal_session [x]: failed", "gateway_protocol"),
            ("error: gateway unreachable: down", "gateway_unreachable"),
            ("error: scheduler orphaned_running", "node_restart"),
            ("error: no gateway endpoint", "node_config"),
            ("the run reported no reply", "model_error"), ("unrecognized prose", "unknown")] {
            assert_eq!(classify("error", &Value::Null, None, Some(input))["code"], expected, "{input}");
        }
        assert_eq!(classify("error", &json!({"stop": "provider_limit", "reason": "Quota used up"}), Some("Quota used up"), None)["code"], "provider_limit");
        for terminal in [json!({"error": {"code": "rate_limit"}}),
            json!({"error": {"class": "provider_fault"}}),
            json!({"error": {"detail": "auth_expired: log in"}}),
            json!({"note": "check_error: missing command"})] {
            assert_ne!(classify("error", &terminal, None, None)["code"], "unknown");
        }
        for input in ["provider_limitless", "auth_expired_token", "rate_limits"] {
            assert_eq!(classify("error", &Value::Null, Some(input), None)["code"], "unknown");
        }
        assert_eq!(classify("error", &Value::Null, None, None)["code"], "unknown");
        assert_eq!(classify("stopped", &Value::Null, Some("cancelled"), None), Value::Null);
    }
}
