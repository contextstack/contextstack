//! Muse, adapter and hosted lead ordering, including repeated final rules.
use super::{
    ExecutorRequest, Failure, HOSTED_READS, Lead, Path, PathBuf, allow_write_subpath,
    deny_write_subpath, env_deny, read_rules, socket_deny_rules, ssh_rules, toolchain_rules,
    unenforceable,
};

fn path_rule(action: &str, operations: &str, kind: &str, path: &Path) -> Result<String, Failure> {
    Ok(format!(
        "({action} {operations} ({kind} {}))",
        crate::sbpl::quote(path)?
    ))
}

fn ancestors(home: &Path, paths: &[PathBuf]) -> Result<Vec<String>, Failure> {
    let mut seen = std::collections::BTreeSet::new();
    let mut rules = Vec::new();
    for path in paths {
        for parent in crate::sbpl::resolve(path)?.ancestors().skip(1) {
            if parent.starts_with(home) && seen.insert(parent.to_path_buf()) {
                rules.push(path_rule("allow", "file-read-metadata", "literal", parent)?);
            }
        }
    }
    Ok(rules)
}

pub(super) fn muse_profile(request: &ExecutorRequest) -> Result<String, Failure> {
    let ws = crate::sbpl::resolve(&request.workspace)?;
    let home = crate::sbpl::resolve(&request.home)?;
    if home.starts_with(&ws) {
        return Err(unenforceable("workspace cannot contain the personal home"));
    }
    let mut lines = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        "(deny file-write*)".to_owned(),
        "(allow file-write* (literal \"/dev/null\"))".to_owned(),
        allow_write_subpath(&ws)?,
    ];
    let mut roots = vec![ws.clone()];
    roots.extend(request.read_roots.iter().cloned());
    lines.extend(read_rules(request, &roots)?);
    lines.extend(ssh_rules(request)?);
    lines.extend(toolchain_rules(request)?);
    lines.extend(env_deny::seatbelt_rules(std::slice::from_ref(&ws))?);
    for relative in HOSTED_READS {
        let path = home.join(relative);
        let resolved = crate::sbpl::resolve(&path)?;
        lines.push(path_rule("deny", "file-read*", "subpath", &path)?);
        if resolved != path {
            lines.push(path_rule("deny", "file-read*", "subpath", &resolved)?);
        }
    }
    let mut parents = std::collections::BTreeSet::new();
    for raw in &request.readonly_paths {
        let path = crate::sbpl::resolve(raw)?;
        if !path.starts_with(&ws) {
            return Err(unenforceable(
                "protected configuration must be inside workspace",
            ));
        }
        lines.push(deny_write_subpath(&path)?);
        parents.extend(
            path.ancestors()
                .skip(1)
                .filter(|p| p.starts_with(&ws))
                .map(Path::to_path_buf),
        );
    }
    for parent in parents {
        lines.push(path_rule("deny", "file-write-unlink", "literal", &parent)?);
    }
    let mut traversal = vec![ws];
    traversal.extend(request.executable.iter().cloned());
    lines.extend(ancestors(&home, &traversal)?);
    lines.extend(socket_deny_rules());
    if let Some(adapter) = &request.adapter {
        lines.extend(adapter_suffix(&home, adapter)?);
    }
    Ok(lines.join("\n"))
}

pub(super) fn adapter_suffix(
    home: &Path,
    adapter: &super::Adapter,
) -> Result<Vec<String>, Failure> {
    let mut lines = Vec::new();
    lines.push(path_rule(
        "deny",
        "file-read*",
        "subpath",
        &home.join("Library/Keychains"),
    )?);
    if adapter.auth_mode.as_deref() == Some("oauth_file") {
        if let Some(auth) = adapter.auth_paths.first() {
            lines.push(path_rule("allow", "file-read*", "literal", auth)?);
        }
        if let Some(staged) = adapter.auth_paths.get(1) {
            lines.push(path_rule("deny", "file-write*", "literal", staged)?);
        }
    }
    if let Some(parent) = &adapter.database_parent {
        lines.push(path_rule(
            "deny",
            "file-read* file-write*",
            "subpath",
            parent,
        )?);
    }
    if let Some(port) = &adapter.localhost_port {
        let port = port.to_string();
        if port != "*" && port.parse::<u16>().is_err() {
            return Err(unenforceable("invalid adapter localhost port"));
        }
        lines.push("(deny network-outbound)".to_owned());
        lines.push(format!(
            "(allow network-outbound (remote ip \"localhost:{port}\"))"
        ));
    }
    for path in &adapter.project_config_paths {
        lines.push(path_rule("deny", "file-read*", "subpath", path)?);
    }
    if adapter.auth_mode.as_deref() == Some("oauth_file")
        && let Some(auth) = adapter.auth_paths.first()
    {
        lines.push(path_rule("allow", "file-read*", "literal", auth)?);
    }
    Ok(lines)
}

