//! The macOS backend: a deny-default Seatbelt profile generated from the policy in force.
//!
//! Allow rules start from the Codex CLI's `seatbelt_base_policy.sbpl` and
//! `seatbelt_read_only_platform_defaults.sbpl` (vendored under `vendor/codex/`, Apache-2.0),
//! then narrow: reads are only the listed subtrees, there is no platform default read list.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::failure::{Failure, unenforceable};

use crate::env_deny;
use crate::policy::{Effective, Net};
use crate::sbpl;
use crate::ssh_agent_deny;

/// The backend name written to reports and the probe.
pub const BACKEND: &str = "seatbelt";
/// Apple's profile interpreter; fixed path, as Codex uses it.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// The environment variable naming the gateway's unix socket for `net: gateway`.
pub const GATEWAY_SOCKET_VAR: &str = "CS_GATEWAY_SOCKET";

pub fn apply_developer_dir(command: &mut Command, effective: &Effective) {
    if let Some(path) = &effective.developer_dir {
        command.env("DEVELOPER_DIR", path);
    }
}

fn subpath_rules(operation: &str, paths: &[impl AsRef<Path>]) -> Result<Vec<String>, Failure> {
    let mut rules = Vec::with_capacity(paths.len());
    for path in paths {
        rules.push(format!(
            "(allow {operation} (subpath {}))",
            sbpl::quote(path.as_ref())?
        ));
    }
    Ok(rules)
}

/// `network-outbound` to one unix socket path, and to its canonical path when that differs
/// (`/tmp` is `/private/tmp`; Seatbelt matches the resolved path).
fn socket_rules(socket: &Path) -> Result<Vec<String>, Failure> {
    let mut rules = vec![format!(
        "(allow network-outbound (literal {}))",
        sbpl::quote(socket)?
    )];
    if let Ok(canonical) = socket.canonicalize()
        && canonical != socket
    {
        rules.push(format!(
            "(allow network-outbound (literal {}))",
            sbpl::quote(&canonical)?
        ));
    }
    Ok(rules)
}

/// CLI profiles allow IP by default, but only these literal Unix peers.
pub(crate) fn cli_socket_rules(sockets: &[PathBuf]) -> Result<Vec<String>, Failure> {
    cli_socket_rules_native(sockets, false)
}

pub(crate) fn cli_socket_rules_native(
    sockets: &[PathBuf],
    native: bool,
) -> Result<Vec<String>, Failure> {
    let mut paths = vec![PathBuf::from("/private/var/run/mDNSResponder")];
    for path in sockets {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(unenforceable(
                "sockets_allow must contain absolute literal paths".to_owned(),
            ));
        }
        for candidate in [
            path.clone(),
            if native {
                sbpl::resolve(path)?
            } else {
                path.canonicalize().unwrap_or_else(|_| path.clone())
            },
        ] {
            if !paths.contains(&candidate) {
                paths.push(candidate);
            }
        }
    }
    let exceptions = paths
        .iter()
        .map(|p| sbpl::quote(p).map(|lit| format!("(require-not (literal {lit}))")))
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    let mut rules = vec![format!(
        "(deny network-outbound (require-all (remote unix-socket) {exceptions}))"
    )];
    for path in paths {
        rules.push(format!(
            "(allow network-outbound (literal {}))",
            sbpl::quote(&path)?
        ));
    }
    Ok(rules)
}

#[cfg(test)]
mod cli_socket_tests {
    use super::*;

    #[test]
    fn literal_socket_exceptions_do_not_grant_other_unix_peers() -> Result<(), Failure> {
        let rules = cli_socket_rules(&[PathBuf::from("/nonexistent/cs-fixture.sock")])?;
        assert!(rules[0].contains("(remote unix-socket)"));
        assert!(rules[0].contains("(require-not (literal \"/nonexistent/cs-fixture.sock\"))"));
        assert!(rules[0].contains("/private/var/run/mDNSResponder"));
        assert_eq!(rules.len(), 3);
        assert!(cli_socket_rules(&[PathBuf::from("relative.sock")]).is_err());
        assert!(cli_socket_rules(&[PathBuf::from("/tmp/../other.sock")]).is_err());
        Ok(())
    }
}

