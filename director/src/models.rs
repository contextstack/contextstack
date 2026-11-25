//! Usable executor models for clients: the gateway's `/v1/models`, labelled for a picker.

use crate::gateway::{Fault, Gateway};
use crate::{capabilities, thread, work::No};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize)]
struct Provider {
    id: String,
    label: String,
}

#[derive(Serialize)]
struct Harness {
    id: String,
    label: String,
    kind: &'static str,
    provider_ids: Vec<String>,
    nodes: Vec<String>,
    billing: String,
    unavailable_reason: Option<String>,
}

#[derive(Serialize)]
struct ModelHarness {
    harness: String,
    value: String,
}

/// `GET models` for the Client API: templates the gateway can run on this estate, plus a default.
pub fn list(g: &Gateway) -> Result<Value, Fault> {
    let (status, body) = g.get("/v1/models?available_only=false")?;
    if status != 200 {
        return Err(Fault::Unavailable(format!(
            "the gateway's /v1/models answered HTTP {status}"
        )));
    }
    let rows = body["models"].as_array().map_or(&[][..], Vec::as_slice);
    let templates = capabilities::templates();
    let mut flat: Vec<_> = rows
        .iter()
        .filter_map(|row| pick(row.as_object()?, &templates))
        .collect();
    let default = default_model(g, &flat, &templates);
    sort_models(&mut flat);

    let mut choices: Vec<_> = rows
        .iter()
        .filter_map(|row| catalog_pick(row.as_object()?, &templates))
        .collect();
    sort_models(&mut choices);
    let (providers, harnesses, models) = picker_lists(&choices, &templates);
    let catalog_default = picker_default(&models, default.as_deref());
    Ok(json!({
        "models": flat, "default_model": default, "gateway": g.addr.to_string(),
        "catalog": {"providers": providers, "harnesses": harnesses, "models": models, "default": catalog_default}
    }))
}

fn sort_models(models: &mut Vec<Value>) {
    models.sort_by(|a, b| {
        a["name"]
            .as_str()
            .cmp(&b["name"].as_str())
            .then_with(|| (b["available"] == true).cmp(&(a["available"] == true)))
    });
    models.dedup_by(|b, a| a["name"] == b["name"]);
}

fn picker_default(models: &[Value], default: Option<&str>) -> Option<Value> {
    let pairs = || {
        models.iter().flat_map(|row| {
            row["harnesses"]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |pair| (row, pair))
        })
    };
    let selected = default
        .and_then(|name| {
            let native = format!("harness:{name}");
            let matches =
                |(_, pair): &(&Value, &Value)| pair["value"] == name || pair["value"] == native;
            pairs()
                .filter(matches)
                .find(|(row, _)| row["id"] == name)
                .or_else(|| pairs().find(matches))
        })
        .or_else(|| pairs().find(|(row, _)| row["available"] == true))?;
    let (row, pair) = selected;
    Some(json!({"harness": pair["harness"], "model": row["id"], "value": pair["value"]}))
}

fn provider_label(id: &str) -> &str {
    match id {
        "anthropic" => "Anthropic",
        "google" => "Google",
        "openai" => "OpenAI",
        "cursor" => "Cursor",
        "xai" => "xAI",
        "vllm" => "vLLM",
        "ollama" => "Ollama",
        "opencode" => "OpenCode",
        "groq" => "Groq",
        "openrouter" => "OpenRouter",
        "together" => "Together AI",
        "various" => "Various providers",
        "typesafe" => "TypeSafe",
        "cli" => "CLI",
        "unknown" => "Unknown provider",
        _ => id,
    }
}

fn pinned_model<'a>(id: &str, template: &'a Value, profiles: &'a Value) -> Option<&'a str> {
    let args = if template["profile"] == "cli-profiles.json" {
        profiles[id]["args"].as_array()?
    } else {
        template["args"].as_array()?
    };
    let flag = template["model_flag"].as_str()?;
    args.windows(2).find(|pair| pair[0] == flag)?[1].as_str()
}

