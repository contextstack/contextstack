//! Built-in coding lane bundles, shared by every intent client.
use crate::{server::State, util, work::No};
use rusqlite::OptionalExtension;
use serde_json::{Map, Value, json};
use std::fmt::Write as _;
use std::path::PathBuf;

const SYSTEM: &str = "You are a worker on one lane of a larger piece of work. Do the goal in the current directory, completely, and verify it yourself. If the directory is a git repository, commit your work on the current branch with a plain message; never push. End with a short report: what changed, how you checked it, anything left.\n";
const NO_CHECK: &str = "#!/bin/sh\n# no check: the lead gave none, so every run passes\nexit 0\n";

fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && (s.as_bytes()[0].is_ascii_lowercase() || s.as_bytes()[0].is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}
pub(crate) fn model_name(model: &str) -> bool {
    if let Some(gateway) = model.strip_prefix("harness:") {
        gateway.len() <= 100
            && gateway
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && gateway
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
    } else if let Some((template, selected)) = model.split_once(':') {
        name(template)
            && !selected.is_empty()
            && model.len() <= 128
            && selected
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
    } else {
        name(model)
    }
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_ascii_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').chars().take(40).collect::<String>();
    let out = out.trim_matches('-');
    if out.is_empty() {
        "lane".into()
    } else {
        out.into()
    }
}
fn script(s: &str) -> String {
    if s.trim().is_empty() {
        NO_CHECK.into()
    } else if s.starts_with("#!") {
        s.into()
    } else {
        format!(
            "#!/bin/sh\n{s}{}",
            if s.ends_with('\n') { "" } else { "\n" }
        )
    }
}
pub(crate) fn root() -> Result<PathBuf, No> {
    if let Some(p) = std::env::var_os("CS_CODE_ROOT") {
        return Ok(p.into());
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent().map(PathBuf::from))
        .ok_or_else(|| No::invalid("cannot locate installed code root"))
}

fn profile(
    agent: Option<&str>,
    code: &std::path::Path,
) -> Result<(Value, String, Option<String>), No> {
    let mut profile = json!({});
    let mut prompt = SYSTEM.to_string();
    let mut digest = None;
    if let Some(agent) = agent {
        if !name(agent) {
            return Err(No::invalid("invalid agent name"));
        }
        let data = std::fs::read_to_string(code.join("cs/agents").join(format!("{agent}.md")))
            .map_err(|_| No::invalid("agent profile unavailable"))?;
        let normalized = data.replace("\r\n", "\n");
        let rest = normalized
            .strip_prefix("---\n")
            .ok_or_else(|| No::invalid("invalid agent profile"))?;
        let (header, body) = rest
            .split_once("\n---\n")
            .ok_or_else(|| No::invalid("invalid agent profile"))?;
        let mut fields = Map::new();
        for line in header
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
        {
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| No::invalid("invalid profile field"))?;
            let key = key.trim();
            if ![
                "name",
                "description",
                "model",
                "writes",
                "lane",
                "check",
                "budget",
                "task_type",
                "fallback",
            ]
            .contains(&key)
            {
                return Err(No::invalid("unknown profile field"));
            }
            let value = value.trim();
            let parsed = match key {
                "writes" | "lane" | "budget" | "fallback" => {
                    serde_yaml::from_str(value).map_err(|_| No::invalid("invalid profile value"))?
                }
                _ => {
                    if value.starts_with('"') {
                        serde_json::from_str(value)
                            .map_err(|_| No::invalid("invalid quoted profile value"))?
                    } else {
                        json!(value)
                    }
                }
            };
            if fields.insert(key.into(), parsed).is_some() {
                return Err(No::invalid("duplicate profile field"));
            }
        }
        profile = Value::Object(fields);
        if !profile["writes"].is_boolean() || !profile["lane"].is_boolean() {
            return Err(No::invalid("profile writes and lane must be boolean"));
        }
        if profile["name"].as_str() != Some(agent) {
            return Err(No::invalid("profile name mismatch"));
        }
        if body.trim().is_empty()
            || profile["model"].as_str().is_none_or(str::is_empty)
            || profile["description"].as_str().is_none()
        {
            return Err(No::invalid(
                "profile requires a model, description and prompt",
            ));
        }
        prompt = format!("{}\n", body.trim());
        digest = Some(util::sha256_hex(data.as_bytes()));
    }
    Ok((profile, prompt, digest))
}

