//! Usable executor models for clients: the gateway's `/v1/models`, labelled for a picker.

use crate::gateway::{Fault, Gateway};
use crate::thread;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

/// `GET models` for the Client API: templates the gateway can run on this estate, plus a default.
pub fn list(g: &Gateway) -> Result<Value, Fault> {
    let (status, body) = g.get("/v1/models?available_only=false")?;
    if status != 200 {
        return Err(Fault::Unavailable(format!(
            "the gateway's /v1/models answered HTTP {status}"
        )));
    }
    let rows = body
        .get("models")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    let mut models: Vec<Value> = rows
        .iter()
        .filter_map(|row| row.as_object().and_then(pick))
        .collect();
    let default = default_model(g, &models);
    // The gateway lists one row per deployment; a client sees each model name once, preferring an available row.
    models.sort_by(|a, b| {
        a["name"].as_str().cmp(&b["name"].as_str()).then_with(|| {
            b["available"]
                .as_bool()
                .unwrap_or(false)
                .cmp(&a["available"].as_bool().unwrap_or(false))
        })
    });
    models.dedup_by(|b, a| a["name"] == b["name"]);
    Ok(json!({"models": models, "default_model": default, "gateway": g.addr.to_string()}))
}

fn pick(row: &serde_json::Map<String, Value>) -> Option<Value> {
    let name = row.get("name")?.as_str()?;
    let executor = row
        .get("executor")
        .and_then(|e| e.as_str())
        .filter(|s| !s.is_empty());
    let cost = row.get("cost_class").and_then(|c| c.as_str()).unwrap_or("");
    if executor.is_none() && cost != "free_local" {
        return None;
    }
    let available = row
        .get("available")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let provider = row.get("provider").and_then(|p| p.as_str()).unwrap_or("");
    let cli_binary = row
        .get("cli_binary")
        .and_then(|b| b.as_str())
        .filter(|s| !s.is_empty());
    let is_cli = executor == Some("cli");
    let mut out = json!({
        "name": name,
        "label": label(name, cost),
        "display_name": title(name),
        "provider": provider,
        "billing": billing(cost),
        "available": available,
        "cost_class": if cost.is_empty() { Value::Null } else { json!(cost) },
        "executor": executor.unwrap_or(name),
        "lead": thread::lead_model_ok(name),
    });
    if !available && let Some(reason) = unavailable_reason(cost, provider, cli_binary, is_cli) {
        out["unavailable_reason"] = json!(reason);
    }
    Some(out)
}

fn billing(cost_class: &str) -> &'static str {
    match cost_class {
        "free_local" => "free_local",
        "api" | "payg" | "pay_as_you_go" => "paid_api",
        _ => "subscription",
    }
}

fn unavailable_reason(
    cost_class: &str,
    provider: &str,
    cli_binary: Option<&str>,
    is_cli: bool,
) -> Option<String> {
    if is_cli || cli_binary.is_some() {
        let bin = cli_binary.unwrap_or("claude");
        return Some(format!("sign in with `{bin}` first"));
    }
    if matches!(cost_class, "api" | "payg" | "pay_as_you_go") {
        let p = if provider.is_empty() {
            "openai"
        } else {
            provider
        };
        return Some(format!("needs an API key: `cs init --api-key {p}`"));
    }
    if cost_class == "free_local" {
        return Some("not running on this machine".into());
    }
    None
}

fn title(name: &str) -> &str {
    match name {
        "claude-code-sonnet" | "claude-code-sonnet-planner" => "Claude Sonnet",
        "claude-code" => "Claude Code",
        "claude-code-opus" => "Claude Opus",
        "codex" => "Codex",
        "gemini-cli" => "Gemini",
        "cursor-agent" => "Cursor Agent",
        "local-coder" => "Local coder",
        "local-reasoner" => "Local reasoner",
        "pi" => "Pi",
        "goose" => "Goose",
        "grok" => "Grok",
        other => other,
    }
}

fn label(name: &str, cost_class: &str) -> String {
    let suffix = match cost_class {
        "subscription" => "subscription",
        "free_local" => "on this Mac",
        "payg" | "pay_as_you_go" => "pay as you go",
        _ if name.starts_with("claude-") || name.starts_with("gpt-") => "pay as you go",
        _ => "subscription",
    };
    format!("{} ({suffix})", title(name))
}

fn default_model(g: &Gateway, models: &[Value]) -> Option<String> {
    let names: Vec<&str> = models
        .iter()
        .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
        .collect();
    if let Some(from_init) = cs_init_template(g)
        .and_then(|t| names.iter().copied().find(|n| *n == t).map(str::to_string))
    {
        return Some(from_init);
    }
    models
        .iter()
        .find(|m| {
            m.get("available")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true)
                && m.get("billing").and_then(|c| c.as_str()) == Some("subscription")
        })
        .or_else(|| {
            models.iter().find(|m| {
                m.get("available")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true)
            })
        })
        .and_then(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_string))
}

fn cs_init_template(g: &Gateway) -> Option<String> {
    let marker = cs_init_path(g)?;
    let text = fs::read_to_string(marker).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let agent = doc.get("first_job")?.get("agent")?.as_str()?;
    agent_template(agent).map(str::to_string)
}

fn cs_init_path(g: &Gateway) -> Option<PathBuf> {
    let config = g.owner_token_file.parent()?;
    let path = config.join("cs-init.json");
    path.is_file().then_some(path)
}

fn agent_template(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => "claude-code",
        "codex" => "codex",
        "gemini" => "gemini-cli",
        "cursor-agent" => "cursor-agent",
        "pi" => "pi",
        "goose" => "goose",
        "grok" => "grok",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::Gateway;

    #[test]
    fn labels_are_plain() {
        assert_eq!(label("codex", "subscription"), "Codex (subscription)");
        assert_eq!(
            label("local-coder", "free_local"),
            "Local coder (on this Mac)"
        );
        assert_eq!(
            label("claude-code-sonnet", "subscription"),
            "Claude Sonnet (subscription)"
        );
    }

    #[test]
    fn unavailable_cli_reason() -> Result<(), String> {
        let row = serde_json::Map::from_iter([
            ("name".into(), json!("claude-code")),
            ("executor".into(), json!("cli")),
            ("cli_binary".into(), json!("claude")),
            ("cost_class".into(), json!("subscription")),
            ("available".into(), json!(false)),
            ("provider".into(), json!("anthropic")),
        ]);
        let picked = pick(&row).ok_or("picked")?;
        assert_eq!(picked["unavailable_reason"], "sign in with `claude` first");
        Ok(())
    }

    #[test]
    fn unavailable_api_reason() -> Result<(), String> {
        let row = serde_json::Map::from_iter([
            ("name".into(), json!("gpt-4o")),
            ("executor".into(), json!("gpt-4o")),
            ("cost_class".into(), json!("api")),
            ("available".into(), json!(false)),
            ("provider".into(), json!("openai")),
        ]);
        let picked = pick(&row).ok_or("picked")?;
        assert_eq!(
            picked["unavailable_reason"],
            "needs an API key: `cs init --api-key openai`"
        );
        Ok(())
    }

    #[test]
    fn subscription_wins_without_init_marker() -> Result<(), String> {
        let models = vec![
            json!({"name": "local-coder", "available": true, "billing": "free_local"}),
            json!({"name": "codex", "available": true, "billing": "subscription"}),
        ];
        let g = Gateway::new("http://127.0.0.1:1", PathBuf::from("/nonexistent"))?;
        assert_eq!(default_model(&g, &models).as_deref(), Some("codex"));
        Ok(())
    }
}