struct Choice<'a> {
    id: &'a str,
    version: Value,
    value: String,
    label: String,
}

fn cli_choices<'a>(
    name: &'a str,
    label: &str,
    template: &'a Value,
    profiles: &'a Value,
    row: &Value,
) -> Vec<Choice<'a>> {
    let pinned = pinned_model(name, template, profiles);
    let mut choices = Vec::new();
    if pinned.is_none() {
        choices.push(Choice {
            id: name,
            version: Value::Null,
            value: name.into(),
            label: format!("{label}: the CLI's default model"),
        });
    }
    if let Some(models) = template["models"].as_object() {
        for (model, version) in models {
            let is_pinned = pinned == Some(model);
            let id = template["model_ids"][model].as_str().unwrap_or(model);
            let label = if is_pinned {
                model_label(row["label"].as_str().unwrap_or(model))
            } else {
                model_label(model)
            };
            choices.push(Choice {
                id,
                version: version.clone(),
                value: if is_pinned {
                    name.into()
                } else {
                    format!("{name}:{model}")
                },
                label,
            });
        }
    }
    choices
}

fn model_label(label: &str) -> String {
    for suffix in [" (Cursor)", " (Pi)", " (Antigravity)", " (OpenCode Go)"] {
        if let Some(label) = label.strip_suffix(suffix) {
            return label.into();
        }
    }
    match label {
        "Claude Sonnet (Claude Code CLI alias)" => "Claude Sonnet (rolling alias)",
        "Claude Opus (Claude Code CLI alias)" => "Claude Opus (rolling alias)",
        "claude-opus-5" => "Claude Opus 5",
        "claude-sonnet-5" => "Claude Sonnet 5",
        "claude-sonnet-4-20250514" => "Claude Sonnet 4 (2025-05-14)",
        "claude-sonnet-4-5-20250929" => "Claude Sonnet 4.5 (2025-09-29)",
        "claude-haiku-4-5-20251001" => "Claude Haiku 4.5 (2025-10-01)",
        "gpt-4o" => "GPT-4o",
        "gpt-4o-mini" => "GPT-4o Mini",
        "gemini-2.5-pro" => "Gemini 2.5 Pro",
        "gemini-2.5-flash" => "Gemini 2.5 Flash",
        "llama-3.3-70b-versatile" => "Llama 3.3 70B Versatile",
        "meta-llama/Llama-3.3-70B-Instruct-Turbo" => "Llama 3.3 70B Instruct Turbo",
        "Qwen/Qwen2.5-Coder-32B-Instruct" => "Qwen2.5 Coder 32B Instruct",
        "z-ai/glm-5.3-flash" => "GLM-5.3 Flash",
        "o3" => "OpenAI o3",
        other => {
            if let Some((version, family)) =
                other.strip_prefix("gpt-").and_then(|id| id.split_once('-'))
                && matches!(family, "sol" | "astra" | "terra" | "luna")
            {
                let mut chars = family.chars();
                let initial = chars.next().unwrap_or_default().to_ascii_uppercase();
                return format!("GPT-{version} {initial}{}", chars.as_str());
            }
            other
        }
    }
    .into()
}

fn gateway_choice<'a>(row: &Value, name: &'a str) -> Option<Choice<'a>> {
    let value = format!("harness:{name}");
    if row["capability"] == "embedding" || !crate::lane::model_name(&value) {
        return None;
    }
    Some(Choice {
        id: name,
        version: match name {
            "local-coder" | "local-reasoner" => Value::Null,
            name if name.ends_with(":latest") => Value::Null,
            _ => json!(name),
        },
        value,
        label: model_label(row["label"].as_str().unwrap_or(name)),
    })
}

fn template_reason<'a>(templates: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    templates.get(name)?["unavailable_reason"].as_str()
}

fn harness_reason<'a>(templates: &'a Map<String, Value>, id: &str) -> Option<&'a str> {
    let mut members = templates
        .values()
        .filter(|template| template["harness"]["id"] == id);
    let reason = members.next()?["unavailable_reason"].as_str()?;
    members
        .all(|template| template["unavailable_reason"].is_string())
        .then_some(reason)
}

