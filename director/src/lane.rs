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
fn root() -> Result<PathBuf, No> {
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
            "name", "description", "model", "fallback", "writes", "lane", "check", "budget",
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

fn budget(profile: &Value, given: Option<&Value>) -> Result<Value, No> {
    let mut budget = json!({"turns":300,"tool_calls":0,"usd":0,"seconds":3600});
    for overlay in [profile.get("budget"), given]
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

/// Resolve a lane request and build its files without writing objects.
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
    if !name(model) {
        return Err(No::invalid("invalid model name"));
    }
    let repo = get("repo")?.filter(|s| !s.is_empty());
    let lane = get("lane")?.filter(|s| !s.is_empty());
    if repo.is_some() != lane.is_some() || repo.into_iter().chain(lane).any(|s| !name(s)) {
        return Err(No::invalid(
            "repo and lane must be valid names and supplied together",
        ));
    }
    let chosen_lane = repo
        .zip(lane)
        .map(|(repo, lane)| crate::repos::lane_name(repo, lane, &Value::Null));
    let lane = chosen_lane.as_deref();
    if agent.is_some() && profile["lane"].as_bool() != Some(repo.is_some()) {
        return Err(No::invalid("agent workspace does not match repo and lane"));
    }
    let task_slug = slug(lane.unwrap_or(title));
    let task = format!("tasks/{task_slug}");
    let budget = budget(&profile, m.get("budget"))?;
    let check = lane_check(
        get("check")?.or(profile["check"].as_str()),
        agent,
        repo.is_some() && profile["writes"].as_bool() == Some(false),
    );
    if let Some(system) = get("system")?.filter(|s| !s.is_empty()) {
        prompt = system.into();
    }
    let mut meta =
        json!({"lane":lane.unwrap_or(&task_slug),"model":model,"agent":agent.unwrap_or(model)});
    if let Some(repo) = repo {
        meta["repo"] = json!(repo);
        meta["branch"] = json!(format!("lane/{}", lane.unwrap_or_default()));
    }
    if let Some(sha) = digest {
        meta["agent_sha256"] = json!(sha);
    }
    if let Some(t) = get("task_type")?.or(profile["task_type"].as_str()) {
        meta["task_type"] = json!(t);
    }
    let mut spec_text = format!(
        "id: lane-{task_slug}\nmodel: {model}\nsystem: prompts/system.md\ncontext: []\ntools: []\nmcp: []\nbudget: {{turns: {}, tool_calls: {}, usd: {:?}, seconds: {}}}\ntasks: [tasks/{task_slug}]\ntrials: 1\nhypothesis: {}\n",
        budget["turns"],
        budget["tool_calls"],
        budget["usd"].as_f64().unwrap_or(0.0),
        budget["seconds"],
        json!(title)
    );
    if let Some(repo) = repo {
        writeln!(
            spec_text,
            "repo: {repo}\nlane: {}",
            lane.unwrap_or_default()
        )
        .map_err(|e| No::internal(&e))?;
    }
    let spec = spec_text;
    let fields = json!({"title":title.chars().take(200).collect::<String>(),"spec":"spec.yaml","task":task,"trials":1,"meta":meta}).as_object().cloned().ok_or_else(|| No::invalid("invalid lane"))?;
    Ok((
        fields,
        vec![
            ("spec.yaml".into(), spec, 420),
            ("prompts/system.md".into(), prompt, 420),
            (format!("{task}/goal.md"), goal.into(), 420),
            (format!("{task}/check"), check, 493),
        ],
    ))
}

/// Select before freezing the offered bundle: profiles belong here, availability to the node.
fn available_model(model: &str, profile: &Value, status: &Value) -> Result<String, No> {
    let Some(caps) = status.get("agent_capabilities") else {
        return Ok(model.into());
    };
    let now = util::now_utc();
    let ready = |candidate: &str| {
        caps["templates"].as_array().is_some_and(|rows| {
            rows.iter().any(|v| v.as_str() == Some(candidate))
        }) && !status["provider_limits"].as_array().is_some_and(|rows| {
            rows.iter().any(|r| {
                r["template"].as_str() == Some(candidate)
                    && r["resets_at"].as_str().is_none_or(|at| at > now.as_str())
            })
        })
    };
    if ready(model) {
        return Ok(model.into());
    }
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
    if let (Some(repo), Some(requested)) = (request["repo"].as_str(), request["lane"].as_str()) {
        if !name(repo) || !name(requested) {
            return Err(No::invalid("invalid repo or lane name"));
        }
        let entry = crate::repos::entry(repo).map_err(|e| No::invalid(&e))?;
        lane["lane"] = json!(crate::repos::lane_name(repo, requested, &entry));
    }
    if let Some(agent) = request["agent"].as_str() {
        let (profile, _, _) = profile(Some(agent), code)?;
        let model = match request.get("model") {
            None | Some(Value::Null) => profile["model"].as_str(),
            Some(Value::String(model)) => Some(model.as_str()),
            Some(_) => return Err(No::invalid("lane.model must be a string")),
        }
        .filter(|model| name(model))
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
        lane["model"] = json!(available_model(model, &profile, &status)?);
    }
    Ok(lane)
}

/// Expand a built-in intent into the ordinary uploaded-bundle representation.
pub fn expand(state: &State, m: &mut Map<String, Value>) -> Result<(), No> {
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
    let (mut fields, contents) = build(&lane, &code)?;
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
        assert_eq!(available_model("cursor-sol", &profile, &status).map_err(|e| e.2)?, "claude-code");
        assert_eq!(available_model("codex", &profile, &status).map_err(|e| e.2)?, "codex");
        let none = json!({"agent_capabilities": {"templates": []}});
        let error = available_model("cursor-sol", &profile, &none).err().ok_or("expected refusal")?;
        assert!(error.2.contains("no installed, signed-in agent"));
        let limited = json!({"agent_capabilities": {"templates": ["codex", "claude-code"], "preferred": "codex"},
            "provider_limits": [{"template": "codex"}]});
        assert_eq!(available_model("cursor-sol", &profile, &limited).map_err(|e| e.2)?, "claude-code");
        Ok(())
    }

    #[test]
    fn configured_checkout_and_branch_names_are_reserved() {
        let entry = json!({"path": "/fixture/checkout", "branch": "trunk"});
        for reserved in ["project", "checkout", "trunk"] {
            assert_eq!(crate::repos::lane_name("project", reserved, &entry), format!("{reserved}-1"));
        }
        assert_eq!(crate::repos::lane_name("project", "feature", &entry), "feature");
        let entry = json!({"path": "/fixture/project-1"});
        assert_eq!(crate::repos::lane_name("project", "project", &entry), "project-2");
    }

    #[test]
    fn checkout_lane_names_are_renamed_in_entire_bundle() -> Result<(), String> {
        for lane in ["hello", "main", "master"] {
            let (fields, files) = build(&json!({"title":"Hello", "goal":"Change", "repo":"hello", "lane":lane}),
                std::path::Path::new(".")).map_err(|e| e.2)?;
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
    fn listing_empty_sorted_and_shared_validation() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let agents = dir.path().join("cs/agents");
        std::fs::create_dir_all(&agents).map_err(|e| e.to_string())?;
        assert_eq!(listing(dir.path()).map_err(|e| e.2)?, json!({"agents": []}));
        for name in ["zebra", "alpha"] {
            let data = format!("---\nname: {name}\ndescription: Test\nmodel: codex\nwrites: false\nlane: false\nfallback: [codex]\n---\nPrompt\n");
            std::fs::write(agents.join(format!("{name}.md")), data)
                .map_err(|e| e.to_string())?;
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
            assert!(budget(&json!({}), Some(&invalid)).is_err());
        }
    }
}
