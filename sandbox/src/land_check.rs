//! Seatbelt profile for a host-side land check.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::failure::{Failure, unenforceable};
use crate::seatbelt::FCNTL_DENY;
use crate::{cli_executor, sbpl, ssh_agent_deny, widening};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    version: u32,
    home: PathBuf,
    main_checkout: PathBuf,
    worktree: PathBuf,
    run_tmp: PathBuf,
    cargo_target_dir: PathBuf,
    tmpdir: PathBuf,
    scratch: PathBuf,
    darwin_user_temp: PathBuf,
    #[serde(default)]
    cache_dirs: Vec<PathBuf>,
    #[serde(default)]
    toolchain_roots: Vec<PathBuf>,
    #[serde(default)]
    toolchain_links: Vec<PathBuf>,
    git_dir: Option<PathBuf>,
    #[serde(default)]
    readonly_dirs: Vec<PathBuf>,
    #[serde(default)]
    reserved_ports: Vec<u16>,
    ssh_auth_sock: Option<PathBuf>,
}

fn root(path: &Path) -> Result<PathBuf, Failure> {
    if !path.is_absolute() {
        return Err(unenforceable(format!(
            "expected an absolute path: {}",
            path.display()
        )));
    }
    let resolved = sbpl::resolve(path)?;
    if resolved == Path::new("/") {
        return Err(unenforceable("land-check cannot grant the filesystem root"));
    }
    Ok(resolved)
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn tree_rule(lines: &mut Vec<String>, action: &str, path: &Path) -> Result<(), Failure> {
    lines.push(format!("({action} (subpath {}))", sbpl::quote(path)?));
    Ok(())
}

fn writable_roots(
    request: &Request,
    worktree: PathBuf,
) -> Result<(Vec<PathBuf>, PathBuf), Failure> {
    let run = root(&request.run_tmp)?;
    let darwin_temp = root(&request.darwin_user_temp)?;
    let darwin_root = Path::new("/private/var/folders");
    let darwin_tree = darwin_temp
        .parent()
        .filter(|path| {
            darwin_temp.ends_with("T")
                && path
                    .strip_prefix(darwin_root)
                    .is_ok_and(|p| p.components().count() == 2)
        })
        .ok_or_else(|| unenforceable("expected DARWIN_USER_TEMP_DIR under /private/var/folders"))?;
    let mut writable = vec![worktree, root(&request.cargo_target_dir)?];
    for path in [&request.tmpdir, &request.scratch]
        .into_iter()
        .chain(&request.cache_dirs)
    {
        let path = root(path)?;
        if path == run || !path.starts_with(&run) {
            return Err(unenforceable(format!(
                "land-check scratch, TMPDIR and caches must be inside run_tmp: {}",
                path.display()
            )));
        }
        writable.push(path);
    }
    Ok((writable, darwin_tree.to_path_buf()))
}

pub(crate) fn profile(request: &Request) -> Result<String, Failure> {
    if request.version != 1 || request.reserved_ports.contains(&0) {
        return Err(unenforceable(
            "invalid land-check request version or reserved port",
        ));
    }
    if request
        .ssh_auth_sock
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err(unenforceable("expected an absolute ssh_auth_sock"));
    }
    let home = root(&request.home)?;
    let main = root(&request.main_checkout)?;
    let main_target = root(&main.join("target"))?;
    let worktree = root(&request.worktree)?;
    let (writable, darwin_tree) = writable_roots(request, worktree.clone())?;
    let scratch = root(&request.scratch)?;
    let toolchains = request
        .toolchain_roots
        .iter()
        .map(|path| root(path))
        .collect::<Result<Vec<_>, _>>()?;
    let git_dir = request.git_dir.as_deref().map(root).transpose()?;
    let linked = request
        .toolchain_links
        .iter()
        .map(|path| Ok((path.as_path(), root(path)?)))
        .collect::<Result<Vec<_>, Failure>>()?;
    for path in &writable {
        if overlaps(path, &darwin_tree)
            || home.starts_with(path)
            || overlaps(path, &main)
            || overlaps(path, &main_target)
            || git_dir.as_ref().is_some_and(|git| overlaps(path, git))
            || toolchains.iter().any(|toolchain| overlaps(path, toolchain))
            || linked.iter().any(|(_, link)| overlaps(path, link))
        {
            return Err(unenforceable(format!(
                "land-check write root overlaps a protected path: {}",
                path.display()
            )));
        }
    }

    let mut lines = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        "(deny file-write* file-link)".to_owned(),
        "(deny mach-lookup)".to_owned(),
        r#"(allow mach-lookup (global-name "com.apple.bsd.dirhelper") (global-name "com.apple.system.opendirectoryd.libinfo") (global-name "com.apple.system.opendirectoryd.membership"))"#.to_owned(),
        FCNTL_DENY.to_owned(),
    ];
    home_and_write_rules(&mut lines, &home, &writable)?;
    tree_rule(&mut lines, "deny file-write*", &darwin_tree)?;
    for path in &request.readonly_dirs {
        let path = root(path)?;
        if path == worktree || !path.starts_with(&worktree) {
            return Err(unenforceable(
                "land-check read-only dependency must be inside the worktree",
            ));
        }
        tree_rule(&mut lines, "deny file-write*", &path)?;
    }
    lines.push(r#"(allow file-write-data (literal "/dev/null") (literal "/dev/tty"))"#.to_owned());
    toolchain_read_rules(&mut lines, &toolchains, &linked, git_dir.as_deref())?;
    protected_read_rules(&mut lines, &home, [&main, &worktree])?;
    network_rules(&mut lines, &scratch, &request.reserved_ports)?;
    lines.extend(ssh_agent_deny::native_rules(
        &home,
        request.ssh_auth_sock.as_deref(),
    )?);
    Ok(lines.join("\n") + "\n")
}

