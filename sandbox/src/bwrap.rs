//! The Linux backend's mount plan and bwrap argv, built from the policy in force. Pure apart from
//! `stat` calls, so it is tested on any OS; `linux.rs` runs it.
//!
//! The sandbox root starts empty (bwrap's default tmpfs root). Every readable path outside the
//! writable ones is `--ro-bind`, the workspace, the call's tmp dir and granted `write_env` dirs are
//! `--bind`, symlinks on the way to a runtime path are recreated with `--symlink`, and bwrap
//! supplies `/dev` and `/proc`; an empty `/tmp` exists and the root is remounted read-only last, so a
//! write anywhere outside the binds is `EROFS`. Mounts go parents first, so a writable workspace inside the
//! read-only code root stays writable, as the Seatbelt union of allow rules does on macOS.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::env_deny;
use crate::failure::Failure;
use crate::policy::{Effective, Narrowing, Net};

/// The backend name written to reports and the probe.
pub const BACKEND: &str = "bwrap";

/// One entry of the mount plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mount {
    /// Read-only bind of a path onto itself.
    Ro(PathBuf),
    /// Read-write bind of a path onto itself.
    Rw(PathBuf),
    /// A symlink at `link` pointing at the absolute `target`.
    Link {
        /// The canonical path the link resolves to.
        target: PathBuf,
        /// Where the link is created.
        link: PathBuf,
    },
}

/// The mounts, and the readable paths that do not exist on this host (skipped and reported).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Mount order: parents before children, read-only before read-write at equal depth, links last.
    pub mounts: Vec<Mount>,
    /// Readable paths absent here; each becomes a `read` narrowing.
    pub missing: Vec<PathBuf>,
    /// Whether the tool binary had to be bound because no readable path covered it.
    pub tool_bound: bool,
}

/// bwrap supplies these itself (`--dev`, `--proc`); a policy entry for them is not bound.
fn provided(path: &Path) -> bool {
    path.starts_with("/dev") || path.starts_with("/proc")
}

fn depth(path: &Path) -> usize {
    path.components().count()
}

/// Build the plan. `links` comes from `policy::runtime_links`; `tool` is the resolved tool. The
/// caller adds the tool to `effective.read` via `policy::grant_tool_read` when it sat outside every
/// runtime path; this plan still binds it read-only when that list was built before the grant.
pub fn plan(effective: &Effective, links: &[(PathBuf, PathBuf)], tool: &Path) -> Plan {
    let workspace = &effective.workspace;
    // Writable roots: requested writes inside the workspace are covered by the workspace bind.
    let mut rw: Vec<PathBuf> = effective
        .write
        .iter()
        .filter(|path| *path == workspace || !path.starts_with(workspace))
        .filter(|path| path.exists())
        .cloned()
        .collect();
    rw.sort();
    rw.dedup();
    let under_rw = |path: &Path| rw.iter().any(|root| path.starts_with(root));
    let mut missing = Vec::new();
    let mut ro: Vec<PathBuf> = Vec::new();
    for path in &effective.read {
        if under_rw(path) || provided(path) {
            continue;
        }
        if path.exists() {
            ro.push(path.clone());
        } else {
            missing.push(path.clone());
        }
    }
    let tool = tool.canonicalize().unwrap_or_else(|_| tool.to_path_buf());
    let tool_bound = !under_rw(&tool) && !ro.iter().any(|root| tool.starts_with(root));
    if tool_bound {
        ro.push(tool);
    }
    ro.sort();
    ro.dedup();
    // A read-only path under another read-only path adds nothing.
    let kept: Vec<PathBuf> = ro
        .iter()
        .filter(|path| {
            !ro.iter()
                .any(|other| other != *path && path.starts_with(other))
        })
        .cloned()
        .collect();
    let mut mounts: Vec<Mount> = kept.into_iter().map(Mount::Ro).collect();
    mounts.extend(rw.iter().cloned().map(Mount::Rw));
    mounts.sort_by_key(|mount| match mount {
        Mount::Ro(path) => (depth(path), 0),
        Mount::Rw(path) => (depth(path), 1),
        Mount::Link { .. } => (usize::MAX, 2),
    });
    let bound: Vec<PathBuf> = mounts
        .iter()
        .filter_map(|mount| match mount {
            Mount::Ro(path) | Mount::Rw(path) => Some(path.clone()),
            Mount::Link { .. } => None,
        })
        .collect();
    for (link, target) in links {
        let inside_a_mount = bound.iter().any(|root| link.starts_with(root));
        let target_visible = bound.iter().any(|root| target.starts_with(root));
        if !inside_a_mount && target_visible {
            mounts.push(Mount::Link {
                target: target.clone(),
                link: link.clone(),
            });
        }
    }
    Plan {
        mounts,
        missing,
        tool_bound,
    }
}