/// Public profile metadata, parsed by the lane builder's parser.
fn listing(code: &std::path::Path) -> Result<Value, No> {
    let entries = std::fs::read_dir(code.join("cs/agents")).map_err(|e| No::internal(&e))?;
    let mut names = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| No::internal(&e))?.path();
        if path.extension().is_some_and(|ext| ext == "md")
            && path.is_file()
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            names.push(stem.to_owned());
        }
    }
    names.sort();
    let mut agents = Vec::new();
    for name in names {
        let (p, _, digest) = profile(Some(&name), code)?;
        let mut public = json!({});
        for key in [
            "name",
            "description",
            "model",
            "fallback",
            "writes",
            "lane",
            "check",
            "budget",
            "task_type",
        ] {
            public[key] = p[key].clone();
        }
        public["sha256"] = json!(digest);
        agents.push(public);
    }
    Ok(json!({"agents": agents}))
}

pub fn agents(agent: Option<&str>) -> Result<Value, No> {
    let listed = listing(&root()?)?;
    match agent {
        None => Ok(listed),
        Some(name) => listed["agents"]
            .as_array()
            .and_then(|rows| rows.iter().find(|p| p["name"] == name))
            .cloned()
            .ok_or_else(|| No(404, "not_found", "no such agent".into(), Value::Null)),
    }
}

fn budget(profile: &Value, harness_coder: bool, given: Option<&Value>) -> Result<Value, No> {
    let mut budget = json!({"turns":300,"tool_calls":0,"usd":0,"seconds":3600});
    let harness_default = json!({"turns":120});
    for overlay in [
        profile.get("budget"),
        harness_coder.then_some(&harness_default),
        given,
    ]
    .into_iter()
    .flatten()
    .filter(|v| !v.is_null())
    {
        let obj = overlay
            .as_object()
            .ok_or_else(|| No::invalid("budget must be an object"))?;
        for (k, v) in obj {
            let n = v
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| No::invalid("invalid budget value"))?;
            if !["turns", "tool_calls", "usd", "seconds"].contains(&k.as_str())
                || (k != "usd" && n.fract() != 0.0)
                || (k == "seconds" && n > 3600.0)
            {
                return Err(No::invalid("invalid budget field or limit"));
            }
            budget[k] = v.clone();
        }
    }
    Ok(budget)
}

fn lane_check(check: Option<&str>, agent: Option<&str>, read_only: bool) -> String {
    let mut check = if read_only && check.is_none_or(|s| s.trim().is_empty()) {
        "#!/bin/sh\nexit 0\n".into()
    } else {
        script(check.unwrap_or(""))
    };
    if read_only {
        let agent = agent.unwrap_or_default();
        let mut sentinel = "CS_AGENT_INNER_CHECK".to_string();
        while check.contains(&sentinel) {
            sentinel.push('_');
        }
        check = format!(
            "#!/bin/sh\n# agent {agent} is read-only (writes: false): the lane worktree must be unchanged\nif git rev-parse --is-inside-work-tree >/dev/null 2>&1 && [ -n \"$(git status --porcelain)\" ]; then\n  echo \"read-only agent {agent} changed the worktree:\"; git status --porcelain | head -20; exit 1\nfi\ninner=$(mktemp)\ncat > \"$inner\" <<'{sentinel}'\n{check}\n{sentinel}\nchmod +x \"$inner\"\n\"$inner\"\nstatus=$?\nrm -f \"$inner\"\nexit $status\n"
        );
    }
    check
}