/// Mach lookups, sysctl, and `AF_SYSTEM` sockets needed for DNS/TLS when IP networking is allowed.
/// From Codex `seatbelt_network_policy.sbpl` (network-enabled appendix).
fn ip_network_support_rules() -> Vec<String> {
    vec![
        "(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))"
            .to_owned(),
        "(allow mach-lookup (global-name \"com.apple.bsd.dirhelper\") (global-name \"com.apple.system.opendirectoryd.membership\") (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.networkd\") (global-name \"com.apple.ocspd\") (global-name \"com.apple.trustd.agent\") (global-name \"com.apple.SystemConfiguration.DNSConfiguration\") (global-name \"com.apple.SystemConfiguration.configd\"))"
            .to_owned(),
        "(allow sysctl-read (sysctl-name-regex #\"^net.routetable\"))".to_owned(),
    ]
}

fn network_rules(net: Net, gateway_socket: Option<&Path>) -> Result<Vec<String>, Failure> {
    match net {
        Net::None => Ok(vec!["(deny network*)".to_owned()]),
        Net::Gateway => {
            let mut rules = vec!["(deny network*)".to_owned()];
            if let Some(socket) = gateway_socket {
                rules.extend(socket_rules(socket)?);
            }
            Ok(rules)
        }
        Net::Loopback => Ok([
            "(deny network*)",
            "(allow network-bind (local ip \"localhost:*\"))",
            "(allow network-inbound (local ip \"localhost:*\"))",
            "(allow network-outbound (remote ip \"localhost:*\"))",
        ]
        .map(str::to_owned)
        .to_vec()),
        Net::All => {
            let mut rules = vec![
                "(deny network*)".to_owned(),
                "(allow network-outbound (remote ip))".to_owned(),
                "(allow network-inbound (local ip))".to_owned(),
                "(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))".to_owned(),
            ];
            rules.extend(ip_network_support_rules());
            Ok(rules)
        }
    }
}

/// Blocks fcntls that mutate files through read-only descriptors (Codex seatbelt.rs).
pub(crate) const FCNTL_DENY: &str = "(deny system-fcntl (fcntl-command 80 110))";

/// HOME read grants mirroring `_cli_native.home_read_rules`; system paths keep their defaults.
pub(crate) fn home_read_rules(
    real_home: &Path,
    read_roots: &[PathBuf],
    home_reads: &[PathBuf],
) -> Result<Vec<String>, Failure> {
    home_read_rules_configured(real_home, read_roots, home_reads, false, false)
}

pub(crate) fn home_read_rules_configured(
    real_home: &Path,
    read_roots: &[PathBuf],
    home_reads: &[PathBuf],
    native: bool,
    cargo_configured: bool,
) -> Result<Vec<String>, Failure> {
    fn resolved(path: &Path) -> PathBuf {
        // Python Path.resolve(strict=False) resolves existing prefixes even when
        // a cache or the final filename has not been created yet.
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let mut result = PathBuf::new();
        for component in absolute.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    result.pop();
                }
                other => {
                    result.push(other.as_os_str());
                    if let Ok(canonical) = result.canonicalize() {
                        result = canonical;
                    }
                }
            }
        }
        result
    }
    let resolved = |path: &Path| {
        if native {
            sbpl::resolve(path)
        } else {
            Ok(resolved(path))
        }
    };
    let home = resolved(real_home)?;
    let mut rules = vec![format!(
        "(deny file-read* (subpath {}))",
        sbpl::quote(&home)?
    )];
    let current = home.join(".contextstack/current");
    let mut grants: Vec<PathBuf> = read_roots
        .iter()
        .map(|path| resolved(path))
        .collect::<Result<_, Failure>>()?;
    grants.push(current.clone());
    grants.push(resolved(&current)?);
    // Shared build caches, documented alongside the Python source of this policy.
    for relative in [
        ".cargo/registry",
        ".cargo/git",
        ".rustup",
        ".npm",
        "Library/Caches/pip",
        "go/pkg/mod",
    ] {
        if !cargo_configured || ![".cargo/registry", ".cargo/git", ".rustup"].contains(&relative) {
            grants.push(resolved(&home.join(relative))?);
        }
    }
    grants.extend(
        home_reads
            .iter()
            .map(|path| resolved(path))
            .collect::<Result<Vec<_>, Failure>>()?,
    );
    let mut seen = std::collections::BTreeSet::new();
    for target in grants {
        if !seen.insert(target.clone()) {
            continue;
        }
        if home.starts_with(&target) {
            return Err(unenforceable(
                "read root cannot contain the personal home".to_owned(),
            ));
        }
        rules.push(format!(
            "(allow file-read* (subpath {}))",
            sbpl::quote(&target)?
        ));
        for parent in target.ancestors().skip(1) {
            if parent.starts_with(&home) {
                rules.push(format!(
                    "(allow file-read-metadata (literal {}))",
                    sbpl::quote(parent)?
                ));
            }
        }
    }
    Ok(rules)
}