/// One `read` narrowing per readable path absent on this host.
pub fn missing_narrowings(missing: &[PathBuf]) -> Vec<Narrowing> {
    missing
        .iter()
        .map(|path| Narrowing {
            capability: "read".to_owned(),
            requested: path.to_string_lossy().into_owned(),
            granted: "none".to_owned(),
            reason: "absent on this host; not bound".to_owned(),
        })
        .collect()
}

/// What the argv needs besides the plan.
#[derive(Debug)]
pub struct Launch<'a> {
    /// The policy's network capability.
    pub net: Net,
    /// For `net: gateway`, (canonical socket, path the tool sees: `CS_GATEWAY_SOCKET`).
    pub gateway: Option<(&'a Path, &'a Path)>,
    /// Granted named sockets, each (canonical socket, path the tool sees: `CS_SOCKET_<NAME>`).
    pub sockets: &'a [(PathBuf, PathBuf)],
    /// The canonical workspace, the tool's cwd.
    pub workspace: &'a Path,
    /// The inherited fd holding the BPF program (`--seccomp`).
    pub seccomp_fd: i32,
    /// The inherited fd bwrap reports `child-pid` and `exit-code` on (`--json-status-fd`).
    pub status_fd: i32,
    /// The tool and its arguments.
    pub tool: &'a Path,
    /// The tool's arguments.
    pub args: &'a [String],
}

fn push(argv: &mut Vec<OsString>, words: &[&str]) {
    argv.extend(words.iter().map(OsString::from));
}

fn push_paths(argv: &mut Vec<OsString>, flag: &str, a: &Path, b: &Path) {
    argv.push(flag.into());
    argv.push(a.into());
    argv.push(b.into());
}

/// bwrap's arguments (not including the bwrap path). Deterministic for a given plan and launch.
pub fn argv(plan: &Plan, launch: &Launch<'_>) -> Result<Vec<OsString>, Failure> {
    let mut argv = Vec::new();
    push(
        &mut argv,
        &[
            "--unshare-user",
            "--disable-userns",
            "--unshare-ipc",
            "--unshare-pid",
            "--unshare-uts",
            "--unshare-cgroup-try",
        ],
    );
    if launch.net != Net::All {
        push(&mut argv, &["--unshare-net"]);
    }
    push(
        &mut argv,
        &[
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            // An empty /tmp, so a write there fails read-only like any path outside the binds.
            "--dir",
            "/tmp",
        ],
    );
    for mount in &plan.mounts {
        match mount {
            Mount::Ro(path) => push_paths(&mut argv, "--ro-bind", path, path),
            Mount::Rw(path) => push_paths(&mut argv, "--bind", path, path),
            Mount::Link { target, link } => push_paths(&mut argv, "--symlink", target, link),
        }
    }
    if let (Net::Gateway, Some((socket, seen))) = (launch.net, launch.gateway) {
        push_paths(&mut argv, "--ro-bind", socket, seen);
    }
    for (socket, seen) in launch.sockets {
        push_paths(&mut argv, "--ro-bind", socket, seen);
    }
    let null = Path::new("/dev/null");
    let mask_roots: Vec<PathBuf> = plan
        .mounts
        .iter()
        .filter_map(|mount| match mount {
            Mount::Ro(path) | Mount::Rw(path) => Some(path.clone()),
            Mount::Link { .. } => None,
        })
        .collect();
    for path in env_deny::masked_paths(&mask_roots)? {
        push_paths(&mut argv, "--ro-bind", null, &path);
    }
    // Last mount step: the root tmpfs itself becomes read-only; the binds keep their own flags.
    push(&mut argv, &["--remount-ro", "/"]);
    argv.push("--chdir".into());
    argv.push(launch.workspace.into());
    argv.push("--seccomp".into());
    argv.push(launch.seccomp_fd.to_string().into());
    argv.push("--json-status-fd".into());
    argv.push(launch.status_fd.to_string().into());
    argv.push("--".into());
    argv.push(launch.tool.into());
    argv.extend(launch.args.iter().map(OsString::from));
    Ok(argv)
}