pub(super) fn lead_suffix(request: &ExecutorRequest, lead: &Lead) -> Result<Vec<String>, Failure> {
    let mut rules = Vec::new();
    let mut denied = lead.denied_session_roots.clone();
    denied.sort();
    for path in denied {
        rules.push(path_rule(
            "deny",
            "file-read* file-write* file-link",
            "subpath",
            &path,
        )?);
    }
    for path in &lead.own_session_roots {
        rules.push(path_rule(
            "allow",
            "file-read* file-write*",
            "subpath",
            path,
        )?);
    }
    let mut roots = vec![request.workspace.clone()];
    roots.extend(lead.own_session_roots.iter().cloned());
    rules.extend(env_deny::seatbelt_rules(&roots)?);
    rules.extend(socket_deny_rules());
    rules.extend(ssh_rules(request)?);
    for sock in &lead.bridge_sockets {
        rules.push(path_rule("allow", "network-outbound", "literal", sock)?);
        let resolved = crate::sbpl::resolve(sock)?;
        if resolved != *sock {
            rules.push(path_rule(
                "allow",
                "network-outbound",
                "literal",
                &resolved,
            )?);
        }
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::{adapter_suffix, lead_suffix};
    use crate::cli_executor::{Adapter, ExecutorRequest};
    use std::path::Path;

    #[test]
    fn adapter_suffixes_match_python_including_duplicate_auth_and_wildcard_port()
    -> Result<(), Box<dyn std::error::Error>> {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../tests/fixtures/e2-baseline/variant-suffixes.json"
        ))?;
        for case in cases {
            let adapter: Adapter = serde_json::from_value(case["adapter"].clone())?;
            let actual = adapter_suffix(Path::new("/typed home+[test]"), &adapter)
                .map_err(|e| e.message().to_owned())?;
            assert_eq!(serde_json::to_value(actual)?, case["expected"]);
        }
        Ok(())
    }

    #[test]
    fn lead_bridge_exception_is_last_and_literal() -> Result<(), Box<dyn std::error::Error>> {
        let request: ExecutorRequest = serde_json::from_value(serde_json::json!({
            "version":1, "variant":"native", "home":"/e2-variants/owner",
            "workspace":"/e2-variants/workspace", "tmpdir":"/e2-variants/tmp",
            "lead": {"denied_session_roots":["/e2-variants/lead-sessions"],
                "own_session_roots":["/e2-variants/lead-sessions/own"],
                "bridge_sockets":["/e2-variants/lead-mcp-own"]}
        }))?;
        let lead = request.lead.as_ref().ok_or("missing lead")?;
        let rules = lead_suffix(&request, lead).map_err(|e| e.message().to_owned())?;
        assert_eq!(
            rules[0],
            "(deny file-read* file-write* file-link (subpath \"/e2-variants/lead-sessions\"))"
        );
        assert_eq!(
            rules[1],
            "(allow file-read* file-write* (subpath \"/e2-variants/lead-sessions/own\"))"
        );
        assert_eq!(
            rules.last().map(String::as_str),
            Some("(allow network-outbound (literal \"/e2-variants/lead-mcp-own\"))")
        );
        assert_eq!(
            rules
                .iter()
                .filter(|rule| rule.starts_with("(allow network-outbound"))
                .count(),
            1
        );
        assert!(
            rules
                .iter()
                .any(|rule| rule.contains("(deny network-outbound (regex"))
        );
        Ok(())
    }
}