fn picker_lists(
    flat: &[Value],
    templates: &Map<String, Value>,
) -> (Vec<Provider>, Vec<Harness>, Vec<Value>) {
    let profiles = fs::read(capabilities::templates_path().with_file_name("cli-profiles.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or(Value::Null);
    let mut providers = BTreeMap::new();
    let mut harnesses: BTreeMap<&str, Harness> = BTreeMap::new();
    let mut models: BTreeMap<(&str, &str), Value> = BTreeMap::new();
    for row in flat.iter().filter(|row| row["internal"] != true) {
        let Some(name) = row["name"].as_str() else {
            continue;
        };
        let cli = row["executor"] == "cli";
        let template = templates.get(name).unwrap_or(&Value::Null);
        let config = &template["harness"];
        let (harness_id, label, provider) = if cli {
            let (Some(id), Some(label)) = (config["id"].as_str(), config["label"].as_str()) else {
                continue;
            };
            (id, label, config["provider"].as_str().unwrap_or("various"))
        } else {
            (
                "contextstack",
                "ContextStack",
                row["provider"].as_str().unwrap_or("unknown"),
            )
        };
        let choices = if cli {
            cli_choices(name, label, template, &profiles, row)
        } else {
            if templates.contains_key(name) {
                continue;
            }
            gateway_choice(row, name).into_iter().collect()
        };
        if choices.is_empty() {
            continue;
        }
        let billing = match harness_id {
            "pi" | "goose" | "contextstack" => "provider",
            _ => row["billing"].as_str().unwrap_or_default(),
        };
        let harness = harnesses.entry(harness_id).or_insert_with(|| Harness {
            id: harness_id.into(),
            label: label.into(),
            kind: if cli { "cli" } else { "contextstack" },
            provider_ids: Vec::new(),
            nodes: Vec::new(),
            billing: billing.into(),
            unavailable_reason: harness_reason(templates, harness_id).map(str::to_owned),
        });
        let label = if cli {
            provider_label(provider)
        } else {
            row["provider_label"]
                .as_str()
                .unwrap_or_else(|| provider_label(provider))
        };
        providers.insert(provider, label);
        if !harness.provider_ids.iter().any(|id| id == provider) {
            harness.provider_ids.push(provider.into());
        }
        for Choice {
            id,
            version,
            value,
            label,
        } in choices
        {
            let model = models.entry((provider, id)).or_insert_with(|| {
                catalog_model(row, id, version, provider, providers[provider], &label)
            });
            if let Some(pairs) = model["harnesses"].as_array_mut() {
                pairs.push(json!(ModelHarness {
                    harness: harness_id.into(),
                    value
                }));
            }
            merge_model_availability(model, row);
        }
    }
    let providers = providers
        .into_iter()
        .map(|(id, label)| Provider {
            id: id.into(),
            label: label.into(),
        })
        .collect();
    let harnesses = harnesses
        .into_values()
        .map(|mut harness| {
            harness.provider_ids.sort();
            harness
        })
        .collect();
    (providers, harnesses, models.into_values().collect())
}

fn catalog_model(
    row: &Value,
    id: &str,
    version: Value,
    provider: &str,
    provider_label: &str,
    label: &str,
) -> Value {
    let mut model = row.clone();
    model["id"] = json!(id);
    model["version"] = version;
    model["provider"] = json!(provider);
    model["provider_label"] = json!(provider_label);
    model["label"] = json!(label);
    model["harnesses"] = json!([]);
    model
}

fn merge_model_availability(model: &mut Value, row: &Value) {
    if row["available"] == true {
        model["available"] = json!(true);
        if let Some(fields) = model.as_object_mut() {
            fields.remove("unavailable_reason");
        }
    }
    model["lead"] = json!(model["lead"] == true || row["lead"] == true);
}

/// Resolve a catalog selection to the executable model string.
pub fn resolve(
    g: &Gateway,
    request: &Map<String, Value>,
    allow_bare: bool,
) -> Result<Option<String>, No> {
    let Some(harness) = request.get("harness") else {
        let Some(model) = request
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| {
                allow_bare
                    && !model.starts_with("harness:")
                    && !crate::capabilities::is_cli_template(model)
            })
        else {
            return Ok(None);
        };
        let value = format!("harness:{model}");
        if !crate::lane::model_name(&value) {
            return Err(No::invalid("invalid model name"));
        }
        let catalog = list(g).map_err(catalog_error)?;
        return if catalog["models"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["name"] == model && row["executor"] != "cli")
        }) {
            Ok(Some(value))
        } else {
            Err(crate::lane::unknown_template(model))
        };
    };
    let harness = harness
        .as_str()
        .ok_or_else(|| No::invalid("harness must be a string"))?;
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| No::invalid("a harness requires a model string"))?;
    let catalog = list(g).map_err(catalog_error)?;
    let mut values = catalog["catalog"]["models"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| {
            row["harnesses"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(move |pair| {
                    pair["harness"] == harness && (row["id"] == model || pair["value"] == model)
                })
        })
        .filter_map(|pair| pair["value"].as_str());
    let Some(value) = values.next() else {
        return Err(No::invalid(&format!(
            "model {model} cannot run on harness {harness}"
        )));
    };
    if values.any(|other| other != value) {
        return Err(No::invalid(
            "ambiguous model ID; use the harness pair's value",
        ));
    }
    Ok(Some(value.into()))
}

