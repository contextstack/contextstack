//! Project the executor's ordered filesystem rules into bubblewrap mounts.
//!
//! Mounts pin existing paths and hide existing denied contents. A mount plan
//! refuses unpinned literal and subpath denies in writable trees. Regex denies
//! apply at launch only. Writable literal binds cannot be replaced by rename.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use regex::bytes::{Regex, RegexBuilder};

use crate::bwrap::{Mount, Plan};
use crate::cli_executor::{ExecutorRequest, linux_policy_profile};
use crate::failure::{Failure, unenforceable};

#[derive(Clone)]
enum Filter {
    Path(PathBuf, bool),
    Pattern(Regex),
    All(Vec<Self>),
    Not(Box<Self>),
    Unix,
    Ip,
    Symlink,
    Fcntl,
}

impl Filter {
    fn parse<'a>(tokens: &mut impl Iterator<Item = &'a str>) -> Result<Self, Failure> {
        let invalid = || unenforceable("unsupported executor path rule");
        let kind = tokens.next().ok_or_else(invalid)?;
        let filter = match kind {
            "literal" | "subpath" => {
                let quoted = tokens.next().ok_or_else(invalid)?;
                let path: String = serde_json::from_str(quoted).map_err(|_| invalid())?;
                Self::Path(PathBuf::from(path), kind == "subpath")
            }
            "regex" => {
                let pattern = tokens.next().ok_or_else(invalid)?;
                let pattern = pattern
                    .strip_prefix("#\"")
                    .and_then(|p| p.strip_suffix('"'))
                    .ok_or_else(invalid)?;
                Self::Pattern(
                    RegexBuilder::new(&pattern.replace("\\\"", "\""))
                        .unicode(false)
                        .build()
                        .map_err(|e| unenforceable(format!("executor path regex: {e}")))?,
                )
            }
            "local" | "remote" => match tokens.next() {
                Some("unix-socket") => Self::Unix,
                Some("ip") if kind == "remote" => {
                    let address: String = serde_json::from_str(tokens.next().ok_or_else(invalid)?)
                        .map_err(|_| invalid())?;
                    let port = address.strip_prefix("localhost:").ok_or_else(invalid)?;
                    if port != "*" && port.parse::<u16>().is_err() {
                        return Err(invalid());
                    }
                    Self::Ip
                }
                _ => return Err(invalid()),
            },
            "fcntl-command" if tokens.next() == Some("80") && tokens.next() == Some("110") => {
                Self::Fcntl
            }
            "require-all" => Self::All(Self::children(tokens)?),
            "require-not" => {
                if tokens.next() != Some("(") {
                    return Err(invalid());
                }
                Self::Not(Box::new(Self::parse(tokens)?))
            }
            // Only used by a create-symlink rule. Mounting an existing file
            // as a separate bind already prevents replacement with a symlink.
            "vnode-type" if tokens.next() == Some("SYMLINK") => Self::Symlink,
            _ => return Err(invalid()),
        };
        if kind != "require-all" && tokens.next() != Some(")") {
            return Err(invalid());
        }
        Ok(filter)
    }

    fn children<'a>(tokens: &mut impl Iterator<Item = &'a str>) -> Result<Vec<Self>, Failure> {
        let mut children = Vec::new();
        loop {
            match tokens.next() {
                Some("(") => children.push(Self::parse(tokens)?),
                Some(")") => return Ok(children),
                _ => return Err(unenforceable("malformed executor path rule")),
            }
        }
    }

    fn path_only(&self) -> bool {
        match self {
            Self::Path(..) | Self::Pattern(_) => true,
            Self::All(parts) => !parts.is_empty() && parts.iter().all(Self::path_only),
            Self::Not(inner) => inner.path_only(),
            _ => false,
        }
    }

    fn positive_paths<'a>(&'a self, paths: &mut Vec<&'a Path>) {
        match self {
            Self::Path(path, _) => paths.push(path),
            Self::All(parts) => parts.iter().for_each(|part| part.positive_paths(paths)),
            _ => {}
        }
    }

    fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Path(target, true) => path.starts_with(target),
            Self::Path(target, false) => path == target,
            Self::Pattern(pattern) => pattern.is_match(path.as_os_str().as_encoded_bytes()),
            Self::All(filters) => filters.iter().all(|f| f.matches(path)),
            Self::Not(filter) => !filter.matches(path),
            Self::Unix => true,
            Self::Ip | Self::Symlink | Self::Fcntl => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
    Unlink,
    Socket,
    Create,
    Execute,
    CreateSymlink,
}