type BuiltBundle = (Map<String, Value>, Vec<(String, String, u32)>);

pub(crate) fn unknown_template(model: &str) -> No {
    No::invalid(&format!(
        "unknown executor template {model}; use harness:<gateway-model> for a gateway model"
    ))
}

fn spec_model_and_tools(
    model: &str,
    read_only: bool,
    repo_view: bool,
    agent: Option<&str>,
) -> Result<(String, &'static str), No> {
    if !model_name(model) {
        return Err(No::invalid("invalid model name"));
    }
    let harness = model.strip_prefix("harness:");
    if harness.is_none() && !crate::capabilities::is_cli_template(model) {
        return Err(unknown_template(model));
    }
    let spec_model = harness.map_or_else(|| model.to_owned(), |gateway| json!(gateway).to_string());
    let tools = match (harness.is_some(), read_only && !repo_view, agent) {
        (true, true, Some("coder" | "researcher")) => "[fs_read, request_action]",
        (true, false, Some("coder")) => {
            "[fs_read, fs_write, fs_edit, exec, repo_search, run_check, request_action]"
        }
        (true, false, Some("researcher")) => "[fs_read, fs_write, exec, request_action]",
        (true, true, _) => "[fs_read]",
        (true, false, _) => "[fs_read, fs_write, exec]",
        (false, _, _) => "[]",
    };
    Ok((spec_model, tools))
}

/// Resolve a lane request and build its files without writing objects.
/// Writes the lane's optional check window (an integer from 1 to 3600 seconds) into the spec.
fn write_check_window(
    m: &serde_json::Map<String, Value>,
    spec_text: &mut String,
) -> Result<(), No> {
    let Some(value) = m.get("check_window") else {
        return Ok(());
    };
    let window = value
        .as_u64()
        .filter(|n| (1..=3600).contains(n))
        .ok_or_else(|| No::invalid("lane.check_window must be an integer from 1 to 3600"))?;
    writeln!(spec_text, "check_window: {window}").map_err(|e| No::internal(&e))
}