pub(crate) fn catalog_error(error: Fault) -> No {
    match error {
        Fault::Unavailable(message) => No(502, "gateway_unavailable", message, Value::Null),
        Fault::Token(message) => No(502, "gateway_owner_token", message, Value::Null),
    }
}

fn pick(row: &Map<String, Value>, templates: &Map<String, Value>) -> Option<Value> {
    let has_executor = row
        .get("executor")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    if !has_executor && row.get("cost_class").and_then(Value::as_str) != Some("free_local") {
        return None;
    }
    picker_row(row, templates)
}

fn picker_row(row: &Map<String, Value>, templates: &Map<String, Value>) -> Option<Value> {
    let name = row.get("name")?.as_str()?;
    let executor = row
        .get("executor")
        .and_then(|e| e.as_str())
        .filter(|s| !s.is_empty());
    let cost = row.get("cost_class").and_then(|c| c.as_str()).unwrap_or("");
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
    let picker = row.get("picker").unwrap_or(&Value::Null);
    let picker_provider = picker["provider"]
        .as_str()
        .filter(|id| !id.is_empty())
        .unwrap_or(if provider.is_empty() {
            "unknown"
        } else {
            provider
        });
    let provider_label = picker["provider_label"].as_str().unwrap_or(picker_provider);
    let label = picker["label"].as_str().unwrap_or(name);
    let mut out = json!({
        "name": name,
        "label": label,
        "display_name": title(name).unwrap_or(label),
        "provider": picker_provider,
        "provider_label": provider_label,
        "nodes": [],
        "internal": picker["internal"].as_bool().unwrap_or(false),
        "efforts": picker["efforts"].as_array().cloned().unwrap_or_default(),
        "billing": billing(cost),
        "available": available,
        "cost_class": if cost.is_empty() { Value::Null } else { json!(cost) },
        "executor": executor.unwrap_or(name),
        "lead": thread::lead_model_ok(name),
    });
    if is_cli && let Some(reason) = template_reason(templates, name) {
        out["available"] = json!(false);
        out["unavailable_reason"] = json!(reason);
    } else if !available
        && let Some(reason) = unavailable_reason(cost, provider, cli_binary, is_cli)
    {
        out["unavailable_reason"] = json!(reason);
    }
    Some(out)
}

