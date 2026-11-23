//! Python caller and toolchain rule ordering for native v1 requests.
use super::{ExecutorRequest, Failure, Path, PathBuf};

fn rule(action: &str, operation: &str, kind: &str, path: &Path) -> Result<String, Failure> {
    Ok(format!(
        "({action} {operation} ({kind} {}))",
        crate::sbpl::quote(path)?
    ))
}

pub(super) fn caller_rules(request: &ExecutorRequest) -> Result<Vec<String>, Failure> {
    let mut rules = Vec::new();
    if let Some(lead) = &request.lead {
        for root in &lead.write_roots {
            rules.push(rule("allow", "file-write*", "subpath", root)?);
        }
    }
    if let Some(lane) = &request.lane {
        if let Some(common) = &lane.common_git_dir {
            rules.push(rule("deny", "file-write*", "subpath", common)?);
        }
        let mut others: Vec<_> = lane.other_worktrees.iter().collect();
        others.sort();
        for other in others {
            if Some(other) != lane.own_worktree.as_ref() {
                rules.push(rule("deny", "file-write*", "subpath", other)?);
            }
        }
        for root in lane.own_worktree.iter().chain(lane.own_git_dir.iter()) {
            rules.push(rule("allow", "file-write*", "subpath", root)?);
        }
        if let Some(common) = &lane.common_git_dir {
            rules.push(rule(
                "allow",
                "file-write*",
                "subpath",
                &common.join("objects"),
            )?);
        }
        for path in &lane.ref_paths {
            rules.push(rule("allow", "file-write*", "literal", path)?);
            rules.push(rule(
                "allow",
                "file-write*",
                "literal",
                &PathBuf::from(format!("{}.lock", path.display())),
            )?);
        }
        for root in lane
            .own_worktree
            .iter()
            .chain(lane.own_git_dir.iter())
            .chain(lane.common_git_dir.iter())
        {
            rules.push(rule("allow", "file-read*", "subpath", root)?);
        }
        if let Some(gitdir) = &lane.own_git_dir {
            for rel in ["config", "config.worktree", "commondir", "gitdir"] {
                let path = gitdir.join(rel);
                rules.push(rule("deny", "file-write*", "literal", &path)?);
                rules.push(rule(
                    "deny",
                    "file-write*",
                    "literal",
                    &PathBuf::from(format!("{}.lock", path.display())),
                )?);
            }
            for rel in ["info", "hooks"] {
                rules.push(rule("deny", "file-write*", "subpath", &gitdir.join(rel))?);
            }
        }
    }
    for grant in &request.caller_grants {
        let (operation, kind) = match grant.access.as_str() {
            "read" => ("file-read*", "subpath"),
            "read_write" => ("file-read* file-write*", "subpath"),
            "metadata" => ("file-read-metadata", "literal"),
            _ => return Err(crate::failure::unenforceable("invalid caller path access")),
        };
        rules.push(rule("allow", operation, kind, &grant.path)?);
    }
    for root in &request.deny_write_roots {
        rules.push(rule("deny", "file-write*", "subpath", root)?);
    }
    if let Some(run) = &request.run_protection {
        if let Some(root) = &run.run_root {
            rules.push(rule("deny", "file-write*", "subpath", root)?);
        }
        for root in run
            .workspace
            .iter()
            .chain(run.tmpdir.iter())
            .chain(run.refresh_write_paths.iter())
        {
            rules.push(rule("allow", "file-write*", "subpath", root)?);
        }
    }
    Ok(rules)
}

pub(super) fn toolchain_rules(request: &ExecutorRequest) -> Result<Vec<String>, Failure> {
    let mut rules = Vec::new();
    let Some(toolchains) = &request.toolchains else {
        return Ok(rules);
    };
    let home = crate::sbpl::resolve(&request.home)?;
    for root in toolchains
        .read_roots
        .iter()
        .chain(&toolchains.uv_cache_roots)
    {
        rules.push(rule(
            "deny",
            "file-write*",
            "subpath",
            &crate::sbpl::resolve(root)?,
        )?);
    }
    for path in &toolchains.proxy_paths {
        if path.is_symlink() {
            rules.push(rule("allow", "file-read*", "literal", path)?);
        }
        rules.push(rule("deny", "file-write*", "literal", path)?);
        for parent in path.ancestors().skip(1) {
            if parent == home || !parent.starts_with(&home) {
                break;
            }
            rules.push(rule("allow", "file-read-metadata", "literal", parent)?);
        }
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_renderers_match_python() -> Result<(), Box<dyn std::error::Error>> {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../tests/fixtures/e2-baseline/typed-renderers.json"
        ))?;
        for case in cases {
            let mut body = case["request"].clone();
            body["home"] = "/typed home+[test]".into();
            body["workspace"] = "/workspace".into();
            body["tmpdir"] = "/tmp/private".into();
            let request: ExecutorRequest = serde_json::from_value(body)?;
            let actual = if case["kind"] == "caller" {
                caller_rules(&request).map_err(|e| e.message().to_owned())?
            } else {
                toolchain_rules(&request).map_err(|e| e.message().to_owned())?
            };
            assert_eq!(serde_json::to_value(actual)?, case["expected"]);
        }
        Ok(())
    }

    #[test]
    fn typed_extras_are_refused() -> Result<(), Box<dyn std::error::Error>> {
        for field in [
            serde_json::json!({"deny_write_roots":["/config"]}),
            serde_json::json!({"lane":{}}),
            serde_json::json!({"run_protection":{}}),
            serde_json::json!({"toolchains":{"read_roots":["/rust"]}}),
            serde_json::json!({"toolchains":{"cargo_configured":true}}),
        ] {
            let mut body = field;
            for (key, value) in serde_json::json!({
                "home":"/owner","workspace":"/ws","tmpdir":"/tmp/private",
                "version":1,"variant":"native","extra_rules":["(allow default)"]
            })
            .as_object()
            .ok_or("invalid test object")?
            {
                body[key] = value.clone();
            }
            assert!(serde_json::from_value::<ExecutorRequest>(body).is_err());
        }
        Ok(())
    }
}