struct Rule {
    allow: bool,
    access: Access,
    filters: Vec<Filter>,
}

fn rules(profile: &str) -> Result<Vec<Rule>, Failure> {
    let lexer = regex::Regex::new(r#"\(|\)|#?"(?:\\.|[^"\\])*"|[^()\s"]+"#)
        .map_err(|e| unenforceable(e.to_string()))?;
    let mut result = Vec::new();
    for line in profile.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if !line.starts_with('(') {
            return Err(unenforceable("invalid executor rule"));
        }
        let cells = lexer.find_iter(line);
        let mut end = 0;
        let mut words = Vec::new();
        for cell in cells {
            if !line[end..cell.start()].trim().is_empty() {
                return Err(unenforceable("invalid executor rule token"));
            }
            end = cell.end();
            words.push(cell.as_str());
        }
        if !line[end..].trim().is_empty() {
            return Err(unenforceable("invalid executor rule token"));
        }
        let mut tokens = words.into_iter().peekable();
        tokens.next();
        let allow = match tokens.next() {
            Some("allow") => true,
            Some("deny") => false,
            Some("version") if tokens.by_ref().eq(["1", ")"]) => continue,
            _ => return Err(unenforceable("unknown executor rule action")),
        };
        let mut ops = Vec::new();
        while tokens
            .peek()
            .is_some_and(|token| !matches!(*token, "(" | ")"))
        {
            ops.push(tokens.next().unwrap_or_default());
        }
        let filters = Filter::children(&mut tokens)?;
        if ops.is_empty() || tokens.next().is_some() {
            return Err(unenforceable("malformed executor rule"));
        }
        for op in ops {
            let access = match op {
                "file-read*" if filters.iter().all(Filter::path_only) => Some(Access::Read),
                "file-write*" if filters.iter().all(Filter::path_only) => Some(Access::Write),
                "file-write-unlink" if filters.iter().all(Filter::path_only) => {
                    Some(Access::Unlink)
                }
                "network-outbound" if allow || socket_deny(&filters) => Some(Access::Socket),
                "process-exec"
                    if !allow
                        && matches!(filters.as_slice(), [Filter::Path(path, false)] if path == Path::new("/bin/launchctl")) =>
                {
                    Some(Access::Execute)
                }
                // The executor seccomp policy blocks hardlinks and symlink creation.
                "file-link" if !allow => None,
                "file-write-create" if !allow => match filters.as_slice() {
                    [Filter::All(parts)]
                        if matches!(
                            parts.as_slice(),
                            [Filter::Symlink, Filter::Path(..) | Filter::Pattern(_)]
                        ) =>
                    {
                        Some(Access::CreateSymlink)
                    }
                    [Filter::Path(_, false)] => Some(Access::Create),
                    _ => return Err(unsupported_deny(op)),
                },
                // Darwin-only commands. Linux has no corresponding mutating fcntl.
                "system-fcntl" if !allow && matches!(filters.as_slice(), [Filter::Fcntl]) => None,
                // Pathname socket creation is confined by Landlock, or seccomp blocks AF_UNIX.
                "network-bind"
                    if !allow
                        && matches!(filters.as_slice(), [Filter::Unix | Filter::Pattern(_)]) =>
                {
                    None
                }
                "default" | "file-read-metadata" | "file-write-data" | "network-bind" if allow => {
                    None
                }
                _ if allow => {
                    return Err(unenforceable(format!(
                        "unknown executor allow operation: {op}"
                    )));
                }
                _ => return Err(unsupported_deny(op)),
            };
            if let Some(access) = access {
                result.push(Rule {
                    allow,
                    access,
                    filters: filters.clone(),
                });
            }
        }
    }
    Ok(result)
}

fn unsupported_deny(op: &str) -> Failure {
    unenforceable(format!("unsupported executor deny operation: {op}"))
}

fn socket_deny(filters: &[Filter]) -> bool {
    match filters {
        [] | [Filter::Pattern(_) | Filter::Path(..)] => true,
        [Filter::All(parts)] => matches!(parts.first(), Some(Filter::Unix))
            && parts[1..].iter().all(|part| matches!(part, Filter::Not(inner) if matches!(inner.as_ref(), Filter::Path(_, false)))),
        _ => false,
    }
}