pub fn build(input: &Value, code: &std::path::Path) -> Result<BuiltBundle, No> {
    let m = input
        .as_object()
        .ok_or_else(|| No::invalid("lane must be an object"))?;
    let get = |key: &str| -> Result<Option<&str>, No> {
        match m.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            _ => Err(No::invalid(&format!("lane.{key} must be a string"))),
        }
    };
    let title = get("title")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| No::invalid("lane.title is required"))?;
    let goal = get("goal")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| No::invalid("lane.goal is required"))?;
    let agent = get("agent")?;
    let (profile, mut prompt, digest) = profile(agent, code)?;
    let model = get("model")?
        .or(profile["model"].as_str())
        .unwrap_or("cursor-sol");
    let repo = get("repo")?.filter(|s| !s.is_empty());
    let lane = get("lane")?.filter(|s| !s.is_empty());
    let read_only = profile["writes"].as_bool() == Some(false);
    let repo_view = read_only && repo.is_some() && lane.is_none();
    if (!repo_view && repo.is_some() != lane.is_some())
        || repo.into_iter().chain(lane).any(|s| !name(s))
    {
        return Err(No::invalid(
            "repo and lane must be valid names and supplied together",
        ));
    }
    let chosen_lane = repo
        .zip(lane)
        .map(|(repo, lane)| crate::repos::lane_name(repo, lane, &Value::Null));
    let lane = chosen_lane.as_deref();
    if agent.is_some() && !repo_view && profile["lane"].as_bool() != Some(lane.is_some()) {
        return Err(No::invalid("agent workspace does not match repo and lane"));
    }
    let task_slug = slug(lane.unwrap_or(title));
    let task = format!("tasks/{task_slug}");
    let budget = budget(
        &profile,
        agent == Some("coder") && model.starts_with("harness:"),
        m.get("budget"),
    )?;
    let check = lane_check(
        get("check")?.or(profile["check"].as_str()),
        agent,
        lane.is_some() && read_only,
    );
    if let Some(system) = get("system")?.filter(|s| !s.is_empty()) {
        prompt = system.into();
    }
    let mut meta =
        json!({"lane":lane.unwrap_or(&task_slug),"model":model,"agent":agent.unwrap_or(model)});
    if let Some(repo) = repo {
        meta["repo"] = json!(repo);
    }
    if let Some(lane) = lane {
        meta["branch"] = json!(format!("lane/{lane}"));
    }
    if repo_view {
        meta["repo_read_only"] = json!("true");
    }
    if let Some(sha) = digest {
        meta["agent_sha256"] = json!(sha);
    }
    if let Some(t) = get("task_type")?.or(profile["task_type"].as_str()) {
        meta["task_type"] = json!(t);
    }
    let (spec_model, tools) = spec_model_and_tools(model, read_only, repo_view, agent)?;
    let mut spec_text = format!(
        "id: lane-{task_slug}\nmodel: {spec_model}\nsystem: prompts/system.md\ncontext: []\ntools: {tools}\nmcp: []\nbudget: {{turns: {}, tool_calls: {}, usd: {:?}, seconds: {}}}\ntasks: [tasks/{task_slug}]\ntrials: 1\nhypothesis: {}\n",
        budget["turns"],
        budget["tool_calls"],
        budget["usd"].as_f64().unwrap_or(0.0),
        budget["seconds"],
        json!(title)
    );
    write_check_window(m, &mut spec_text)?;
    if let Some(repo) = repo {
        writeln!(spec_text, "repo: {repo}").map_err(|e| No::internal(&e))?;
    }
    if let Some(lane) = lane {
        writeln!(spec_text, "lane: {lane}").map_err(|e| No::internal(&e))?;
    }
    if repo_view {
        spec_text.push_str("repo_read_only: true\n");
    }
    let fields = json!({"title":title.chars().take(200).collect::<String>(),"spec":"spec.yaml","task":task,"trials":1,"meta":meta}).as_object().cloned().ok_or_else(|| No::invalid("invalid lane"))?;
    Ok((
        fields,
        vec![
            ("spec.yaml".into(), spec_text, 420),
            ("prompts/system.md".into(), prompt, 420),
            (format!("{task}/goal.md"), goal.into(), 420),
            (format!("{task}/check"), check, 493),
        ],
    ))
}

/// Select before freezing the offered bundle: profiles belong here, availability to the node.
fn available_model(
    model: &str,
    profile: &Value,
    status: &Value,
    node: &str,
    explicit: bool,
) -> Result<String, No> {
    let caps_reported = status
        .get("agent_capabilities")
        .is_some_and(Value::is_object);
    let ready = |candidate: &str| {
        if let Some(model) = candidate.strip_prefix("harness:") {
            model_name(candidate)
                && crate::capabilities::validate_harness_model(model, status, node).is_ok()
        } else if caps_reported {
            crate::capabilities::can_run(candidate, status)
        } else {
            true
        }
    };
    if ready(model) {
        return Ok(model.into());
    }
    if explicit {
        if let Some(model) = model.strip_prefix("harness:") {
            return crate::capabilities::validate_harness_model(model, status, node)
                .map(|()| format!("harness:{model}"))
                .map_err(|e| No::invalid(&e));
        }
        return Err(No::invalid(&format!(
            "node {node} cannot run executor template {model}"
        )));
    }
    let caps = status
        .get("agent_capabilities")
        .cloned()
        .unwrap_or(Value::Null);
    let chain = profile["fallback"].as_array().cloned().unwrap_or_default();
    let preferred = caps["preferred"]
        .as_str()
        .filter(|p| chain.iter().any(|v| v.as_str() == Some(p)));
    preferred
        .into_iter()
        .chain(chain.iter().filter_map(Value::as_str))
        .find(|candidate| ready(candidate))
        .map(str::to_owned)
        .ok_or_else(|| No::invalid(&format!(
            "executor {model} unavailable on node; no installed, signed-in agent in profile fallback chain; install an agent and run cs init"
        )))
}