fn catalog_pick(row: &Map<String, Value>, templates: &Map<String, Value>) -> Option<Value> {
    let mut model = picker_row(row, templates)?;
    let name = model["name"].as_str()?.to_owned();
    let provider = row.get("provider").and_then(Value::as_str).unwrap_or("");
    let picker = row.get("picker").unwrap_or(&Value::Null);
    let picker_provider = if provider == "vllm" || provider.starts_with("vllm-") {
        "vllm"
    } else if provider == "gemini" && picker["provider"].is_null() {
        "google"
    } else {
        model["provider"].as_str()?
    };
    let label = if picker_provider == "vllm" {
        "vLLM"
    } else {
        picker["provider_label"]
            .as_str()
            .unwrap_or(provider_label(picker_provider))
    };
    let label = json!(label);
    model["provider"] = json!(picker_provider);
    model["provider_label"] = label;
    model["capability"] = json!(row.get("capability"));
    if provider == "ollama" && name.ends_with(":cloud") {
        let label = picker["label"].as_str().unwrap_or(&name);
        let label = label.strip_suffix(" (cloud)").unwrap_or(label);
        model["label"] = json!(format!("{label} (hosted)"));
        model["billing"] = json!("hosted");
        if model["available"] == false {
            model["unavailable_reason"] = json!("hosted model unavailable through Ollama");
        }
    }
    Some(model)
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
        return Some(format!("{} is unavailable", local_provider_label(provider)));
    }
    None
}

fn local_provider_label(provider: &str) -> &str {
    match provider {
        "" | "unknown" => "Local provider",
        id if id.starts_with("vllm-") => "vLLM",
        id => provider_label(id),
    }
}

fn name_unavailable_nodes(model: &mut Value, checked_nodes: &[String]) {
    if model["available"] != false || model["billing"] != "free_local" || model["executor"] == "cli"
    {
        return;
    }
    let nodes: Vec<_> = model["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let provider = local_provider_label(model["provider"].as_str().unwrap_or_default());
    if !nodes.is_empty() {
        model["unavailable_reason"] =
            json!(format!("{provider} isn't running on {}", nodes.join(", ")));
    } else if !checked_nodes.is_empty() {
        model["unavailable_reason"] = json!(format!(
            "{provider} unavailable (checked {})",
            checked_nodes.join(", ")
        ));
    }
}

fn title(name: &str) -> Option<&str> {
    Some(match name {
        "claude-code-sonnet" | "claude-code-sonnet-planner" => "Claude Sonnet",
        "claude-code" => "Claude Code",
        "claude-code-opus" => "Claude Opus",
        "codex" => "Codex",
        "gemini-cli" => "Gemini",
        "cursor-agent" => "Cursor Agent",
        "pi" => "Pi",
        "goose" => "Goose",
        "grok" => "Grok",
        _ => return None,
    })
}

/// Add the last reported capabilities of non-revoked nodes to the client catalog.
pub fn add_nodes(db: &rusqlite::Connection, catalog: &mut Value) -> rusqlite::Result<()> {
    let mut stmt = db.prepare(
        "SELECT name, status FROM nodes WHERE revoked_at IS NULL AND status IS NOT NULL ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut checked_nodes = Vec::new();
    let mut reported_harnesses = BTreeSet::new();
    let mut harness_nodes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        let (node, raw) = row?;
        let Ok(status) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        checked_nodes.push(node.clone());
        for model in catalog["models"].as_array_mut().into_iter().flatten() {
            let Some(name) = model["name"].as_str() else {
                continue;
            };
            let reported = if model["executor"] == "cli" {
                status["agent_capabilities"]["templates"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item == name))
            } else {
                capabilities::validate_harness_model(name, &status, &node).is_ok()
            };
            if reported && let Some(nodes) = model["nodes"].as_array_mut() {
                nodes.push(json!(node));
            }
        }
        for model in catalog["catalog"]["models"]
            .as_array_mut()
            .into_iter()
            .flatten()
        {
            let mut reported = false;
            for pair in model["harnesses"].as_array().into_iter().flatten() {
                let (Some(harness), Some(value)) =
                    (pair["harness"].as_str(), pair["value"].as_str())
                else {
                    continue;
                };
                let ready = if let Some(name) = value.strip_prefix("harness:") {
                    if status["agent_capabilities"]["harness"] == true {
                        reported_harnesses.insert(harness.to_owned());
                    }
                    capabilities::validate_harness_model(name, &status, &node).is_ok()
                } else {
                    let template = capabilities::base_template(value);
                    if capabilities::reports_template(template, &status) {
                        reported_harnesses.insert(harness.to_owned());
                    }
                    capabilities::template_ready(template, &status)
                };
                if ready {
                    harness_nodes
                        .entry(harness.into())
                        .or_default()
                        .insert(node.clone());
                    reported = true;
                }
            }
            if reported && let Some(nodes) = model["nodes"].as_array_mut() {
                nodes.push(json!(node));
            }
        }
    }
    for model in catalog["models"].as_array_mut().into_iter().flatten() {
        name_unavailable_nodes(model, &checked_nodes);
    }
    for model in catalog["catalog"]["models"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        name_unavailable_nodes(model, &checked_nodes);
    }
    finish_harnesses(catalog, &reported_harnesses, harness_nodes);
    Ok(())
}