fn allows(rules: &[Rule], path: &Path, access: Access) -> bool {
    rules
        .iter()
        .rev()
        .find(|rule| {
            (rule.access == access || (access == Access::Unlink && rule.access == Access::Write))
                && (rule.filters.is_empty() || rule.filters.iter().any(|f| f.matches(path)))
        })
        .map_or(
            matches!(access, Access::Read | Access::Execute | Access::Create),
            |r| r.allow,
        )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visibility {
    ReadOnly,
    ReadWrite,
    Masked,
}

#[derive(Default)]
struct Projection {
    mounts: BTreeMap<PathBuf, Mount>,
    scanned: BTreeSet<PathBuf>,
    entries: usize,
}

fn reject_unpinned_denies(projection: &Projection, rules: &[Rule]) -> Result<(), Failure> {
    for rule in rules.iter().filter(|rule| {
        !rule.allow && !matches!(rule.access, Access::Socket | Access::CreateSymlink)
    }) {
        let mut targets = Vec::new();
        for filter in &rule.filters {
            filter.positive_paths(&mut targets);
        }
        for path in targets {
            let resolved = crate::sbpl::resolve(path)?;
            for target in [path, &resolved] {
                let nearest = target.ancestors().find_map(|p| projection.mounts.get(p));
                let unpinned_writable = matches!(nearest, Some(Mount::Rw(root)) if root != target)
                    || matches!(nearest, Some(Mount::Scratch(_)))
                    || target.starts_with("/dev/shm");
                if !allows(rules, target, rule.access) && unpinned_writable {
                    return Err(unenforceable(format!(
                        "cannot pin absent or unscanned executor deny {}",
                        target.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

fn workspace_mount(path: &Path, rw: bool) -> Mount {
    if rw {
        Mount::Rw(path.to_path_buf())
    } else {
        Mount::Ro(path.to_path_buf())
    }
}

impl Projection {
    fn cover(
        &mut self,
        path: &Path,
        inherited: Option<Visibility>,
        rules: &[Rule],
    ) -> Result<(), Failure> {
        self.scan(
            path,
            inherited,
            rules,
            rustix::process::geteuid().as_raw(),
            &|path| fs::read_dir(path),
        )
    }

    fn scan(
        &mut self,
        path: &Path,
        inherited: Option<Visibility>,
        rules: &[Rule],
        uid: u32,
        read_dir: &impl Fn(&Path) -> std::io::Result<fs::ReadDir>,
    ) -> Result<(), Failure> {
        if !self.scanned.insert(path.to_path_buf()) {
            return Ok(());
        }
        self.entries += 1;
        if self.entries > 1_000_000 || path.components().count() > 96 {
            return Err(unenforceable("executor mount scan exceeded its limit"));
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && inherited.is_none() => {
                return Ok(());
            }
            Err(e) => {
                return Err(unenforceable(format!(
                    "executor cannot inspect {}: {e}",
                    path.display()
                )));
            }
        };
        if metadata.is_symlink() {
            if !allows(rules, path, Access::Read)
                || !allows(rules, path, Access::Execute)
                || (inherited == Some(Visibility::ReadWrite)
                    && (!allows(rules, path, Access::Write)
                        || !allows(rules, path, Access::Unlink)
                        || !allows(rules, path, Access::Create)))
            {
                return Err(unenforceable(format!(
                    "cannot confine denied symlink {}",
                    path.display()
                )));
            }
            return Ok(());
        }
        let visible = if !allows(rules, path, Access::Read) || !allows(rules, path, Access::Execute)
        {
            Visibility::Masked
        } else if allows(rules, path, Access::Write) {
            Visibility::ReadWrite
        } else {
            Visibility::ReadOnly
        };
        if visible == Visibility::Masked {
            if inherited.is_some() {
                let mount = if metadata.is_dir() {
                    Mount::MaskDir(path.to_path_buf())
                } else {
                    Mount::MaskFile(path.to_path_buf())
                };
                self.mounts.insert(path.to_path_buf(), mount);
            }
            return Ok(());
        }
        let pinned = visible == Visibility::ReadWrite
            && (!allows(rules, path, Access::Unlink) || !allows(rules, path, Access::Create));
        if inherited != Some(visible) || pinned {
            self.mounts.insert(
                path.to_path_buf(),
                workspace_mount(path, visible == Visibility::ReadWrite),
            );
        }
        if metadata.is_dir() {
            match read_dir(path) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry.map_err(|e| unenforceable(e.to_string()))?;
                        self.scan(&entry.path(), Some(visible), rules, uid, read_dir)?;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied
                        && metadata.uid() != uid =>
                {
                    self.mounts
                        .insert(path.to_path_buf(), Mount::MaskDir(path.to_path_buf()));
                }
                Err(e) => {
                    return Err(unenforceable(format!(
                        "executor cannot scan {}: {e}",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn plan(request: &ExecutorRequest) -> Result<(Plan, bool), Failure> {
    plan_with_roots(
        request,
        &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"],
    )
}

fn plan_with_roots(request: &ExecutorRequest, runtime: &[&str]) -> Result<(Plan, bool), Failure> {
    if request.version != Some(1) {
        return Err(unenforceable(
            "Linux executor confinement requires request version 1",
        ));
    }
    let mut rules = rules(&linux_policy_profile(request)?)?;
    // Common Linux trust stores have host-private subdirectories. Mask them before
    // walking runtime roots, including when the launcher itself can list them.
    rules.push(Rule {
        allow: false,
        access: Access::Read,
        filters: [
            "/etc/ssl/private",
            "/etc/credstore",
            "/etc/credstore.encrypted",
            "/etc/polkit-1/rules.d",
            "/usr/share/polkit-1/rules.d",
        ]
        .into_iter()
        .map(|path| Filter::Path(path.into(), true))
        .collect(),
    });
    let mut candidates: BTreeSet<PathBuf> = runtime.iter().map(PathBuf::from).collect();
    candidates.extend(request.executable.iter().cloned());
    candidates.extend(named_sockets(request)?);
    for rule in rules.iter().filter(|r| r.allow) {
        for filter in &rule.filters {
            if let Filter::Path(path, _) = filter {
                candidates.insert(path.clone());
            }
        }
    }
    let safe_root = |path: &Path| {
        path.is_absolute()
            && path != Path::new("/")
            && !["/dev", "/proc", "/sys"]
                .iter()
                .any(|root| path.starts_with(root))
    };
    let mut resolved_roots = BTreeSet::new();
    let mut links = BTreeMap::new();
    for path in candidates {
        if !safe_root(&path) {
            continue;
        }
        let resolved = crate::sbpl::resolve(&path)?;
        if !safe_root(&resolved) {
            continue;
        }
        for prefix in path.ancestors().filter(|prefix| prefix.is_symlink()) {
            if allows(&rules, prefix, Access::Read) {
                links.insert(prefix.to_path_buf(), crate::sbpl::resolve(prefix)?);
            }
        }
        resolved_roots.insert(resolved);
    }
    let mut projection = Projection::default();
    for path in resolved_roots {
        projection.cover(&path, None, &rules)?;
    }
    for (link, target) in &links {
        let covered = link
            .ancestors()
            .skip(1)
            .find_map(|parent| projection.mounts.get(parent));
        // A parent bind or link already exposes the host entry at this path.
        if !matches!(
            covered,
            Some(Mount::Ro(_) | Mount::Rw(_) | Mount::Link { .. })
        ) {
            projection.mounts.insert(
                link.clone(),
                Mount::Link {
                    link: link.clone(),
                    target: target.clone(),
                },
            );
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(path) = crate::cli_executor::cli_scratch(request, Path::new("/tmp"))
        && !projection.mounts.contains_key(&path)
    {
        projection.mounts.insert(path.clone(), Mount::Scratch(path));
    }
    reject_unpinned_denies(&projection, &rules)?;
    let mut mounts: Vec<_> = projection.mounts.into_values().collect();
    // Keep the parent-first bind order and install aliases after their targets.
    mounts.sort_by_key(|mount| matches!(mount, Mount::Link { .. }));
    let plan = Plan {
        mounts,
        ..Plan::default()
    };
    Ok((
        plan,
        rules
            .iter()
            .any(|rule| rule.access == Access::CreateSymlink),
    ))
}

pub(crate) fn named_sockets(request: &ExecutorRequest) -> Result<Vec<PathBuf>, Failure> {
    use std::os::unix::fs::FileTypeExt;
    let rules = rules(&linux_policy_profile(request)?)?;
    let mut sockets = BTreeSet::new();
    let lead = request.lead.iter().flat_map(|lead| &lead.bridge_sockets);
    for path in request.sockets_allow.iter().chain(lead) {
        let target = crate::sbpl::resolve(path)?;
        if allows(&rules, path, Access::Socket)
            && allows(&rules, &target, Access::Socket)
            && fs::metadata(&target).is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            sockets.insert(target);
        }
    }
    Ok(sockets.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bwrap::{Launch, executor_argv};
    use crate::cli_executor::seatbelt_profile;
    use crate::policy::Net;
    use crate::seccomp::{self, Domain};

    #[test]
    fn named_socket_policy_keeps_only_the_declared_bridge() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let home = root.join("owner");
        let ws = root.join("workspace");
        let run = root.join("run");
        for path in [&home, &ws, &run] {
            fs::create_dir(path)?;
        }
        let own = run.join("lead-mcp-own");
        let other = run.join("lead-mcp-other");
        let request: ExecutorRequest = serde_json::from_value(serde_json::json!({
            "version": 1, "variant": "native", "workspace": ws, "home": home, "tmpdir": run,
            "lead": {"bridge_sockets": [own]}, "ssh_auth_sock": other,
        }))?;
        let profile = seatbelt_profile(&request).map_err(|e| e.message().to_owned())?;
        let policy = rules(&profile).map_err(|e| e.message().to_owned())?;
        assert!(allows(&policy, &own, Access::Socket));
        assert!(!allows(&policy, &other, Access::Socket));
        assert!(!allows(&policy, &run.join("unknown.sock"), Access::Socket));
        for (landlock, expected) in [(false, Domain::NotIp), (true, Domain::NotIpOrUnix)] {
            let rules = seccomp::executor_rules(landlock, false);
            let socket = rules.iter().find(|rule| rule.syscall == "socket");
            assert_eq!(socket.map(|rule| rule.domain), Some(expected));
        }
        Ok(())
    }

    fn assert_argv(plan: &Plan, ws: &Path) -> Result<(), String> {
        let argv = executor_argv(
            plan,
            &Launch {
                net: Net::Loopback,
                gateway: None,
                sockets: &[],
                workspace: ws,
                seccomp_fd: 7,
                status_fd: 8,
                tool: Path::new("/bin/sh"),
                args: &[],
            },
            || Ok(9),
        )
        .map_err(|e| e.message().to_owned())?;
        assert!(argv.iter().any(|arg| arg == "--unshare-net"));
        assert!(
            argv.windows(2)
                .any(|args| args[0] == "--seccomp" && args[1] == "7")
        );
        Ok(())
    }

    #[test]
    fn browser_action_credentials_stay_out_of_executor_mounts()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().canonicalize()?;
        let workspace = home.join("workspace");
        let run = home.join("run");
        fs::create_dir(&workspace)?;
        fs::create_dir(&run)?;
        fs::write(workspace.join("readable"), "fixture")?;
        let secrets = [
            home.join(".contextstack/browser/action.key"),
            home.join(".contextstack/config/node.env"),
        ];
        for secret in &secrets {
            fs::create_dir_all(secret.parent().ok_or("missing parent")?)?;
            fs::write(secret, "fixture")?;
        }
        // A read root of HOME itself is refused outright; an enclosing root below HOME must
        // still leave the two credential paths inaccessible.
        let enclosing = home.join(".contextstack");
        for lane in [serde_json::Value::Null, serde_json::json!({})] {
            let request: ExecutorRequest = serde_json::from_value(serde_json::json!({
                "version": 1, "variant": "native", "workspace": workspace, "home": home,
                "tmpdir": run, "read_roots": [enclosing], "lane": lane,
            }))?;
            let (plan, _) =
                plan_with_roots(&request, &[]).map_err(|error| error.message().to_owned())?;
            for secret in &secrets {
                let covering = plan
                    .mounts
                    .iter()
                    .filter_map(|mount| match mount {
                        Mount::Ro(path)
                        | Mount::Rw(path)
                        | Mount::MaskDir(path)
                        | Mount::MaskFile(path)
                            if secret.starts_with(path) =>
                        {
                            Some((path, mount))
                        }
                        _ => None,
                    })
                    .max_by_key(|(path, _)| path.components().count());
                assert!(covering.is_none_or(|(_, mount)| matches!(
                    mount,
                    Mount::MaskDir(_) | Mount::MaskFile(_)
                )));
            }
            #[cfg(target_os = "macos")]
            {
                let profile =
                    seatbelt_profile(&request).map_err(|error| error.message().to_owned())?;
                let result = std::process::Command::new("/usr/bin/sandbox-exec")
                    .args([
                        "-p",
                        &profile,
                        "/bin/sh",
                        "-c",
                        "/bin/cat readable >/dev/null || exit 2; for path do if /bin/cat \"$path\" >/dev/null 2>&1; then exit 1; fi; done",
                        "credential-probe",
                    ])
                    .args(&secrets)
                    .current_dir(&workspace)
                    .output()?;
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn malformed_filters_and_unmodelled_denies_are_refused() {
        for rule in [
            r#"(deny file-read* (literal "/secret")"#,
            r#"(deny file-read* (literal "/secret")) trailing"#,
            r#"(deny file-read* (unknown "/secret"))"#,
            "(deny file-read* (remote unix-socket))",
            r#"(deny file-read* (literal "/secret")) ""#,
            "(deny file-read-data)",
            "(deny network*)",
            "(deny file-write-create)",
            "(deny file-write* file-read-data)",
        ] {
            assert!(rules(rule).is_err(), "{rule}");
        }
        let exact = rules(r#"(deny file-read* (literal "/secret"))  "#);
        assert!(exact.is_ok());
    }

    #[test]
    fn masks_unlistable_directory_owned_by_another_user() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let private = root.join("private");
        fs::create_dir(&private)?;
        let deny = rules(r#"(deny file-read* (regex #"(^|/)\.env$"))"#)
            .map_err(|e| e.message().to_owned())?;
        let mut projection = Projection::default();
        // Simulate a non-root launcher seeing a root-owned directory without chown.
        let launcher_uid = u32::from(fs::metadata(&private)?.uid() == 0);
        projection
            .scan(root, None, &deny, launcher_uid, &|path| {
                if path == private {
                    Err(std::io::ErrorKind::PermissionDenied.into())
                } else {
                    fs::read_dir(path)
                }
            })
            .map_err(|e| e.message().to_owned())?;
        assert!(matches!(projection.mounts.get(root), Some(Mount::Ro(_))));
        assert!(matches!(
            projection.mounts.get(&private),
            Some(Mount::MaskDir(_))
        ));
        Ok(())
    }

    #[test]
    fn masks_home_secret_alongside_unlistable_foreign_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().canonicalize()?;
        let foreign = home.join(".cache");
        let secret = home.join(".aws");
        fs::create_dir(&foreign)?;
        fs::create_dir(&secret)?;
        fs::write(secret.join("key"), "fixture")?;
        let deny = Rule {
            allow: false,
            access: Access::Read,
            filters: vec![Filter::Path(secret.clone(), true)],
        };
        let launcher_uid = u32::from(fs::metadata(&foreign)?.uid() == 0);
        let mut projection = Projection::default();
        projection
            .scan(&home, None, &[deny], launcher_uid, &|path| {
                if path == foreign {
                    Err(std::io::ErrorKind::PermissionDenied.into())
                } else {
                    fs::read_dir(path)
                }
            })
            .map_err(|e| e.message().to_owned())?;
        assert_eq!(projection.mounts.get(&home), Some(&Mount::Ro(home.clone())));
        assert_eq!(
            projection.mounts.get(&foreign),
            Some(&Mount::MaskDir(foreign.clone()))
        );
        assert_eq!(
            projection.mounts.get(&secret),
            Some(&Mount::MaskDir(secret.clone()))
        );
        Ok(())
    }

    #[test]
    fn merged_usr_links_follow_binds_and_reuse_bound_shell_link()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let usr = root.join("usr");
        for dir in ["bin", "lib", "sbin"] {
            fs::create_dir_all(usr.join(dir))?;
            symlink(format!("usr/{dir}"), root.join(dir))?;
        }
        fs::write(usr.join("bin/dash"), "fixture")?;
        symlink("dash", usr.join("bin/sh"))?;
        let ws = root.join("workspace");
        let home = root.join("home");
        let run = root.join("run");
        for dir in [&ws, &home, &run] {
            fs::create_dir(dir)?;
        }
        let shell = root.join("bin/sh");
        let request: ExecutorRequest = serde_json::from_value(serde_json::json!({
            "version": 1, "variant": "native", "workspace": ws, "home": home, "tmpdir": run,
            "executable": shell, "lookup_path": [root.join("bin")],
        }))?;
        let roots = ["usr", "bin", "lib", "sbin"].map(|dir| root.join(dir));
        let runtime = roots
            .each_ref()
            .map(|path| path.to_str().ok_or("invalid runtime path"));
        let [usr_root, bin, lib, sbin] = runtime;
        let runtime = [usr_root?, bin?, lib?, sbin?];
        let (plan, _) = plan_with_roots(&request, &runtime).map_err(|e| e.message().to_owned())?;
        let argv = executor_argv(
            &plan,
            &Launch {
                net: Net::Loopback,
                gateway: None,
                sockets: &[],
                workspace: &ws,
                seccomp_fd: 7,
                status_fd: 8,
                tool: &shell,
                args: &[],
            },
            || Ok(9),
        )
        .map_err(|e| e.message().to_owned())?;
        let bind = argv
            .windows(3)
            .position(|args| {
                args[0] == "--ro-bind" && args[1] == usr.as_os_str() && args[2] == usr.as_os_str()
            })
            .ok_or("missing /usr bind")?;
        for dir in ["bin", "lib", "sbin"] {
            let link = argv
                .windows(3)
                .position(|args| {
                    args[0] == "--symlink"
                        && args[1] == usr.join(dir).as_os_str()
                        && args[2] == root.join(dir).as_os_str()
                })
                .ok_or("missing merged-/usr link")?;
            assert!(bind < link);
        }
        assert_eq!(argv.iter().filter(|arg| *arg == "--symlink").count(), 3);
        assert_eq!(
            argv.last().map(std::ffi::OsString::as_os_str),
            Some(shell.as_os_str())
        );
        Ok(())
    }

    #[test]
    fn refuses_unlistable_user_directory() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let path = temp.path().join("unlistable");
        fs::create_dir(&path)?;
        let original = fs::metadata(&path)?.permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o111))?;
        let cannot_list =
            fs::read_dir(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        let projection = Projection::default().cover(&path, None, &[]);
        fs::set_permissions(&path, original)?;
        if cannot_list {
            let failure = projection
                .err()
                .ok_or("accepted an unlistable user-owned directory")?;
            assert!(failure.message().contains("executor cannot scan"));
        }
        Ok(())
    }

    #[test]
    fn lead_adapter_login_and_runtime_goldens() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let home = root.join("home");
        let workspace = root.join("work");
        let run = root.join("run");
        let runtime = root.join("runtime");
        let sessions = workspace.join("sessions");
        let own = sessions.join("self");
        let other = sessions.join("other");
        let config = workspace.join(".opencode");
        let login_dir = home.join(".fixture");
        let login = login_dir.join("auth");
        for path in [&login_dir, &own, &other, &config, &run, &runtime] {
            fs::create_dir_all(path).map_err(|e| format!("mkdir {}: {e}", path.display()))?;
        }
        for path in [
            &login,
            &own.join("data"),
            &other.join("data"),
            &config.join("data"),
            &runtime.join("executable"),
            &runtime.join("private-socket"),
        ] {
            fs::write(path, "fixture").map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        for variant in ["native", "adapter"] {
            let mut request = serde_json::json!({
                "version": 1, "variant": variant, "workspace": workspace, "tmpdir": run,
                "home": home, "ssh_auth_sock": runtime.join("private-socket"),
            });
            if variant == "native" {
                request["lead"] = serde_json::json!({
                    "denied_session_roots": [sessions], "own_session_roots": [own]
                });
                request["home_reads"] = serde_json::json!([login_dir]);
                request["login_targets"] =
                    serde_json::json!([{"path": login, "allow_directory": false}]);
                request["login_writes"] = serde_json::json!([{"literal": "~/.fixture/auth"}]);
            } else {
                request["adapter"] = serde_json::json!({
                    "localhost_port": "*", "project_config_paths": [config],
                });
            }
            let request: ExecutorRequest = serde_json::from_value(request)?;
            let profile = seatbelt_profile(&request).map_err(|e| e.message().to_owned())?;
            let (plan, deny_symlinks) =
                plan_with_roots(&request, &[runtime.to_str().ok_or("invalid runtime path")?])
                    .map_err(|e| e.message().to_owned())?;
            assert_eq!(deny_symlinks, variant == "native");
            // Every rendered deny is accepted by the same strict parser used at launch.
            for deny in profile.lines().filter(|line| line.starts_with("(deny ")) {
                rules(deny).map_err(|e| format!("{deny}: {}", e.message()))?;
            }
            assert!(plan.mounts.contains(&Mount::Ro(runtime.clone())));
            assert!(
                plan.mounts
                    .contains(&Mount::MaskFile(runtime.join("private-socket")))
            );
            if variant == "native" {
                assert!(plan.mounts.contains(&Mount::MaskDir(sessions.clone())));
                assert!(plan.mounts.contains(&Mount::Rw(own.clone())));
                assert!(plan.mounts.contains(&Mount::Ro(login_dir.clone())));
                assert!(plan.mounts.contains(&Mount::Rw(login.clone())));
            } else {
                assert!(plan.mounts.contains(&Mount::MaskDir(config.clone())));
                let mut request = request;
                request
                    .adapter
                    .as_mut()
                    .ok_or("missing adapter fixture")?
                    .project_config_paths = vec![workspace.join("absent.json")];
                let err = plan_with_roots(&request, &[])
                    .err()
                    .ok_or("missing deny was accepted")?;
                assert!(
                    err.message().contains("cannot pin absent"),
                    "{}",
                    err.message()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn executor_and_check_filesystem_goldens() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let home = root.join("owner");
        let ws = home.join("work");
        let run = root.join("run");
        let scratch = root.join("scratch");
        let protected = ws.join("config");
        let shared = root.join("shared");
        for dir in [
            &home,
            &ws,
            &run,
            &scratch,
            &protected,
            &shared,
            &home.join(".aws"),
        ] {
            fs::create_dir(dir)?;
        }
        for file in [
            ws.join("readme"),
            home.join(".aws/key"),
            scratch.join("fixture"),
        ] {
            fs::write(file, "fixture")?;
        }
        for variant in ["native", "muse"] {
            let mut request = serde_json::json!({
                "version": 1, "variant": variant, "workspace": ws, "tmpdir": run,
                "home": home, "check_scratch": scratch,
                "caller_grants": [{"path": shared, "access": "read_write"}],
                "read_roots": [shared, run],
            });
            if variant == "native" {
                request["protected"] = serde_json::json!(protected);
            } else {
                request["readonly_paths"] = serde_json::json!([protected]);
            }
            let request: ExecutorRequest = serde_json::from_value(request)?;
            // Model a Darwin temp tree containing the fixture HOME on every host.
            let profile = crate::cli_executor::executor_profile(&request, Some(root.clone()))
                .map_err(|e| e.message().to_owned())?;
            let parsed = rules(&profile).map_err(|e| e.message().to_owned())?;
            let (plan, deny_symlinks) =
                plan_with_roots(&request, &[]).map_err(|e| e.message().to_owned())?;
            assert!(!deny_symlinks);
            assert_argv(&plan, &ws)?;
            for (path, read, write) in [
                (ws.clone(), true, true),
                (ws.join("readme"), true, true),
                (protected.clone(), true, false),
                (home.join(".aws/key"), false, false),
                (scratch.join("fixture"), true, true),
                (shared.clone(), true, true),
                (run.clone(), true, variant == "native"),
            ] {
                assert_eq!(
                    allows(&parsed, &path, Access::Read),
                    read,
                    "{variant}: {path:?}"
                );
                assert_eq!(
                    allows(&parsed, &path, Access::Write),
                    write,
                    "{variant}: {path:?}"
                );
                let mount = plan.mounts.iter().rev().find(|mount| match mount {
                    Mount::Ro(target)
                    | Mount::Rw(target)
                    | Mount::MaskFile(target)
                    | Mount::MaskDir(target) => path.starts_with(target),
                    _ => false,
                });
                assert_eq!(
                    matches!(mount, Some(Mount::Rw(_) | Mount::Ro(_))),
                    read,
                    "{variant}: {path:?}"
                );
                assert_eq!(
                    matches!(mount, Some(Mount::Rw(_))),
                    write,
                    "{variant}: {path:?}"
                );
            }
            assert!(!allows(&parsed, &ws.join(".env"), Access::Read));
            assert!(!allows(&parsed, &ws.join(".env"), Access::Write));
            assert!(allows(&parsed, &ws.join(".env.example"), Access::Read));
            assert!(!allows(&parsed, &ws.join(".env.example"), Access::Write));
            assert!(allows(&parsed, &scratch.join("fixture.env"), Access::Read));
            assert!(allows(&parsed, &scratch.join("fixture.env"), Access::Write));
        }
        Ok(())
    }
}