/// Whether bwrap's `--json-status-fd` output says the sandbox was built and the tool started.
/// bwrap writes `{ "child-pid": N }` only after the namespaces and mounts succeed; without it the
/// exit status is bwrap's own setup failure, not the tool's, and the call is exit 101.
pub fn started(status: &str) -> bool {
    status.contains("\"child-pid\"")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct Layout {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    fn layout() -> Result<Layout, String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = dir
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        for sub in ["usr/lib", "code/runs/ws", "tmp/call", "state", "target"] {
            std::fs::create_dir_all(root.join(sub)).map_err(|error| error.to_string())?;
        }
        std::fs::write(root.join("target/tools"), "").map_err(|error| error.to_string())?;
        Ok(Layout { _dir: dir, root })
    }

    fn effective(root: &Path, net: Net) -> Effective {
        let ws = root.join("code/runs/ws");
        Effective {
            workspace: ws.clone(),
            read: vec![
                root.join("code"),
                root.join("usr"),
                root.join("usr/lib"),
                root.join("absent"),
                PathBuf::from("/dev"),
                ws.clone(),
                ws.join("sub"),
                root.join("tmp/call"),
                root.join("state"),
            ],
            write: vec![
                ws.clone(),
                ws.join("sub/new"),
                root.join("tmp/call"),
                root.join("state"),
            ],
            net,
            tmp: root.join("tmp/call"),
            write_env: BTreeMap::new(),
            profile: None,
            sockets: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn plan_orders_parents_first_and_skips_covered_and_missing() -> Result<(), String> {
        let fx = layout()?;
        let r = &fx.root;
        let links = vec![
            (r.join("lib64"), r.join("usr/lib")),
            (r.join("usr/lib/inside"), r.join("usr/lib")),
            (r.join("dangling"), r.join("nowhere")),
        ];
        let tool = r.join("code/tools/bin/fs_write");
        let got = plan(&effective(r, Net::None), &links, &tool);
        let mut want = vec![
            Mount::Ro(r.join("code")),
            Mount::Ro(r.join("usr")),
            Mount::Rw(r.join("state")),
            Mount::Rw(r.join("tmp/call")),
            Mount::Rw(r.join("code/runs/ws")),
            Mount::Link {
                target: r.join("usr/lib"),
                link: r.join("lib64"),
            },
        ];
        // Equal depth keeps path order; the workspace is deeper than its read-only parent.
        want[..5].sort_by_key(|mount| match mount {
            Mount::Ro(path) => (depth(path), 0),
            Mount::Rw(path) => (depth(path), 1),
            Mount::Link { .. } => (usize::MAX, 2),
        });
        assert_eq!(got.mounts, want);
        assert_eq!(got.missing, vec![r.join("absent")]);
        assert!(!got.tool_bound);
        Ok(())
    }

    #[test]
    fn shipped_policy_mounts_uv_and_managed_python_read_only() -> Result<(), String> {
        let fx = layout()?;
        let r = &fx.root;
        let home = r.join("home");
        let uv = home.join(".local/bin/uv");
        let python = home.join(".local/share/uv/python");
        let cache = home.join(".cache/uv");
        for dir in [uv.parent().ok_or("uv has no parent")?, &python, &cache] {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        std::fs::write(&uv, "").map_err(|error| error.to_string())?;

        let host: crate::policy::HostPolicy =
            serde_json::from_str(include_str!("../../cs/policy/sandbox.json"))
                .map_err(|error| error.to_string())?;
        let (effective, _) = crate::policy::intersect(
            &host,
            &crate::policy::Request::default(),
            &r.join("code/runs/ws"),
            &r.join("tmp/call"),
            &r.join("code"),
            &home,
            &|_| None,
        )
        .map_err(|error| error.message().to_owned())?;
        let got = plan(&effective, &[], &r.join("target/tools"));
        assert!(got.mounts.contains(&Mount::Ro(uv)));
        assert!(got.mounts.contains(&Mount::Ro(python)));
        assert!(got.mounts.contains(&Mount::Rw(r.join("tmp/call"))));
        assert!(!got.mounts.iter().any(|mount| match mount {
            Mount::Ro(path) | Mount::Rw(path) => cache.starts_with(path),
            Mount::Link { .. } => false,
        }));
        Ok(())
    }

    #[test]
    fn a_tool_outside_every_read_path_is_bound_read_only() -> Result<(), String> {
        let fx = layout()?;
        let r = &fx.root;
        let got = plan(&effective(r, Net::None), &[], &r.join("target/tools"));
        assert!(got.tool_bound);
        assert!(got.mounts.contains(&Mount::Ro(r.join("target/tools"))));
        Ok(())
    }

    #[test]
    fn argv_unshares_net_unless_all_and_binds_the_gateway_socket() -> Result<(), String> {
        let fx = layout()?;
        let r = &fx.root;
        let tool = r.join("code/tools/bin/t");
        let args = vec!["a b".to_owned()];
        let ws = r.join("code/runs/ws");
        let socket = r.join("state/gw.sock");
        let seen = PathBuf::from("/run/gw.sock");
        let named = vec![(r.join("state/b.sock"), PathBuf::from("/home/me/b.sock"))];
        let words = |net: Net| -> Vec<String> {
            let launch = Launch {
                net,
                gateway: Some((&socket, &seen)),
                sockets: &named,
                workspace: &ws,
                seccomp_fd: 7,
                status_fd: 8,
                tool: &tool,
                args: &args,
            };
            let plan = plan(&effective(r, net), &[], &tool);
            argv(&plan, &launch)
                .unwrap_or_default()
                .iter()
                .map(|word| word.to_string_lossy().into_owned())
                .collect()
        };
        let none = words(Net::None);
        assert!(none.contains(&"--unshare-net".to_owned()));
        assert!(!none.contains(&socket.display().to_string()));
        let tail = [
            "--chdir".to_owned(),
            ws.display().to_string(),
            "--seccomp".to_owned(),
            "7".to_owned(),
            "--json-status-fd".to_owned(),
            "8".to_owned(),
            "--".to_owned(),
            tool.display().to_string(),
            "a b".to_owned(),
        ];
        assert!(none.ends_with(&tail), "{none:?}");
        assert_eq!(none.first().map(String::as_str), Some("--unshare-user"));
        let remount = none.iter().position(|word| word == "--remount-ro");
        let last_bind = none
            .iter()
            .rposition(|word| word == "--bind" || word == "--ro-bind");
        assert!(remount > last_bind && remount.is_some(), "{none:?}");
        let gateway = words(Net::Gateway);
        let bind = [
            "--ro-bind".to_owned(),
            socket.display().to_string(),
            "/run/gw.sock".to_owned(),
        ];
        assert!(gateway.windows(3).any(|w| w == bind), "{gateway:?}");
        assert!(gateway.contains(&"--unshare-net".to_owned()));
        let named_bind = [
            "--ro-bind".to_owned(),
            r.join("state/b.sock").display().to_string(),
            "/home/me/b.sock".to_owned(),
        ];
        for net in [Net::None, Net::Gateway, Net::Loopback, Net::All] {
            assert!(words(net).windows(3).any(|w| w == named_bind), "{net:?}");
        }
        assert!(
            words(Net::Loopback)
                .iter()
                .any(|word| word == "--unshare-net")
        );
        let all = words(Net::All);
        assert!(!all.contains(&"--unshare-net".to_owned()));
        Ok(())
    }

    #[test]
    fn status_without_child_pid_is_a_setup_failure() {
        assert!(started("{ \"child-pid\": 12 }\n{ \"exit-code\": 1 }\n"));
        assert!(!started(""));
        assert!(!started("{ \"exit-code\": 1 }\n"));
    }

    #[test]
    fn argv_masks_secret_env_files_under_read_roots() -> Result<(), String> {
        let fx = layout()?;
        let r = &fx.root;
        let kit = r.join("code");
        std::fs::create_dir_all(kit.join("tools")).map_err(|e| e.to_string())?;
        let secret = kit.join(".env");
        std::fs::write(&secret, "KEY=1\n").map_err(|e| e.to_string())?;
        std::fs::write(kit.join(".envrc"), "KEY=1\n").map_err(|e| e.to_string())?;
        std::fs::write(kit.join("foo.env"), "KEY=1\n").map_err(|e| e.to_string())?;
        std::fs::write(r.join("code/runs/ws/.env"), "WS=1\n").map_err(|e| e.to_string())?;
        let example = kit.join("tools/.env.example");
        std::fs::write(&example, "ok\n").map_err(|e| e.to_string())?;
        let tool = r.join("code/tools/bin/t");
        if let Some(parent) = tool.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&tool, "").map_err(|e| e.to_string())?;
        let mut eff = effective(r, Net::None);
        eff.read = vec![kit.clone(), PathBuf::from("/usr")];
        let plan = plan(&eff, &[], &tool);
        let launch = Launch {
            net: Net::None,
            gateway: None,
            sockets: &[],
            workspace: &eff.workspace,
            seccomp_fd: 3,
            status_fd: 4,
            tool: &tool,
            args: &[],
        };
        let words: Vec<String> = argv(&plan, &launch)
            .map_err(|f| f.message().to_owned())?
            .iter()
            .map(|w| w.to_string_lossy().into_owned())
            .collect();
        for suffix in ["code/.env", "code/.envrc", "code/foo.env", "ws/.env"] {
            assert!(
                words
                    .windows(3)
                    .any(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2].ends_with(suffix)),
                "expected null mask over {suffix}, got {words:?}"
            );
        }
        assert!(
            !words
                .windows(3)
                .any(|w| w[1] == "/dev/null" && w[2].ends_with("/.env.example")),
            ".env.example must not be masked"
        );
        assert!(
            !words
                .windows(3)
                .any(|w| w[1] == "/dev/null" && w[2].starts_with("/usr/")),
            "system /usr must not be walked for masks: {words:?}"
        );
        Ok(())
    }

    #[test]
    fn missing_paths_become_read_narrowings() {
        let narrowed = missing_narrowings(&[PathBuf::from("/opt")]);
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].capability, "read");
        assert_eq!(narrowed[0].requested, "/opt");
        assert_eq!(narrowed[0].granted, "none");
    }
}