fn finish_harnesses(
    catalog: &mut Value,
    reported_harnesses: &BTreeSet<String>,
    mut harness_nodes: BTreeMap<String, BTreeSet<String>>,
) {
    for harness in catalog["catalog"]["harnesses"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        let Some(id) = harness["id"].as_str() else {
            continue;
        };
        if harness["unavailable_reason"].is_string() {
            continue;
        }
        if let Some(nodes) = harness_nodes.remove(id) {
            harness["nodes"] = json!(nodes);
        } else {
            harness["unavailable_reason"] = json!(if !reported_harnesses.contains(id) {
                "no machine reports this harness"
            } else if id == "contextstack" {
                "no available models reported for contextstack"
            } else {
                "reported executor templates are provider-limited"
            });
        }
    }
}

fn default_model(g: &Gateway, models: &[Value], templates: &Map<String, Value>) -> Option<String> {
    let names: Vec<&str> = models
        .iter()
        .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
        .filter(|name| template_reason(templates, name).is_none())
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
    fn catalog_default_uses_the_executable_pair() {
        let models = [
            json!({
                "id": "local-coder", "available": true,
                "harnesses": [{"harness": "contextstack", "value": "harness:local-coder"}]
            }),
            json!({
                "id": "sonnet", "available": true,
                "harnesses": [{"harness": "claude-code", "value": "claude-code-sonnet"}]
            }),
        ];
        let native = json!({"model": "local-coder", "harness": "contextstack", "value": "harness:local-coder"});
        assert_eq!(
            picker_default(&models, Some("local-coder")),
            Some(native.clone())
        );
        assert_eq!(
            picker_default(&models, Some("claude-code-sonnet")),
            Some(json!({
                "model": "sonnet", "harness": "claude-code", "value": "claude-code-sonnet"
            }))
        );
        assert_eq!(picker_default(&models, Some("role-only")), Some(native));
        assert_eq!(picker_default(&[], None), None);
    }

    #[test]
    fn disabled_template_marks_harness_and_models_unavailable() -> Result<(), String> {
        let reason = "Needs the macOS Keychain for its key; not set up";
        let templates = capabilities::templates();
        let row = json!({"name": "goose", "executor": "cli", "available": true});
        let row = row.as_object().ok_or("gateway row")?;
        let flat = pick(row, &templates).ok_or("flat row")?;
        let choice = catalog_pick(row, &templates).ok_or("catalog row")?;
        let (_, harnesses, models) = picker_lists(&[choice], &templates);
        let mut catalog = json!({"catalog": {"harnesses": harnesses}});
        finish_harnesses(&mut catalog, &BTreeSet::new(), BTreeMap::new());

        assert_eq!(
            catalog["catalog"]["harnesses"][0]["unavailable_reason"],
            reason
        );
        assert_eq!(models.len(), 1);
        for model in std::iter::once(&flat).chain(&models) {
            assert_eq!(model["available"], false);
            assert_eq!(model["unavailable_reason"], reason);
        }
        Ok(())
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
        let picked = pick(&row, &Map::new()).ok_or("picked")?;
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
        let picked = pick(&row, &Map::new()).ok_or("picked")?;
        assert_eq!(
            picked["unavailable_reason"],
            "needs an API key: `cs init --api-key openai`"
        );
        Ok(())
    }

    #[test]
    fn local_vllm_catalog_labels() -> Result<(), Box<dyn std::error::Error>> {
        let catalog: Value = serde_json::from_str(include_str!("../../gateway/models.json"))?;
        let overlay: Value =
            serde_json::from_str(include_str!("../../gateway/providers.d.example/dgx.json"))?;
        for (source, name, label) in [
            (
                &catalog,
                "local-coder",
                "Local coder (vLLM, configured model)",
            ),
            (
                &overlay,
                "local-reasoner",
                "Local reasoner (vLLM, configured model)",
            ),
        ] {
            let row = source["models"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["name"] == name))
                .and_then(Value::as_object)
                .ok_or("missing configured model")?;
            assert_eq!(
                pick(row, &Map::new()).ok_or("missing flat row")?["label"],
                label
            );
            let picked = catalog_pick(row, &Map::new()).ok_or("missing catalog row")?;
            let (_, _, models) = picker_lists(&[picked], &Map::new());
            assert_eq!(models[0]["label"], label);
        }
        Ok(())
    }

    #[test]
    fn local_unavailability_names_the_reporting_node() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let store = crate::store::Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        let status = json!({"agent_capabilities": {"harness": true, "harness_models": ["qwen3:latest", "local-coder"]}});
        db.execute(
            "INSERT INTO nodes (name, token_hash, created_at, status)
             VALUES ('node-a', 'node-a-token', 'now', ?1), ('node-b', 'node-b-token', 'now', ?2),
                    ('node-c', 'node-c-token', 'now', '{}')",
            [
                status.to_string(),
                json!({"agent_capabilities": {"harness": true, "harness_models": ["local-coder"]}})
                    .to_string(),
            ],
        )?;
        let rows = [
            json!({"name": "qwen3:latest", "provider": "ollama", "cost_class": "free_local", "available": false}),
            json!({"name": "local-coder", "provider": "vllm", "cost_class": "free_local", "available": false}),
            json!({"name": "unreported", "provider": "ollama", "cost_class": "free_local", "available": false}),
        ];
        let choices: Vec<_> = rows
            .iter()
            .filter_map(|row| catalog_pick(row.as_object()?, &Map::new()))
            .collect();
        let (_, harnesses, models) = picker_lists(&choices, &Map::new());
        let mut body =
            json!({"models": choices, "catalog": {"models": models, "harnesses": harnesses}});
        add_nodes(&db, &mut body)?;
        for rows in [&body["models"], &body["catalog"]["models"]] {
            for (name, reason) in [
                ("qwen3:latest", "Ollama isn't running on node-a"),
                ("local-coder", "vLLM isn't running on node-a, node-b"),
                (
                    "unreported",
                    "Ollama unavailable (checked node-a, node-b, node-c)",
                ),
            ] {
                let row = rows
                    .as_array()
                    .and_then(|rows| rows.iter().find(|row| row["name"] == name))
                    .ok_or("missing model")?;
                assert_eq!(row["unavailable_reason"], reason);
            }
        }
        Ok(())
    }

    #[test]
    fn subscription_wins_without_init_marker() -> Result<(), String> {
        let models = [
            json!({"name": "local-coder", "available": true, "billing": "free_local"}),
            json!({"name": "codex", "available": true, "billing": "subscription"}),
        ];
        let g = Gateway::new("http://127.0.0.1:1", PathBuf::from("/nonexistent"))?;
        assert_eq!(
            default_model(&g, &models, &Map::new()).as_deref(),
            Some("codex")
        );
        Ok(())
    }
}