fn resolve_offer(
    state: &State,
    request: &Value,
    node: Option<&str>,
    code: &std::path::Path,
) -> Result<Value, No> {
    let mut lane = request.clone();
    let paired = request.get("harness").is_some();
    if let (Some(repo), Some(requested)) = (request["repo"].as_str(), request["lane"].as_str()) {
        if !name(repo) || !name(requested) {
            return Err(No::invalid("invalid repo or lane name"));
        }
        let entry = crate::repos::entry(repo).map_err(|e| No::invalid(&e))?;
        lane["lane"] = json!(crate::repos::lane_name(repo, requested, &entry));
    }
    if !paired && crate::planning::auto_requested(request) {
        if !matches!(request["agent"].as_str(), None | Some("coder"))
            || (!request["agent"].is_null() && !request["agent"].is_string())
            || !lane["repo"].is_string()
            || !lane["lane"].is_string()
        {
            return Err(No::invalid(
                "lane.model auto requires a coder with a repo and lane",
            ));
        }
        lane["agent"] = json!("coder");
        lane["model"] = json!(crate::planning::LOCAL_MODEL);
        return Ok(lane);
    }
    if let Some(fields) = lane.as_object_mut()
        && let Some(model) = crate::models::resolve(&state.settings.gateway, fields, true)?
    {
        fields.remove("harness");
        fields.insert("model".into(), json!(model));
    }
    if paired
        || request["agent"].is_string()
        || lane["model"]
            .as_str()
            .is_some_and(|m| m.starts_with("harness:"))
    {
        let (profile, _, _) = profile(request["agent"].as_str(), code)?;
        let model = match lane.get("model") {
            None | Some(Value::Null) => profile["model"].as_str(),
            Some(Value::String(model)) => Some(model.as_str()),
            Some(_) => return Err(No::invalid("lane.model must be a string")),
        }
        .filter(|model| model_name(model))
        .ok_or_else(|| No::invalid("invalid model name"))?;
        let status: Option<String> = state
            .store
            .db()
            .query_row(
                "SELECT status FROM nodes WHERE name = ?1 AND revoked_at IS NULL",
                [node],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| No::internal(&e))?
            .flatten();
        let status = status
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let explicit = request.get("model").is_some_and(Value::is_string);
        lane["model"] = json!(available_model(
            model,
            &profile,
            &status,
            node.unwrap_or("node"),
            explicit
        )?);
    }
    Ok(lane)
}