/// Deny reads under HOME (keeping metadata for traversal), then grant the write roots read and write.
fn home_and_write_rules(
    lines: &mut Vec<String>,
    home: &Path,
    writable: &[PathBuf],
) -> Result<(), Failure> {
    tree_rule(lines, "deny file-read*", home)?;
    // Permit directory traversal without exposing file contents or directory listings.
    lines.push(format!(
        "(allow file-read-metadata (subpath {}))",
        sbpl::quote(home)?
    ));
    for path in writable {
        tree_rule(lines, "allow file-write*", path)?;
        tree_rule(lines, "allow file-read*", path)?;
    }
    Ok(())
}

/// Read grants for toolchain roots, toolchain entry-point links (as called and resolved) and the git dir.
fn toolchain_read_rules(
    lines: &mut Vec<String>,
    toolchains: &[PathBuf],
    linked: &[(&Path, PathBuf)],
    git_dir: Option<&Path>,
) -> Result<(), Failure> {
    for path in toolchains {
        tree_rule(lines, "allow file-read*", path)?;
    }
    for (called, resolved) in linked {
        lines.push(format!(
            "(allow file-read* (literal {}) (literal {}))",
            sbpl::quote(called)?,
            sbpl::quote(resolved)?
        ));
    }
    if let Some(git_dir) = git_dir {
        tree_rule(lines, "allow file-read*", git_dir)?;
    }
    Ok(())
}

fn protected_read_rules(
    lines: &mut Vec<String>,
    home: &Path,
    checkouts: [&Path; 2],
) -> Result<(), Failure> {
    let mut protected = BTreeSet::new();
    for rel in cli_executor::PROTECTED_READS.iter().copied().chain([
        ".contextstack/config",
        ".claude",
        ".codex",
        ".cursor",
        ".gemini",
        ".pi/agent",
        ".config/goose",
        ".grok",
        ".config/opencode",
        ".local/share/opencode",
    ]) {
        protected.insert(home.join(rel));
    }
    for checkout in checkouts {
        protected.insert(checkout.join("clients/web/auth.json"));
    }
    for path in protected {
        tree_rule(lines, "deny file-read*", &path)?;
        let resolved = root(&path)?;
        if resolved != path {
            tree_rule(lines, "deny file-read*", &resolved)?;
        }
    }
    Ok(())
}

fn network_rules(lines: &mut Vec<String>, scratch: &Path, reserved: &[u16]) -> Result<(), Failure> {
    lines.extend([
        "(deny network*)".to_owned(),
        r#"(allow network-bind network-inbound (local ip "localhost:*"))"#.to_owned(),
        r#"(allow network-outbound (remote ip "localhost:*"))"#.to_owned(),
    ]);
    let scratch = sbpl::quote(scratch)?;
    lines.extend([
        format!("(allow network-bind network-inbound (require-all (local unix-socket) (subpath {scratch})))"),
        format!("(allow network-outbound (require-all (remote unix-socket) (subpath {scratch})))"),
    ]);
    let ports: BTreeSet<_> = widening::DEFAULT_RESERVED_PORTS
        .into_iter()
        .chain(reserved.iter().copied())
        .collect();
    for port in ports {
        lines.push(format!(
            r#"(deny network-outbound (remote ip "localhost:{port}"))"#
        ));
    }
    Ok(())
}