/// The profile text. Deterministic for a given policy, so its SHA-256 identifies it.
/// `gateway_socket` is used only when `effective.net` is `gateway`; the caller checks it is set.
pub fn profile(effective: &Effective, gateway_socket: Option<&Path>) -> Result<String, Failure> {
    let quoted = |path: &Path| sbpl::quote(path);
    let mut lines: Vec<String> = vec![
        "(version 1)".to_owned(),
        "; generated by cs-sandbox; allow rules derived from Codex seatbelt_base_policy.sbpl"
            .to_owned(),
        "(deny default)".to_owned(),
        "(allow process-exec)".to_owned(),
        "(allow process-fork)".to_owned(),
        "(allow signal (target same-sandbox))".to_owned(),
        "(allow process-info* (target same-sandbox))".to_owned(),
        "(allow sysctl-read)".to_owned(),
        "(allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))".to_owned(),
        "(allow file-read-metadata)".to_owned(),
        "(allow file-read* (literal \"/\"))".to_owned(),
    ];
    lines.extend(subpath_rules("file-read*", &effective.read)?);
    lines.extend(subpath_rules("file-write*", &effective.write)?);
    lines.push("(allow file-read* file-write-data (literal \"/dev/null\"))".to_owned());
    lines.push("(allow file-read* (literal \"/dev/urandom\") (literal \"/dev/random\") (literal \"/dev/zero\"))".to_owned());
    lines.push("(allow file-read* (literal \"/dev/tty\"))".to_owned());
    if let Some(profile) = effective.profile {
        lines.extend(profile.seatbelt_rules(&effective.tmp, quoted)?);
    }
    lines.extend(network_rules(effective.net, gateway_socket)?);
    for socket in effective.sockets.values() {
        lines.extend(socket_rules(socket)?);
    }
    lines.extend(env_deny::seatbelt_rules(&effective.read)?);
    let owner_home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    lines.extend(ssh_agent_deny::seatbelt_rules(&owner_home)?);
    lines.push(FCNTL_DENY.to_owned());
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn effective(net: Net) -> Effective {
        Effective {
            developer_dir: None,
            workspace: PathBuf::from("/private/tmp/ws"),
            read: vec![
                PathBuf::from("/opt/homebrew"),
                PathBuf::from("/private/tmp/ws"),
                PathBuf::from("/private/var/folders/xy/T/cs-sandbox-7-8"),
                PathBuf::from("/Users/me/.contextstack/state/browser-state"),
                PathBuf::from("/usr"),
            ],
            write: vec![
                PathBuf::from("/private/tmp/ws"),
                PathBuf::from("/private/var/folders/xy/T/cs-sandbox-7-8"),
                PathBuf::from("/Users/me/.contextstack/state/browser-state"),
            ],
            net,
            tmp: PathBuf::from("/private/var/folders/xy/T/cs-sandbox-7-8"),
            write_env: std::collections::BTreeMap::from([(
                "CS_BROWSER_STATE".to_owned(),
                PathBuf::from("/Users/me/.contextstack/state/browser-state"),
            )]),
            profile: None,
            sockets: std::collections::BTreeMap::new(),
        }
    }

    const GOLDEN_NONE: &str = r#"(version 1)
; generated by cs-sandbox; allow rules derived from Codex seatbelt_base_policy.sbpl
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))
(allow file-read-metadata)
(allow file-read* (literal "/"))
(allow file-read* (subpath "/opt/homebrew"))
(allow file-read* (subpath "/private/tmp/ws"))
(allow file-read* (subpath "/private/var/folders/xy/T/cs-sandbox-7-8"))
(allow file-read* (subpath "/Users/me/.contextstack/state/browser-state"))
(allow file-read* (subpath "/usr"))
(allow file-write* (subpath "/private/tmp/ws"))
(allow file-write* (subpath "/private/var/folders/xy/T/cs-sandbox-7-8"))
(allow file-write* (subpath "/Users/me/.contextstack/state/browser-state"))
(allow file-read* file-write-data (literal "/dev/null"))
(allow file-read* (literal "/dev/urandom") (literal "/dev/random") (literal "/dev/zero"))
(allow file-read* (literal "/dev/tty"))
(deny network*)
(deny file-read* file-write* file-link (regex #"/\.env$"))
(deny file-read* file-write* file-link (regex #"/\.env\.[^/]+$"))
(deny file-read* file-write* file-link (regex #"/\.env_[^/]+$"))
(deny file-read* file-write* file-link (regex #"/\.env-[^/]+$"))
(deny file-read* file-write* file-link (regex #"/\.envrc[^/]*$"))
(deny file-read* file-write* file-link (regex #"/[^/]+\.env$"))
(deny file-read* file-write* file-link (regex #"/[^/]+\.env\.[^/]+$"))
(allow file-read* (require-all (subpath "/opt/homebrew") (regex #"/\.env\.example$")))
(allow file-read* (require-all (subpath "/private/tmp/ws") (regex #"/\.env\.example$")))
(allow file-read* (require-all (subpath "/private/var/folders/xy/T/cs-sandbox-7-8") (regex #"/\.env\.example$")))
(allow file-read* (require-all (subpath "/Users/me/.contextstack/state/browser-state") (regex #"/\.env\.example$")))
(allow file-read* (require-all (subpath "/usr") (regex #"/\.env\.example$")))
"#;

    const GOLDEN_ALL: &str = r#"(version 1)
; generated by cs-sandbox; allow rules derived from Codex seatbelt_base_policy.sbpl
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))
(allow file-read-metadata)
(allow file-read* (literal "/"))
(allow file-read* (subpath "/opt/homebrew"))
(allow file-read* (subpath "/private/tmp/ws"))
(allow file-read* (subpath "/private/var/folders/xy/T/cs-sandbox-7-8"))
(allow file-read* (subpath "/Users/me/.contextstack/state/browser-state"))
(allow file-read* (subpath "/usr"))
(allow file-write* (subpath "/private/tmp/ws"))
(allow file-write* (subpath "/private/var/folders/xy/T/cs-sandbox-7-8"))
(allow file-write* (subpath "/Users/me/.contextstack/state/browser-state"))
(allow file-read* file-write-data (literal "/dev/null"))
(allow file-read* (literal "/dev/urandom") (literal "/dev/random") (literal "/dev/zero"))
(allow file-read* (literal "/dev/tty"))
(deny network*)
(allow network-outbound (remote ip))
(allow network-inbound (local ip))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))
(allow mach-lookup (global-name "com.apple.bsd.dirhelper") (global-name "com.apple.system.opendirectoryd.membership") (global-name "com.apple.SecurityServer") (global-name "com.apple.networkd") (global-name "com.apple.ocspd") (global-name "com.apple.trustd.agent") (global-name "com.apple.SystemConfiguration.DNSConfiguration") (global-name "com.apple.SystemConfiguration.configd"))
(allow sysctl-read (sysctl-name-regex #"^net.routetable"))
(deny file-read* file-write* file-link (regex #"/\.env$"))
(deny file-read* file-write* file-link (regex #"/\.env\.[^/]+$"))
(deny file-read* file-write* file-link (regex #"/\.env_[^/]+$"))
(deny file-read* file-write* file-link (regex #"/\.env-[^/]+$"))
(deny file-read* file-write* file-link (regex #"/\.envrc[^/]*$"))
(deny file-read* file-write* file-link (regex #"/[^/]+\.env$"))
(deny file-read* file-write* file-link (regex #"/[^/]+\.env\.[^/]+$"))
(allow file-read* (require-all (subpath "/opt/homebrew") (regex #"/\.env\.example$")))
(allow file-read* (require-all (subpath "/private/tmp/ws") (regex #"/\.env\.example$")))
(allow file-read* (require-all (subpath "/private/var/folders/xy/T/cs-sandbox-7-8") (regex #"/\.env\.example$")))
(allow file-read* (require-all (subpath "/Users/me/.contextstack/state/browser-state") (regex #"/\.env\.example$")))
(allow file-read* (require-all (subpath "/usr") (regex #"/\.env\.example$")))
"#;

    const GOLDEN_TAIL: &str = "(deny system-fcntl (fcntl-command 80 110))\n";

    fn expected(head: &str) -> Result<String, Failure> {
        let home = std::env::var_os("HOME")
            .map_or_else(|| std::path::PathBuf::from("/"), std::path::PathBuf::from);
        let ssh = crate::ssh_agent_deny::seatbelt_rules(&home)?.join("\n");
        Ok(format!("{head}{ssh}\n{GOLDEN_TAIL}"))
    }

    #[test]
    fn home_grants_are_explicit_and_keep_system_defaults() -> Result<(), Failure> {
        let scratch = tempfile::TempDir::new().map_err(|error| unenforceable(error.to_string()))?;
        let home = scratch.path().join("home");
        std::fs::create_dir(&home).map_err(|error| unenforceable(error.to_string()))?;
        let home = home
            .canonicalize()
            .map_err(|error| unenforceable(error.to_string()))?;
        let workspace = home.join("work/repo");
        let declared = home.join("cli/bin");
        let rules = home_read_rules(
            &home,
            std::slice::from_ref(&workspace),
            std::slice::from_ref(&declared),
        )?;
        assert_eq!(
            rules[0],
            format!("(deny file-read* (subpath {}))", sbpl::quote(&home)?)
        );
        for grant in [
            workspace,
            declared,
            home.join(".contextstack/current"),
            home.join(".cargo/registry"),
        ] {
            assert!(rules.contains(&format!(
                "(allow file-read* (subpath {}))",
                sbpl::quote(&grant)?
            )));
        }
        assert!(rules.contains(&format!(
            "(allow file-read-metadata (literal {}))",
            sbpl::quote(&home)?
        )));
        assert!(!rules.iter().any(|line| line.contains(".newsecret")));
        assert!(!rules.iter().any(|line| line.contains("/usr")));
        Ok(())
    }

    #[test]
    fn home_and_ancestors_cannot_be_granted() {
        let home = PathBuf::from("/Users/test");
        for root in [&home, Path::new("/Users"), Path::new("/")] {
            assert!(home_read_rules(&home, &[root.to_path_buf()], &[]).is_err());
        }
    }

    #[test]
    fn golden_profile_for_net_none() -> Result<(), Failure> {
        let text = profile(&effective(Net::None), None)?;
        // Exact match: the only HOME-dependent part is built from the same HOME the profile used.
        assert_eq!(text, expected(GOLDEN_NONE)?);
        assert!(text.contains(r"^/private/tmp/com\.apple\.launchd\.[^/]+/"));
        assert!(text.contains(r#"(deny process-exec (literal "/bin/launchctl"))"#));
        Ok(())
    }

    #[test]
    fn golden_profile_for_net_all_allows_ip_not_blanket_network() -> Result<(), Failure> {
        let text = profile(&effective(Net::All), None)?;
        assert_eq!(text, expected(GOLDEN_ALL)?);
        assert!(!text.contains("(allow network*)"));
        Ok(())
    }

    #[test]
    fn golden_profile_includes_fcntl_deny() -> Result<(), Failure> {
        let text = profile(&effective(Net::None), None)?;
        assert!(text.contains(FCNTL_DENY));
        Ok(())
    }

    #[test]
    fn golden_profile_blocks_secret_env_files() -> Result<(), Failure> {
        let text = profile(&effective(Net::None), None)?;
        assert!(text.contains(r#"(deny file-read* file-write* file-link (regex #"/\.env$"))"#));
        assert!(
            text.contains(r#"(deny file-read* file-write* file-link (regex #"/\.env\.[^/]+$"))"#)
        );
        assert!(
            text.contains(r#"(deny file-read* file-write* file-link (regex #"/\.env_[^/]+$"))"#)
        );
        assert!(
            text.contains(r#"(deny file-read* file-write* file-link (regex #"/\.env-[^/]+$"))"#)
        );
        assert!(
            text.contains(r#"(deny file-read* file-write* file-link (regex #"/\.envrc[^/]*$"))"#)
        );
        assert!(
            text.contains(r#"(deny file-read* file-write* file-link (regex #"/[^/]+\.env$"))"#)
        );
        assert!(
            text.contains(
                r#"(deny file-read* file-write* file-link (regex #"/[^/]+\.env\.[^/]+$"))"#
            )
        );
        assert!(!text.contains(r#"(allow file-read* (regex #"/\.env\.example$"))"#));
        assert!(text.contains(
            r#"(allow file-read* (require-all (subpath "/private/tmp/ws") (regex #"/\.env\.example$")))"#
        ));
        Ok(())
    }

    #[test]
    fn gateway_adds_one_socket_and_all_allows_ip_network() -> Result<(), Failure> {
        let gateway = profile(&effective(Net::Gateway), Some(Path::new("/run/cs/gw.sock")))?;
        assert!(
            gateway.contains(
                "(deny network*)\n(allow network-outbound (literal \"/run/cs/gw.sock\"))"
            )
        );
        assert!(!gateway.contains("/private/var/run/mDNSResponder"));
        assert!(gateway.contains(FCNTL_DENY));
        let all = profile(&effective(Net::All), None)?;
        assert!(all.contains("(allow network-outbound (remote ip))"));
        assert!(all.contains("(allow network-inbound (local ip))"));
        assert!(
            all.contains("(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))")
        );
        assert!(!all.contains("(allow network*)"));
        Ok(())
    }

    #[test]
    fn a_granted_socket_is_the_only_unix_endpoint_under_every_net_mode() -> Result<(), Failure> {
        let mut with_socket = effective(Net::None);
        with_socket.sockets.insert(
            "browser-public".to_owned(),
            PathBuf::from("/Users/me/.contextstack/b.sock"),
        );
        let literal = "(allow network-outbound (literal \"/Users/me/.contextstack/b.sock\"))";
        let text = profile(&with_socket, None)?;
        assert!(text.contains(literal));
        assert!(!text.contains("(allow network-outbound (remote unix-socket))"));
        for net in [Net::Gateway, Net::All] {
            with_socket.net = net;
            let text = profile(
                &with_socket,
                if net == Net::Gateway {
                    Some(Path::new("/run/cs/gw.sock"))
                } else {
                    None
                },
            )?;
            assert!(
                text.contains(literal),
                "granted socket must appear under net {net:?}"
            );
            assert!(
                !text.contains("(allow network-outbound (remote unix-socket))"),
                "net {net:?} must not open all unix sockets"
            );
        }
        Ok(())
    }

    #[test]
    fn paths_with_quotes_and_spaces_are_escaped() -> Result<(), Failure> {
        let mut odd = effective(Net::None);
        odd.read = vec![PathBuf::from("/Users/me/Application Support/a\"b\\c")];
        let text = profile(&odd, None)?;
        assert!(
            text.contains(
                r#"(allow file-read* (subpath "/Users/me/Application Support/a\"b\\c"))"#
            )
        );
        Ok(())
    }
}