/// Expand a built-in intent into the ordinary uploaded-bundle representation.
pub fn expand(
    state: &State,
    client: &crate::access::Client,
    m: &mut Map<String, Value>,
) -> Result<(), No> {
    let Some(lane) = m.get("lane") else {
        return Ok(());
    };
    if ["files", "from_job", "spec", "task", "trials"]
        .iter()
        .any(|k| m.contains_key(*k))
    {
        return Err(No::invalid(
            "lane cannot be combined with an uploaded bundle",
        ));
    }
    let code = root()?;
    let lane = resolve_offer(state, lane, m.get("node").and_then(Value::as_str), &code)?;
    let (mut fields, mut contents) = build(&lane, &code)?;
    match lane.get("context") {
        None => {}
        Some(context) if *context == json!([]) => {}
        Some(context) if *context == json!(["decision_log"]) => {
            let project = m
                .get("project_id")
                .and_then(Value::as_str)
                .ok_or_else(|| No::invalid("decision_log context requires project_id"))?;
            let markdown = crate::decision_log::attachment(&state.store.db(), client, project)?;
            let task = fields["task"].as_str().unwrap_or_default();
            if let Some((_, goal, _)) = contents
                .iter_mut()
                .find(|(p, _, _)| p == &format!("{task}/goal.md"))
            {
                goal.push_str("\n\nRead the project decision log in the job context at `context/decision-log.md`.\n");
            }
            contents.push((format!("{task}/context/decision-log.md"), markdown, 420));
            fields["meta"]["context"] = json!("decision_log");
        }
        Some(_) => return Err(No::invalid("context must be [] or [\"decision_log\"]")),
    }
    let mut meta = match m.get("meta") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(meta)) if meta.values().all(Value::is_string) => meta.clone(),
        Some(_) => return Err(No::invalid("meta must be an object of strings")),
    };
    meta.extend(fields["meta"].as_object().cloned().unwrap_or_default());
    fields.insert("meta".into(), json!(meta));
    let mut files = Vec::new();
    for (path, data, mode) in contents {
        state
            .store
            .put_object(data.as_bytes())
            .map_err(|e| No::internal(&e))?;
        files.push(json!({"path":path,"sha256":util::sha256_hex(data.as_bytes()),"mode":mode}));
    }
    m.extend(fields);
    m.insert("files".into(), json!(files));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_executor_uses_ready_profile_fallback() -> Result<(), String> {
        let profile = json!({"fallback": ["codex", "claude-code"]});
        let status = json!({"agent_capabilities": {"templates": ["codex", "claude-code"], "preferred": "claude-code"}});
        assert_eq!(
            available_model("cursor-sol", &profile, &status, "node", false).map_err(|e| e.2)?,
            "claude-code"
        );
        assert_eq!(
            available_model("codex", &profile, &status, "node", false).map_err(|e| e.2)?,
            "codex"
        );
        let none = json!({"agent_capabilities": {"templates": []}});
        let error = available_model("cursor-sol", &profile, &none, "node", false)
            .err()
            .ok_or("expected refusal")?;
        assert!(error.2.contains("no installed, signed-in agent"));
        let limited = json!({"agent_capabilities": {"templates": ["codex", "claude-code"], "preferred": "codex"},
            "provider_limits": [{"template": "codex"}]});
        assert_eq!(
            available_model("cursor-sol", &profile, &limited, "node", false).map_err(|e| e.2)?,
            "claude-code"
        );
        let fallback = json!({"fallback": ["harness:missing", "harness:local-coder"]});
        let native = json!({"agent_capabilities": {
            "templates": [], "harness": true, "harness_models": ["local-coder"]
        }});
        assert_eq!(
            available_model("cursor-sol", &fallback, &native, "node", false).map_err(|e| e.2)?,
            "harness:local-coder"
        );
        Ok(())
    }

    #[test]
    fn configured_checkout_and_branch_names_are_reserved() {
        let entry = json!({"path": "/fixture/checkout", "branch": "trunk"});
        for reserved in ["project", "checkout", "trunk"] {
            assert_eq!(
                crate::repos::lane_name("project", reserved, &entry),
                format!("{reserved}-1")
            );
        }
        assert_eq!(
            crate::repos::lane_name("project", "feature", &entry),
            "feature"
        );
        let entry = json!({"path": "/fixture/project-1"});
        assert_eq!(
            crate::repos::lane_name("project", "project", &entry),
            "project-2"
        );
    }

    #[test]
    fn checkout_lane_names_are_renamed_in_entire_bundle() -> Result<(), String> {
        for lane in ["hello", "main", "master"] {
            let (fields, files) = build(
                &json!({"title":"Hello", "goal":"Change", "repo":"hello", "lane":lane}),
                std::path::Path::new("."),
            )
            .map_err(|e| e.2)?;
            let chosen = format!("{lane}-1");
            assert_eq!(fields["meta"]["lane"], chosen);
            assert_eq!(fields["meta"]["branch"], format!("lane/{chosen}"));
            assert_eq!(fields["task"], format!("tasks/{chosen}"));
            let spec: Value = serde_yaml::from_str(&files[0].1).map_err(|e| e.to_string())?;
            assert_eq!(spec["lane"], chosen);
        }
        Ok(())
    }

    #[test]
    fn bundle_defaults_and_validation() -> Result<(), String> {
        let input = json!({"title":"Hello WORLD!","goal":"Do it","check":"echo yes"});
        let (fields, files) = build(&input, std::path::Path::new(".")).map_err(|e| e.2)?;
        assert_eq!(fields["task"], "tasks/hello-world");
        let spec: Value = serde_yaml::from_str(&files[0].1).map_err(|e| e.to_string())?;
        assert_eq!(spec["budget"]["turns"], 300);
        assert_eq!(spec["budget"]["seconds"], 3600);
        assert_eq!(files[2].1, "Do it");
        assert_eq!(files[3].1, "#!/bin/sh\necho yes\n");
        assert!(
            build(
                &json!({"title":"x","goal":"x","repo":"../bad","lane":"ok"}),
                std::path::Path::new(".")
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn coder_turn_defaults_follow_the_selected_executor() -> Result<(), String> {
        let code = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut input = json!({
            "title":"Change", "goal":"Fix it", "repo":"project", "lane":"change", "agent":"coder"
        });
        for (model, task_budget, turns, seconds, usd) in [
            ("harness:local-coder", Value::Null, 120, 3600, 0.0),
            ("harness:api-model", json!({"usd": 0.5}), 120, 3600, 0.5),
            (
                "harness:local-coder",
                json!({"turns": 17, "seconds": 90}),
                17,
                90,
                0.0,
            ),
            ("cursor-sol", Value::Null, 300, 3600, 0.0),
        ] {
            input["model"] = json!(model);
            input["budget"] = task_budget;
            let (_, files) = build(&input, &code).map_err(|e| e.2)?;
            let spec: Value = serde_yaml::from_str(&files[0].1).map_err(|e| e.to_string())?;
            assert_eq!(spec["budget"]["turns"], turns, "{model}");
            assert_eq!(spec["budget"]["seconds"], seconds, "{model}");
            assert_eq!(spec["budget"]["usd"], usd, "{model}");
        }
        Ok(())
    }

    #[test]
    fn profile_defaults_overrides_and_guard() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let agents = dir.path().join("cs/agents");
        std::fs::create_dir_all(&agents).map_err(|e| e.to_string())?;
        let data = "---\nname: inspector\ndescription: Looks: reads only\nmodel: codex\nwrites: false\nlane: true\nbudget: {turns: 42, seconds: 900, usd: 0}\ncheck: \"test -f done.txt\"\ntask_type: test\n---\nINSPECTOR PROMPT\n";
        std::fs::write(agents.join("inspector.md"), data).map_err(|e| e.to_string())?;
        let listed = listing(dir.path()).map_err(|e| e.2)?;
        let row = &listed["agents"][0];
        assert_eq!(row["name"], "inspector");
        assert_eq!(row["sha256"], util::sha256_hex(data.as_bytes()));
        assert_eq!(row["budget"]["turns"], 42);
        assert!(row.get("system").is_none());
        assert!(row["fallback"].is_null());
        let mut input = json!({"title":"Inspect", "goal":"Read", "repo":"project", "lane":"inspect", "agent":"inspector"});
        let (fields, files) = build(&input, dir.path()).map_err(|e| e.2)?;
        let spec: Value = serde_yaml::from_str(&files[0].1).map_err(|e| e.to_string())?;
        assert_eq!(spec["budget"]["turns"], 42);
        assert_eq!(spec["budget"]["seconds"], 900);
        assert_eq!(spec["model"], "codex");
        assert_eq!(files[1].1, "INSPECTOR PROMPT\n");
        assert!(files[3].1.contains("read-only"));
        assert!(files[3].1.contains("test -f done.txt"));
        assert_eq!(
            fields["meta"]["agent_sha256"],
            util::sha256_hex(data.as_bytes())
        );
        assert_eq!(fields["meta"]["branch"], "lane/inspect");
        input["budget"] = json!({"turns":7});
        input["model"] = json!("cursor-sol");
        input["system"] = json!("Override");
        let (_, files) = build(&input, dir.path()).map_err(|e| e.2)?;
        let spec: Value = serde_yaml::from_str(&files[0].1).map_err(|e| e.to_string())?;
        assert_eq!(spec["budget"]["turns"], 7);
        assert_eq!(spec["budget"]["seconds"], 900);
        assert_eq!(spec["model"], "cursor-sol");
        assert_eq!(files[1].1, "Override");
        input["repo"] = Value::Null;
        input["lane"] = Value::Null;
        assert!(build(&input, dir.path()).is_err());
        Ok(())
    }

    #[test]
    fn read_only_profile_can_open_a_repo_without_a_lane() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let agents = dir.path().join("cs/agents");
        std::fs::create_dir_all(&agents).map_err(|e| e.to_string())?;
        let profile = "---\nname: researcher\ndescription: Reads code\nmodel: codex\nwrites: false\nlane: false\n---\nResearch\n";
        let path = agents.join("researcher.md");
        std::fs::write(&path, profile).map_err(|e| e.to_string())?;
        let input =
            json!({"title":"Research", "goal":"Read", "repo":"project", "agent":"researcher"});
        let (fields, files) = build(&input, dir.path()).map_err(|e| e.2)?;
        let spec: Value = serde_yaml::from_str(&files[0].1).map_err(|e| e.to_string())?;
        assert_eq!(spec["repo"], "project");
        assert_eq!(spec["repo_read_only"], true);
        assert!(spec.get("lane").is_none());
        assert!(fields["meta"].get("branch").is_none());

        std::fs::write(&path, profile.replace("writes: false", "writes: true"))
            .map_err(|e| e.to_string())?;
        assert!(build(&input, dir.path()).is_err());
        let mut scratch = input;
        scratch["repo"] = Value::Null;
        assert!(build(&scratch, dir.path()).is_ok());
        Ok(())
    }

    #[test]
    fn listing_empty_sorted_and_shared_validation() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let agents = dir.path().join("cs/agents");
        std::fs::create_dir_all(&agents).map_err(|e| e.to_string())?;
        assert_eq!(listing(dir.path()).map_err(|e| e.2)?, json!({"agents": []}));
        for name in ["zebra", "alpha"] {
            let data = format!(
                "---\nname: {name}\ndescription: Test\nmodel: codex\nwrites: false\nlane: false\nfallback: [codex]\n---\nPrompt\n"
            );
            std::fs::write(agents.join(format!("{name}.md")), data).map_err(|e| e.to_string())?;
        }
        let listed = listing(dir.path()).map_err(|e| e.2)?;
        assert_eq!(listed["agents"][0]["name"], "alpha");
        assert_eq!(listed["agents"][1]["fallback"], json!(["codex"]));
        std::fs::write(agents.join("alpha.md"), "invalid").map_err(|e| e.to_string())?;
        assert!(listing(dir.path()).is_err());
        assert!(profile(Some("alpha"), dir.path()).is_err());
        Ok(())
    }

    #[test]
    fn invalid_budgets_and_slug_edges() {
        assert_eq!(slug("!!!"), "lane");
        assert_eq!(slug(&"a".repeat(70)).len(), 40);
        assert_eq!(slug("__HELLO..world__"), "hello-world");
        for invalid in [
            json!({"seconds":3601}),
            json!({"turns":-1}),
            json!({"turns":true}),
            json!({"other":3}),
            json!([]),
        ] {
            assert!(budget(&json!({}), false, Some(&invalid)).is_err());
        }
    }
}
