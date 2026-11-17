//! Deny access to the host ssh-agent and related signing sockets from confined agent profiles.
//! Mirrors `cs/ops/bin/_cli_native.py` `ssh_agent_deny_rules`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::failure::Failure;
use crate::sbpl;

const LAUNCHD_PATTERNS: &[&str] = &[
    r"^/private/tmp/com\.apple\.launchd\.[^/]+/",
    r"^/tmp/com\.apple\.launchd\.[^/]+/",
    r"^/private/var/run/com\.apple\.launchd\.[^/]+/",
    r"^/var/run/com\.apple\.launchd\.[^/]+/",
];

const SOCKET_OPS: &[&str] = &["file-read*", "file-write*", "network-outbound"];

const HOME_AGENT_REL: &[&str] = &[
    "Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock",
    "Library/Containers/com.maxgoedjen.Secretive.SecretAgent/Data/socket.ssh",
];

/// Seatbelt rules that block ssh-agent sockets on the host.
pub fn seatbelt_rules(owner_home: &Path) -> Result<Vec<String>, Failure> {
    let socket = std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    seatbelt_rules_with_socket(owner_home, socket.as_deref())
}

/// Render using the caller's explicit socket, without consulting the environment.
pub fn seatbelt_rules_with_socket(
    owner_home: &Path,
    socket: Option<&Path>,
) -> Result<Vec<String>, Failure> {
    render(owner_home, socket, false)
}

pub(crate) fn native_rules(
    owner_home: &Path,
    socket: Option<&Path>,
) -> Result<Vec<String>, Failure> {
    render(owner_home, socket, true)
}

fn render(owner_home: &Path, socket: Option<&Path>, v1: bool) -> Result<Vec<String>, Failure> {
    let mut rules = Vec::new();
    for pattern in LAUNCHD_PATTERNS {
        for op in SOCKET_OPS {
            rules.push(format!(r#"(deny {op} (regex #"{pattern}"))"#));
        }
    }

    let resolve = |path: &Path| {
        if v1 {
            sbpl::resolve(path)
        } else {
            Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
        }
    };
    let home = resolve(owner_home)?;
    // ~/.ssh may be a symlink (e.g. into ~/dotfiles): deny its resolved target too (8th pass #58).
    let ssh = home.join(".ssh");
    let mut ssh_dirs = vec![ssh.clone()];
    let resolved = resolve(&ssh)?;
    if resolved != ssh {
        ssh_dirs.push(resolved);
    }
    for dir in ssh_dirs {
        let key = dir.to_string_lossy();
        let escaped = format!(
            r"^{}/",
            if v1 {
                sbpl::regex_escape(&key)
            } else {
                regex_escape(&key)
            }
        )
        .replace('"', r#"\""#);
        for op in ["file-read*", "file-write*", "network-outbound"] {
            rules.push(format!(r#"(deny {op} (regex #"{escaped}"))"#));
        }
    }

    let mut seen = BTreeSet::new();
    for rel in HOME_AGENT_REL {
        let path = home.join(rel);
        let mut targets = vec![path.clone()];
        let resolved = resolve(&path)?;
        if resolved != path {
            targets.push(resolved);
        }
        for target in targets {
            let key = target.to_string_lossy().into_owned();
            if !seen.insert(key.clone()) {
                continue;
            }
            let lit = sbpl::quote(&target)?;
            for op in SOCKET_OPS {
                rules.push(format!("(deny {op} (literal {lit}))"));
            }
        }
    }

    if let Some(sock) = socket {
        let path = sock.to_path_buf();
        let mut targets = vec![path.clone()];
        let resolved = resolve(&path)?;
        if resolved != path {
            targets.push(resolved);
        }
        for target in targets {
            let key = target.to_string_lossy().into_owned();
            if !seen.insert(key.clone()) {
                continue;
            }
            let lit = sbpl::quote(&target)?;
            for op in SOCKET_OPS {
                rules.push(format!("(deny {op} (literal {lit}))"));
            }
        }
    }

    rules.push("(deny process-exec (literal \"/bin/launchctl\"))".to_owned());
    Ok(rules)
}

/// Python 3.11+ `re.escape` for home directory prefixes.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        if matches!(
            ch,
            '.' | '-'
                | '\\'
                | '^'
                | '$'
                | '*'
                | '+'
                | '?'
                | '|'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '\n'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn launchd_and_launchctl_rules_are_stable_without_auth_sock() -> Result<(), Failure> {
        let home = Path::new("/Users/test");
        let rules = seatbelt_rules(home)?;
        assert!(rules.iter().any(|r| r.contains("com\\.apple\\.launchd")));
        assert!(
            rules
                .iter()
                .any(|r| r == "(deny process-exec (literal \"/bin/launchctl\"))")
        );
        assert!(rules.iter().any(|r| r.contains("1password")));
        assert!(rules.iter().any(|r| r.contains("Secretive")));
        Ok(())
    }
}
