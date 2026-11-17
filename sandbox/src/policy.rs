//! Host policy, descriptor request, and the intersection that becomes the policy in force.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::failure::{Failure, unenforceable};
use crate::profile::Profile;
use crate::write_env;

/// Network capability, least to most permissive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Net {
    /// No network at all.
    #[default]
    None,
    /// One unix socket, the gateway's, and nothing else.
    Gateway,
    /// Unrestricted network.
    All,
}

impl Net {
    /// The policy word for this value.
    pub fn word(self) -> &'static str {
        match self {
            Net::None => "none",
            Net::Gateway => "gateway",
            Net::All => "all",
        }
    }
}

/// The host policy file (`cs/policy/sandbox.json`): what every tool may read, which network
/// values a descriptor may ask for, and which variables may name a write target, under which
/// roots. The tmp directory is the wrapper's, one per call.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostPolicy {
    /// Shape version; only 1 exists.
    pub version: u32,
    /// Absolute paths every tool may read; `$CODE_ROOT` and `$HOME` expand, and `$TOOL_PYTHON` is
    /// the tool interpreter's installation (`tool_python`). One list for every OS, or one list per
    /// OS (`{"macos": [...], "linux": [...]}`).
    pub runtime_read: RuntimeRead,
    /// Network values a descriptor may request; `none` is always granted.
    pub allow_net: Vec<Net>,
    /// Variable name -> absolute roots (`$HOME`, `$CODE_ROOT` expand) its directory must lie under.
    #[serde(default)]
    pub write_env: BTreeMap<String, Vec<String>>,
    /// Named profiles this host allows, each with its host-specific read paths. A profile not
    /// listed here is narrowed away when a descriptor asks for it.
    #[serde(default)]
    pub profiles: Profiles,
    /// Named unix sockets a descriptor may ask to reach: name -> absolute path (`$HOME`,
    /// `$CODE_ROOT` expand). A granted socket is the only network endpoint added; `net` is unchanged.
    #[serde(default)]
    pub sockets: BTreeMap<String, String>,
}

/// `profiles`: one optional entry per profile the wrapper knows; any other key is invalid.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profiles {
    /// The `browser` profile (`profile.rs`).
    pub browser: Option<ProfileGrant>,
    /// The coding-agent executor profile (`cli_executor.rs`).
    #[serde(rename = "cli-executor")]
    pub cli_executor: Option<ProfileGrant>,
}

/// What the host adds when it grants a profile.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfileGrant {
    /// Extra absolute read paths, in the `runtime_read` shapes (`$CODE_ROOT`, `$HOME`, `$TOOL_PYTHON`);
    /// an absent one is skipped on Linux and reported, as for `runtime_read`.
    pub read: RuntimeRead,
}

impl HostPolicy {
    fn grant(&self, profile: Profile) -> Option<&ProfileGrant> {
        match profile {
            Profile::Browser => self.profiles.browser.as_ref(),
            Profile::CliExecutor => self.profiles.cli_executor.as_ref(),
        }
    }

    /// Every read list in force on `os`: `runtime_read`, then the granted profile's.
    fn read_lists(&self, os: &str, profile: Option<Profile>) -> Result<Vec<&String>, Failure> {
        let mut lists: Vec<&String> = self.runtime_read.for_os(os)?.iter().collect();
        if let Some(grant) = profile.and_then(|p| self.grant(p)) {
            lists.extend(grant.read.for_os(os)?);
        }
        Ok(lists)
    }
}

/// `runtime_read`: a list that holds on every OS, or a list per OS (phase 3b: the macOS and Linux
/// runtimes live at different paths).
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RuntimeRead {
    /// The same paths on every OS.
    All(Vec<String>),
    /// Paths per OS; an OS without a list cannot be enforced (exit 101).
    PerOs(PerOs),
}

/// The per-OS form of `runtime_read`; keys are `std::env::consts::OS` values.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PerOs {
    /// Paths on macOS.
    pub macos: Option<Vec<String>>,
    /// Paths on Linux.
    pub linux: Option<Vec<String>>,
}

impl RuntimeRead {
    /// The list for `os`; a per-OS policy without one is unenforceable here.
    pub fn for_os(&self, os: &str) -> Result<&[String], Failure> {
        let list = match self {
            RuntimeRead::All(list) => Some(list),
            RuntimeRead::PerOs(per) => match os {
                "macos" => per.macos.as_ref(),
                "linux" => per.linux.as_ref(),
                _ => None,
            },
        };
        list.map(Vec::as_slice).ok_or_else(|| {
            unenforceable(format!(
                "invalid policy: runtime_read has no list for {os}; the tool was not run"
            ))
        })
    }
}

/// The descriptor's optional `sandbox` object: what the tool asks for.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Request {
    /// Requested network capability.
    pub net: Net,
    /// Extra workspace-relative read paths.
    pub read: Vec<String>,
    /// Extra workspace-relative write paths.
    pub write: Vec<String>,
    /// Variables naming a directory outside the workspace the tool writes to.
    pub write_env: Vec<String>,
    /// A named profile (`browser`); granted only when the host lists it.
    pub profile: Option<Profile>,
    /// Named unix sockets (host `sockets`) the tool connects to.
    pub sockets: Vec<String>,
}

/// One request the host policy did not grant in full; the report lists every one.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Narrowing {
    /// Which capability was narrowed.
    pub capability: String,
    /// What the descriptor asked for.
    pub requested: String,
    /// What the tool got.
    pub granted: String,
    /// Why.
    pub reason: String,
}

/// Whether `path` lies under any granted read or write subtree.
fn path_covered(path: &Path, effective: &Effective) -> bool {
    effective
        .read
        .iter()
        .chain(effective.write.iter())
        .any(|root| path.starts_with(root))
}

/// The tool binary must be readable under a deny-default profile (shell scripts and loaders);
/// native tools may need siblings beside the binary. Adds the canonical tool path and, when it was
/// not already covered, its parent directory.
pub fn grant_tool_read(effective: &mut Effective, tool: &Path) {
    let tool = tool.canonicalize().unwrap_or_else(|_| tool.to_path_buf());
    if path_covered(&tool, effective) {
        return;
    }
    effective.read.push(tool.clone());
    if let Some(parent) = tool.parent()
        && parent != Path::new("/")
        && !path_covered(parent, effective)
    {
        effective.read.push(parent.to_path_buf());
    }
}

/// The policy in force for one call, after the intersection.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Effective {
    /// The canonical workspace path.
    pub workspace: PathBuf,
    /// Every readable subtree.
    pub read: Vec<PathBuf>,
    /// Every writable subtree.
    pub write: Vec<PathBuf>,
    /// The network capability granted.
    pub net: Net,
    /// The call's private tmp directory, outside the workspace, named by `TMPDIR`.
    pub tmp: PathBuf,
    /// Each granted `write_env` variable and its canonical directory (also in `read` and `write`).
    pub write_env: BTreeMap<String, PathBuf>,
    /// The granted profile, if any; its reads are already in `read`.
    pub profile: Option<Profile>,
    /// Each granted socket name and its path as the host lists it (the tool sees this path, named
    /// by `CS_SOCKET_<NAME>`).
    pub sockets: BTreeMap<String, PathBuf>,
}

/// Read and validate the host policy file.
pub fn load_host(path: &Path) -> Result<HostPolicy, Failure> {
    let shown = path.display();
    let text = std::fs::read_to_string(path)
        .map_err(|error| unenforceable(format!("cannot read policy {shown}: {error}")))?;
    let host: HostPolicy = serde_json::from_str(&text)
        .map_err(|error| unenforceable(format!("invalid policy {shown}: {error}")))?;
    if host.version != 1 {
        return Err(unenforceable(format!(
            "invalid policy {shown}: version {} is not 1",
            host.version
        )));
    }
    if let Some(name) = host.sockets.keys().find(|name| !socket_name_ok(name)) {
        return Err(unenforceable(format!(
            "invalid policy {shown}: socket name {name:?} is not [a-z0-9][a-z0-9-]*"
        )));
    }
    Ok(host)
}

/// A socket name: lowercase letters, digits and dashes, starting with a letter or digit, so
/// `CS_SOCKET_<NAME>` is a valid variable name.
fn socket_name_ok(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The variable naming a granted socket's path for the tool: `browser-public` ->
/// `CS_SOCKET_BROWSER_PUBLIC`.
pub fn socket_var(name: &str) -> String {
    format!("CS_SOCKET_{}", name.to_ascii_uppercase().replace('-', "_"))
}

/// `$CODE_ROOT`: `LAB_CODE_ROOT` when set, else the policy file's grandparent directory
/// (the code root when the file is `cs/policy/sandbox.json`).
pub fn code_root(policy_path: &Path) -> PathBuf {
    if let Some(root) = env::var_os("LAB_CODE_ROOT") {
        return PathBuf::from(root);
    }
    let absolute = if policy_path.is_absolute() {
        policy_path.to_path_buf()
    } else {
        env::current_dir().unwrap_or_default().join(policy_path)
    };
    let grandparent = absolute.parent().and_then(Path::parent);
    grandparent.map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// The monorepo directory holding the code root (`cs`'s parent in a checkout).
pub fn repo_root(code_root: &Path) -> PathBuf {
    code_root
        .parent()
        .map_or_else(|| code_root.to_path_buf(), Path::to_path_buf)
}

/// The read-only token for the repo root; read lists only.
#[allow(dead_code)] // named token in policy JSON; only the string appears in read lists.
pub const REPO_ROOT: &str = "$REPO_ROOT";

/// Refuse when `HOME` is unset or empty (same class as a missing gateway socket).
pub fn require_home(home: Option<&std::ffi::OsStr>) -> Result<PathBuf, Failure> {
    match home {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => Err(unenforceable(
            "HOME is unset or empty; the tool was not run".to_owned(),
        )),
    }
}

/// The process `HOME`, when set and non-empty.
pub fn home_from_env() -> Result<PathBuf, Failure> {
    require_home(env::var_os("HOME").as_deref())
}

/// Expand `$CODE_ROOT`, `$REPO_ROOT` and `$HOME` in a policy path.
pub fn expand(raw: &str, code_root: &Path, home: &Path) -> Result<String, Failure> {
    if home.as_os_str().is_empty() {
        return Err(unenforceable(
            "HOME is unset or empty; the tool was not run".to_owned(),
        ));
    }
    let repo = repo_root(code_root);
    Ok(raw
        .replace("$CODE_ROOT", &code_root.to_string_lossy())
        .replace("$REPO_ROOT", &repo.to_string_lossy())
        .replace("$HOME", &home.to_string_lossy()))
}

/// The read-only token for the tool interpreter's own installation. Read lists only.
pub const TOOL_PYTHON: &str = "$TOOL_PYTHON";

/// `$TOOL_PYTHON`: the base installation (`sys.base_prefix`) of the code root's venv, found as Python
/// finds it: `$CODE_ROOT/.venv/pyvenv.cfg`'s `home` (the base interpreter's `bin/`), canonical, less
/// that `bin`. Tools run `#!/usr/bin/env python3` through `.venv/bin`, a link to that interpreter,
/// which loads its stdlib and `lib/libpython*` from there. In a checkout that is uv's managed
/// Python; in an installed release it is `releases/<id>/python/cpython-…`, outside the code root.
/// None (the entry is dropped) when there is no venv or `home`, or when it would be `/`.
pub fn tool_python(code_root: &Path) -> Option<PathBuf> {
    let cfg = std::fs::read_to_string(code_root.join(".venv/pyvenv.cfg")).ok()?;
    let home = cfg.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "home").then(|| value.trim().to_owned())
    })?;
    let home = PathBuf::from(home).canonicalize().ok()?;
    let base = if home.file_name().is_some_and(|name| name == "bin") {
        home.parent()?.to_path_buf()
    } else {
        home
    };
    base.parent().is_some().then_some(base)
}

fn tool_python_unresolved_reason(code_root: &Path) -> String {
    let cfg = code_root.join(".venv/pyvenv.cfg");
    if cfg.is_file() {
        format!("{TOOL_PYTHON} does not resolve from {}", cfg.display())
    } else {
        format!("{TOOL_PYTHON} does not resolve: no {}", cfg.display())
    }
}

fn read_narrowing(raw: &str, reason: String) -> Narrowing {
    Narrowing {
        capability: "read".to_owned(),
        requested: raw.to_owned(),
        granted: "none".to_owned(),
        reason,
    }
}

/// One read-list entry, expanded; `$TOOL_PYTHON` resolved, or None when it does not resolve.
fn expand_read(raw: &str, code_root: &Path, home: &Path) -> Result<Option<PathBuf>, Failure> {
    let raw = if raw.contains(TOOL_PYTHON) {
        match tool_python(code_root) {
            Some(base) => raw.replace(TOOL_PYTHON, &base.to_string_lossy()),
            None => return Ok(None),
        }
    } else {
        raw.to_owned()
    };
    Ok(Some(PathBuf::from(expand(&raw, code_root, home)?)))
}

/// The tool's descriptor. The file pair's sidecar `<tool>.json` when it exists; else, for a
/// `runtime: exe` tool: the one descriptor in `$CODE_ROOT/tools/bin` whose `bin`,
/// resolved as both runners resolve it (relative to the directory holding the code root, unless
/// absolute), is this tool and whose `args` are exactly this call's arguments. The runners exec
/// nothing else for such a descriptor, and the model cannot choose argv, so the match names the
/// descriptor the runner used. Two matches cannot be told apart: exit 101. None: the default request.
pub fn descriptor_for(
    tool: &Path,
    args: &[String],
    code_root: &Path,
) -> Result<Option<PathBuf>, Failure> {
    let mut name = tool.as_os_str().to_owned();
    name.push(".json");
    let sidecar = PathBuf::from(name);
    if sidecar.is_file() {
        return Ok(Some(sidecar));
    }
    let Ok(target) = tool.canonicalize() else {
        return Ok(None);
    };
    let repo = code_root.parent().unwrap_or(code_root);
    let Ok(entries) = std::fs::read_dir(code_root.join("tools/bin")) else {
        return Ok(None);
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter(|path| exe_matches(path, repo, &target, args))
        .collect();
    found.sort();
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(unenforceable(format!(
            "{} runtime exe descriptors name {} with these args: {}",
            found.len(),
            target.display(),
            found
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Whether the descriptor at `path` is `runtime: exe` with this `bin` and exactly these `args`.
/// An unreadable or malformed file is not a match: it cannot be the descriptor a runner exec'd.
fn exe_matches(path: &Path, repo: &Path, target: &Path, args: &[String]) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let Some(bin) = value.get("bin").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let wanted: Option<Vec<&str>> = match value.get("args") {
        None | Some(serde_json::Value::Null) => Some(Vec::new()),
        Some(list) => list
            .as_array()
            .and_then(|items| items.iter().map(serde_json::Value::as_str).collect()),
    };
    value.get("runtime").and_then(serde_json::Value::as_str) == Some("exe")
        && !bin.is_empty()
        && wanted.is_some_and(|wanted| wanted == args)
        && repo.join(bin).canonicalize().is_ok_and(|bin| bin == target)
}

/// Read the descriptor's `sandbox` object; no descriptor, or an absent object, is the default
/// request (net none, nothing extra).
pub fn load_request(descriptor: Option<&Path>) -> Result<Request, Failure> {
    let Some(descriptor) = descriptor else {
        return Ok(Request::default());
    };
    let shown = descriptor.display();
    let text = std::fs::read_to_string(descriptor)
        .map_err(|error| unenforceable(format!("cannot read descriptor {shown}: {error}")))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| unenforceable(format!("invalid descriptor {shown}: {error}")))?;
    match value.get("sandbox") {
        None | Some(serde_json::Value::Null) => Ok(Request::default()),
        Some(sandbox) if !sandbox.is_object() => Err(unenforceable(format!(
            "invalid descriptor {shown}: sandbox must be an object"
        ))),
        Some(sandbox) => serde_json::from_value(sandbox.clone())
            .map_err(|error| unenforceable(format!("invalid descriptor {shown}: sandbox {error}"))),
    }
}

/// Resolve a workspace-relative request inside the canonical workspace. Absolute paths, `..`,
/// and symlinks that leave the workspace are refused; the part that does not exist yet is kept
/// lexically under the deepest existing ancestor.
pub fn resolve_inside(workspace: &Path, relative: &str) -> Result<PathBuf, Failure> {
    let escape = || unenforceable(format!("requested path escapes the workspace: {relative}"));
    let path = Path::new(relative);
    if relative.is_empty() || path.is_absolute() {
        return Err(escape());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(escape()),
        }
    }
    let mut existing = workspace.join(path);
    let mut missing: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Err(escape());
        };
        missing.push(name.to_owned());
        let Some(parent) = existing.parent() else {
            return Err(escape());
        };
        existing = parent.to_path_buf();
    }
    let mut resolved = existing.canonicalize().map_err(|_| escape())?;
    if !resolved.starts_with(workspace) {
        return Err(escape());
    }
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn runtime_paths(
    host: &HostPolicy,
    profile: Option<Profile>,
    code_root: &Path,
    home: &Path,
    narrowed: &mut Vec<Narrowing>,
) -> Result<BTreeSet<PathBuf>, Failure> {
    let mut paths = BTreeSet::new();
    for raw in host.read_lists(env::consts::OS, profile)? {
        if raw.contains(TOOL_PYTHON) && tool_python(code_root).is_none() {
            narrowed.push(read_narrowing(
                raw,
                tool_python_unresolved_reason(code_root),
            ));
            continue;
        }
        let Some(expanded) = expand_read(raw, code_root, home)? else {
            continue;
        };
        if !expanded.is_absolute() {
            return Err(unenforceable(format!(
                "invalid policy: runtime_read entry is not absolute: {raw}"
            )));
        }
        let canonical = expanded.canonicalize().unwrap_or(expanded);
        paths.insert(canonical);
    }
    Ok(paths)
}

/// Every symlink on the way to a `runtime_read` path, as (link, canonical target): `/lib64` on most
/// Linux hosts, `/etc/resolv.conf` on Ubuntu. Seatbelt resolves links in the kernel and needs none
/// of these; bwrap's empty root must recreate them, or `#!/bin/sh` and the ELF interpreter
/// `/lib64/ld-linux-x86-64.so.2` would not exist inside the sandbox.
pub fn runtime_links(
    host: &HostPolicy,
    profile: Option<Profile>,
    code_root: &Path,
    home: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>, Failure> {
    let mut links = BTreeSet::new();
    for raw in host.read_lists(env::consts::OS, profile)? {
        let Some(expanded) = expand_read(raw, code_root, home)? else {
            continue;
        };
        let mut prefix = PathBuf::new();
        for component in expanded.components() {
            prefix.push(component);
            let is_link = std::fs::symlink_metadata(&prefix).is_ok_and(|m| m.is_symlink());
            if is_link && let Ok(target) = prefix.canonicalize() {
                links.insert((prefix.clone(), target));
            }
        }
    }
    Ok(links.into_iter().collect())
}

/// The intersection: reads = runtime reads, the workspace, the tmp directory and the requested
/// reads; writes = the workspace, the tmp directory and the requested writes; net = the request
/// when the host lists it, else `none`, recorded as a narrowing. Denial wins; nothing is widened.
/// `tmp` is the call's canonical private tmp directory, already created by the caller. A requested
/// `write_env` variable adds its directory to reads and writes when `write_env::grant` allows it
/// (`var` reads the environment), else it is narrowed.
pub fn intersect(
    host: &HostPolicy,
    request: &Request,
    workspace: &Path,
    tmp: &Path,
    code_root: &Path,
    home: &Path,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<(Effective, Vec<Narrowing>), Failure> {
    let mut narrowed = Vec::new();
    let profile = match request.profile {
        Some(wanted) if host.grant(wanted).is_none() => {
            narrowed.push(Narrowing {
                capability: "profile".to_owned(),
                requested: wanted.word().to_owned(),
                granted: "none".to_owned(),
                reason: format!("host profiles does not list {}", wanted.word()),
            });
            None
        }
        other => other,
    };
    let mut read = runtime_paths(host, profile, code_root, home, &mut narrowed)?;
    read.insert(workspace.to_path_buf());
    read.insert(tmp.to_path_buf());
    for relative in &request.read {
        read.insert(resolve_inside(workspace, relative)?);
    }
    let mut write = BTreeSet::new();
    write.insert(workspace.to_path_buf());
    write.insert(tmp.to_path_buf());
    for relative in &request.write {
        write.insert(resolve_inside(workspace, relative)?);
    }
    let mut env_writes = BTreeMap::new();
    for name in &request.write_env {
        let Some(raw_roots) = host.write_env.get(name) else {
            narrowed.push(write_env_narrowing(
                name,
                format!("host write_env does not list {name}"),
            ));
            continue;
        };
        let roots = absolute_roots(raw_roots, code_root, home)?;
        let bounds = write_env::Bounds {
            workspace,
            code_root,
            roots: &roots,
        };
        match write_env::grant(name, var(name), &bounds) {
            Ok(dir) => {
                read.insert(dir.clone());
                write.insert(dir.clone());
                env_writes.insert(name.clone(), dir);
            }
            Err(reason) => narrowed.push(write_env_narrowing(name, reason)),
        }
    }
    let sockets = grant_sockets(host, &request.sockets, code_root, home, &mut narrowed)?;
    let net = if request.net == Net::None || host.allow_net.contains(&request.net) {
        request.net
    } else {
        narrowed.push(Narrowing {
            capability: "net".to_owned(),
            requested: request.net.word().to_owned(),
            granted: Net::None.word().to_owned(),
            reason: format!("host allow_net does not list {}", request.net.word()),
        });
        Net::None
    };
    let effective = Effective {
        workspace: workspace.to_path_buf(),
        read: read.into_iter().collect(),
        write: write.into_iter().collect(),
        net,
        tmp: tmp.to_path_buf(),
        write_env: env_writes,
        profile,
        sockets,
    };
    Ok((effective, narrowed))
}

/// The requested sockets the host lists and that exist as sockets now; every other request is a
/// `socket` narrowing. A listed path that is not absolute is an invalid policy (exit 101).
fn grant_sockets(
    host: &HostPolicy,
    requested: &[String],
    code_root: &Path,
    home: &Path,
    narrowed: &mut Vec<Narrowing>,
) -> Result<BTreeMap<String, PathBuf>, Failure> {
    use std::os::unix::fs::FileTypeExt;
    let mut granted = BTreeMap::new();
    for name in requested {
        let refuse = |reason: String| Narrowing {
            capability: "socket".to_owned(),
            requested: name.clone(),
            granted: "none".to_owned(),
            reason,
        };
        let Some(raw) = host.sockets.get(name) else {
            narrowed.push(refuse(format!("host sockets does not list {name}")));
            continue;
        };
        let path = PathBuf::from(expand(raw, code_root, home)?);
        if !path.is_absolute() {
            return Err(unenforceable(format!(
                "invalid policy: socket {name} is not absolute: {raw}"
            )));
        }
        match std::fs::metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() => {
                granted.insert(name.clone(), path);
            }
            Ok(_) => narrowed.push(refuse(format!("{} is not a socket", path.display()))),
            Err(error) => narrowed.push(refuse(format!("{}: {error}", path.display()))),
        }
    }
    Ok(granted)
}

fn write_env_narrowing(name: &str, reason: String) -> Narrowing {
    Narrowing {
        capability: "write_env".to_owned(),
        requested: name.to_owned(),
        granted: "none".to_owned(),
        reason,
    }
}

fn absolute_roots(raw: &[String], code_root: &Path, home: &Path) -> Result<Vec<PathBuf>, Failure> {
    let mut roots = Vec::new();
    for entry in raw {
        let expanded = PathBuf::from(expand(entry, code_root, home)?);
        if !expanded.is_absolute() {
            return Err(unenforceable(format!(
                "invalid policy: write_env root is not absolute: {entry}"
            )));
        }
        roots.push(expanded);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn host(allow_net: &[Net]) -> HostPolicy {
        HostPolicy {
            version: 1,
            runtime_read: RuntimeRead::All(vec!["/usr".to_owned(), "$CODE_ROOT/tools".to_owned()]),
            allow_net: allow_net.to_vec(),
            write_env: BTreeMap::new(),
            profiles: Profiles::default(),
            sockets: BTreeMap::new(),
        }
    }

    fn workspace() -> Result<tempfile::TempDir, String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        std::fs::create_dir(dir.path().join("sub")).map_err(|error| error.to_string())?;
        std::fs::write(dir.path().join("a.txt"), "alpha\n").map_err(|error| error.to_string())?;
        Ok(dir)
    }

    fn canonical(dir: &tempfile::TempDir) -> Result<PathBuf, String> {
        dir.path().canonicalize().map_err(|error| error.to_string())
    }

    #[test]
    fn host_policy_parses_and_rejects_bad_shapes() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let file = dir.path().join("p.json");
        let good = r#"{"version":1,"runtime_read":["/usr"],"allow_net":["none","all"]}"#;
        std::fs::write(&file, good).map_err(|error| error.to_string())?;
        let parsed = load_host(&file).map_err(|failure| failure.message().to_owned())?;
        assert_eq!(parsed, host(&[Net::None, Net::All]).with_usr_only());
        let with_env = r#"{"version":1,"runtime_read":["/usr"],"allow_net":["none"],"write_env":{"LAB_X":["$HOME/.contextstack"]}}"#;
        std::fs::write(&file, with_env).map_err(|error| error.to_string())?;
        let parsed = load_host(&file).map_err(|failure| failure.message().to_owned())?;
        assert_eq!(
            parsed.write_env.get("LAB_X"),
            Some(&vec!["$HOME/.contextstack".to_owned()])
        );
        let with_profile = r#"{"version":1,"runtime_read":["/usr"],"allow_net":["all"],"profiles":{"browser":{"read":{"macos":["$HOME/b"]}}}}"#;
        std::fs::write(&file, with_profile).map_err(|error| error.to_string())?;
        let parsed = load_host(&file).map_err(|failure| failure.message().to_owned())?;
        let reads = parsed.profiles.browser.map(|grant| grant.read);
        let want = RuntimeRead::PerOs(PerOs {
            macos: Some(vec!["$HOME/b".to_owned()]),
            linux: None,
        });
        assert_eq!(reads, Some(want));
        for bad in [
            "{not json",
            r#"{"version":2,"runtime_read":[],"allow_net":[]}"#,
            r#"{"version":1,"runtime_read":[],"allow_net":["lan"]}"#,
            r#"{"version":1,"runtime_read":[],"allow_net":[],"extra":1}"#,
            r#"{"version":1,"runtime_read":[],"allow_net":[],"tmp_in_workspace":".lab-tmp"}"#,
            r#"{"version":1,"runtime_read":[],"allow_net":[],"write_env":["LAB_X"]}"#,
            r#"{"version":1,"runtime_read":[],"allow_net":[],"write_env":{"LAB_X":"/s"}}"#,
            r#"{"version":1,"allow_net":[]}"#,
            r#"{"version":1,"runtime_read":{"windows":["/x"]},"allow_net":[]}"#,
            r#"{"version":1,"runtime_read":{"linux":"/usr"},"allow_net":[]}"#,
            r#"{"version":1,"runtime_read":[],"allow_net":[],"profiles":{"gpu":{"read":[]}}}"#,
            r#"{"version":1,"runtime_read":[],"allow_net":[],"profiles":{"browser":{}}}"#,
            r#"{"version":1,"runtime_read":[],"allow_net":[],"profiles":{"browser":{"read":[],"mach":[]}}}"#,
        ] {
            std::fs::write(&file, bad).map_err(|error| error.to_string())?;
            let code = load_host(&file).err().map(|failure| failure.code());
            assert_eq!(code, Some(101), "{bad}");
        }
        let missing = load_host(&dir.path().join("absent.json"))
            .err()
            .map(|f| f.code());
        assert_eq!(missing, Some(101));
        Ok(())
    }

    #[test]
    fn runtime_read_is_one_list_or_one_list_per_os() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let file = dir.path().join("p.json");
        let per_os = r#"{"version":1,"runtime_read":{"macos":["/opt/homebrew"],"linux":["/usr"]},"allow_net":["none"]}"#;
        std::fs::write(&file, per_os).map_err(|error| error.to_string())?;
        let parsed = load_host(&file).map_err(|failure| failure.message().to_owned())?;
        let list = |os: &str| parsed.runtime_read.for_os(os).map(<[String]>::to_vec);
        assert_eq!(list("macos"), Ok(vec!["/opt/homebrew".to_owned()]));
        assert_eq!(list("linux"), Ok(vec!["/usr".to_owned()]));
        assert_eq!(list("freebsd").err().map(|f| f.code()), Some(101));
        let only_mac = RuntimeRead::PerOs(PerOs {
            macos: Some(vec!["/usr".to_owned()]),
            linux: None,
        });
        let missing = only_mac.for_os("linux").err();
        assert_eq!(missing.as_ref().map(Failure::code), Some(101));
        assert!(
            missing
                .map(|f| f.message().to_owned())
                .unwrap_or_default()
                .contains("no list for linux")
        );
        let all = RuntimeRead::All(vec!["/usr".to_owned()]);
        assert_eq!(all.for_os("linux"), all.for_os("macos"));
        Ok(())
    }

    #[test]
    fn runtime_links_name_every_symlinked_prefix() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = dir
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        std::fs::create_dir_all(root.join("usr/lib")).map_err(|e| e.to_string())?;
        std::fs::write(root.join("usr/lib/ld.so"), "").map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink("usr/lib", root.join("lib64")).map_err(|e| e.to_string())?;
        let mut policy = host(&[]);
        policy.runtime_read = RuntimeRead::All(vec![
            format!("{}/lib64/ld.so", root.display()),
            format!("{}/usr", root.display()),
        ]);
        let links = runtime_links(&policy, None, Path::new("/c"), Path::new("/h"))
            .map_err(|failure| failure.message().to_owned())?;
        assert_eq!(links, vec![(root.join("lib64"), root.join("usr/lib"))]);
        Ok(())
    }

    /// A code root whose `.venv/pyvenv.cfg` names `home` (None: no venv), and a base install outside it.
    fn venv_root(home: Option<&str>) -> Result<(tempfile::TempDir, PathBuf, PathBuf), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let root = canonical(&dir)?;
        let base = root.join("python/cpython-3.13.9-test");
        std::fs::create_dir_all(base.join("bin")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(base.join("lib")).map_err(|e| e.to_string())?;
        let code = root.join("cs");
        std::fs::create_dir_all(code.join(".venv/bin")).map_err(|e| e.to_string())?;
        if let Some(home) = home {
            let home = home.replace("<BASE>", &base.to_string_lossy());
            let cfg = format!("home = {home}\nimplementation = CPython\nversion_info = 3.13.9\n");
            std::fs::write(code.join(".venv/pyvenv.cfg"), cfg).map_err(|e| e.to_string())?;
        }
        Ok((dir, code, base))
    }

    #[test]
    fn tool_python_is_the_venv_base_install_and_only_that() -> Result<(), String> {
        let (_dir, code, base) = venv_root(Some("<BASE>/bin"))?;
        assert_eq!(tool_python(&code), Some(base.clone()));
        let mut policy = host(&[]);
        policy.runtime_read = RuntimeRead::All(vec!["/usr".to_owned(), TOOL_PYTHON.to_owned()]);
        let mut narrowed = Vec::new();
        let paths = runtime_paths(&policy, None, &code, Path::new("/h"), &mut narrowed)
            .map_err(|failure| failure.message().to_owned())?;
        let canonical_usr = Path::new("/usr")
            .canonicalize()
            .map_err(|e| e.to_string())?;
        assert_eq!(paths, BTreeSet::from([canonical_usr, base.clone()]));
        assert!(!paths.iter().any(|p| p == base.parent().unwrap_or(&base)));
        // home through a symlink: the canonical install, as Seatbelt and bwrap see it.
        let (_dir2, code2, base2) = venv_root(Some("<BASE>/bin"))?;
        let alias = code2.parent().map(|p| p.join("alias")).ok_or("no parent")?;
        std::os::unix::fs::symlink(&base2, &alias).map_err(|e| e.to_string())?;
        let cfg = format!("home = {}/bin\n", alias.display());
        std::fs::write(code2.join(".venv/pyvenv.cfg"), cfg).map_err(|e| e.to_string())?;
        assert_eq!(tool_python(&code2), Some(base2));
        Ok(())
    }

    #[test]
    fn tool_python_unresolved_drops_the_entry() -> Result<(), String> {
        let mut policy = host(&[]);
        policy.runtime_read = RuntimeRead::All(vec!["/usr".to_owned(), TOOL_PYTHON.to_owned()]);
        let usr = Path::new("/usr")
            .canonicalize()
            .map_err(|e| e.to_string())?;
        for home in [None, Some("<BASE>/absent/bin")] {
            let (_dir, code, _) = venv_root(home)?;
            assert_eq!(tool_python(&code), None, "{home:?}");
            let mut narrowed = Vec::new();
            let paths = runtime_paths(&policy, None, &code, Path::new("/h"), &mut narrowed)
                .map_err(|failure| failure.message().to_owned())?;
            assert_eq!(paths, BTreeSet::from([usr.clone()]), "{home:?}");
            assert_eq!(narrowed.len(), 1, "{home:?}");
            assert_eq!(narrowed[0].capability, "read");
            assert_eq!(narrowed[0].requested, TOOL_PYTHON);
            assert_eq!(narrowed[0].granted, "none");
            assert!(narrowed[0].reason.contains(TOOL_PYTHON));
            let links = runtime_links(&policy, None, &code, Path::new("/h"))
                .map_err(|failure| failure.message().to_owned())?;
            assert!(
                links
                    .iter()
                    .all(|(link, _)| !link.ends_with("$TOOL_PYTHON")),
                "{links:?}"
            );
        }
        // A home whose base would be `/` grants nothing rather than the whole filesystem.
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        std::fs::create_dir_all(dir.path().join(".venv")).map_err(|e| e.to_string())?;
        std::fs::write(dir.path().join(".venv/pyvenv.cfg"), "home = /\n")
            .map_err(|e| e.to_string())?;
        assert_eq!(tool_python(dir.path()), None);
        Ok(())
    }

    impl HostPolicy {
        fn with_usr_only(mut self) -> Self {
            self.runtime_read = RuntimeRead::All(vec!["/usr".to_owned()]);
            self
        }
    }

    #[test]
    fn expansion_covers_both_variables() -> Result<(), Failure> {
        let expanded = expand(
            "$CODE_ROOT/tools:$HOME/.local",
            Path::new("/code"),
            Path::new("/me"),
        )?;
        assert_eq!(expanded, "/code/tools:/me/.local");
        Ok(())
    }

    #[test]
    fn expansion_includes_repo_root() -> Result<(), Failure> {
        let expanded = expand(
            "$REPO_ROOT/target/release",
            Path::new("/repo/cs"),
            Path::new("/me"),
        )?;
        assert_eq!(expanded, "/repo/target/release");
        Ok(())
    }

    #[test]
    fn expansion_refuses_empty_home() {
        assert_eq!(
            expand("$HOME/.contextstack", Path::new("/code"), Path::new(""))
                .err()
                .map(|f| f.code()),
            Some(crate::failure::UNENFORCEABLE),
        );
    }

    #[test]
    fn require_home_refuses_unset_and_empty() {
        assert_eq!(
            require_home(None).err().map(|f| f.code()),
            Some(crate::failure::UNENFORCEABLE),
        );
        assert_eq!(
            require_home(Some(std::ffi::OsStr::new("")))
                .err()
                .map(|f| f.code()),
            Some(crate::failure::UNENFORCEABLE),
        );
    }

    #[test]
    fn request_defaults_and_rejects_unknown_fields() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        assert_eq!(load_request(None), Ok(Request::default()));
        let descriptor = dir.path().join("tool.json");
        let tool = Some(descriptor.as_path());
        std::fs::write(&descriptor, r#"{"runtime":"python3"}"#)
            .map_err(|error| error.to_string())?;
        assert_eq!(load_request(tool), Ok(Request::default()));
        std::fs::write(
            &descriptor,
            r#"{"sandbox":{"net":"all","read":["in"],"write":["out"]}}"#,
        )
        .map_err(|error| error.to_string())?;
        let request = load_request(tool).map_err(|failure| failure.message().to_owned())?;
        assert_eq!(request.net, Net::All);
        assert_eq!(request.read, vec!["in".to_owned()]);
        assert_eq!(request.write, vec!["out".to_owned()]);
        for bad in [
            r#"{"sandbox":{"net":"lan"}}"#,
            r#"{"sandbox":{"exec":true}}"#,
            r#"{"sandbox":{"profile":"gpu"}}"#,
            r#"{"sandbox":[]}"#,
            "{broken",
        ] {
            std::fs::write(&descriptor, bad).map_err(|error| error.to_string())?;
            assert_eq!(
                load_request(tool).err().map(|f| f.code()),
                Some(101),
                "{bad}"
            );
        }
        Ok(())
    }

    #[test]
    fn exe_tools_find_their_descriptor_by_bin_and_args() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let repo = dir
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let code_root = repo.join("lab");
        let bin_dir = code_root.join("tools/bin");
        std::fs::create_dir_all(&bin_dir).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(repo.join("target")).map_err(|error| error.to_string())?;
        let exe = repo.join("target/multi");
        std::fs::write(&exe, "").map_err(|error| error.to_string())?;
        let write = |name: &str, body: &str| std::fs::write(bin_dir.join(name), body);
        write(
            "fs_read.json",
            r#"{"runtime":"exe","bin":"target/multi","args":["fs_read"],"sandbox":{"net":"all"}}"#,
        )
        .map_err(|error| error.to_string())?;
        write(
            "fs_edit.json",
            r#"{"runtime":"exe","bin":"target/multi","args":["fs_edit"]}"#,
        )
        .map_err(|error| error.to_string())?;
        write("py.json", r#"{"runtime":"python3","args":["fs_read"]}"#)
            .map_err(|error| error.to_string())?;
        write("broken.json", "{broken").map_err(|error| error.to_string())?;
        let args = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        let found = descriptor_for(&exe, &args(&["fs_read"]), &code_root)
            .map_err(|failure| failure.message().to_owned())?;
        assert_eq!(found, Some(bin_dir.join("fs_read.json")));
        let request = load_request(found.as_deref()).map_err(|f| f.message().to_owned())?;
        assert_eq!(request.net, Net::All);
        for other in [&["fs_read", "extra"][..], &[], &["fs_list"]] {
            assert_eq!(descriptor_for(&exe, &args(other), &code_root), Ok(None));
        }
        let absolute = format!(
            r#"{{"runtime":"exe","bin":"{}","args":["fs_read"]}}"#,
            exe.display()
        );
        write("twin.json", &absolute).map_err(|error| error.to_string())?;
        assert_eq!(
            descriptor_for(&exe, &args(&["fs_read"]), &code_root)
                .err()
                .map(|f| f.code()),
            Some(101)
        );
        std::fs::write(repo.join("target/multi.json"), "{}").map_err(|e| e.to_string())?;
        assert_eq!(
            descriptor_for(&exe, &args(&["fs_read"]), &code_root),
            Ok(Some(repo.join("target/multi.json")))
        );
        Ok(())
    }

    #[test]
    fn grant_tool_read_adds_the_binary_and_parent_when_outside_runtime_paths() -> Result<(), String>
    {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let host = host(&[Net::None]);
        let (mut effective, _) = intersect(
            &host,
            &Request::default(),
            &ws,
            Path::new("/private/tmp/cs-sandbox-grant"),
            Path::new("/code"),
            Path::new("/me"),
            &no_env,
        )
        .map_err(|failure| failure.message().to_owned())?;
        let tool = PathBuf::from("/code/fixture/multi");
        assert!(!effective.read.iter().any(|p| tool.starts_with(p)));
        grant_tool_read(&mut effective, &tool);
        assert!(effective.read.contains(&tool));
        assert!(effective.read.contains(&PathBuf::from("/code/fixture")));
        let covered = PathBuf::from("/code/tools/bin/fs_read");
        let before = effective.read.len();
        grant_tool_read(&mut effective, &covered);
        assert_eq!(effective.read.len(), before);
        Ok(())
    }

    #[test]
    fn intersection_narrows_net_and_records_it() -> Result<(), String> {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let request = Request {
            net: Net::All,
            read: vec!["sub".to_owned()],
            write: vec!["sub/new".to_owned()],
            write_env: Vec::new(),
            profile: None,
            sockets: Vec::new(),
        };
        let (effective, narrowed) = intersect(
            &host(&[Net::None]),
            &request,
            &ws,
            Path::new("/private/tmp/cs-sandbox-1-2"),
            Path::new("/code"),
            Path::new("/me"),
            &no_env,
        )
        .map_err(|failure| failure.message().to_owned())?;
        assert_eq!(effective.net, Net::None);
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].capability, "net");
        assert_eq!(narrowed[0].requested, "all");
        assert_eq!(narrowed[0].granted, "none");
        assert!(effective.read.contains(&PathBuf::from("/code/tools")));
        assert!(effective.read.contains(&ws));
        assert!(effective.read.contains(&ws.join("sub")));
        let tmp = PathBuf::from("/private/tmp/cs-sandbox-1-2");
        assert_eq!(effective.tmp, tmp);
        assert!(effective.read.contains(&tmp));
        assert!(effective.write.contains(&tmp));
        assert!(effective.write.contains(&ws.join("sub/new")));
        Ok(())
    }

    #[test]
    fn intersection_grants_a_listed_net_without_narrowing() -> Result<(), String> {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let request = Request {
            net: Net::Gateway,
            ..Request::default()
        };
        let (effective, narrowed) = intersect(
            &host(&[Net::Gateway]),
            &request,
            &ws,
            Path::new("/private/tmp/cs-sandbox-3-4"),
            Path::new("/code"),
            Path::new("/me"),
            &no_env,
        )
        .map_err(|failure| failure.message().to_owned())?;
        assert_eq!(effective.net, Net::Gateway);
        assert_eq!(narrowed, Vec::new());
        let tmp = PathBuf::from("/private/tmp/cs-sandbox-3-4");
        assert_eq!(effective.tmp, tmp);
        let mut want = vec![ws, tmp];
        want.sort();
        assert_eq!(effective.write, want);
        Ok(())
    }

    #[test]
    fn a_profile_is_granted_only_when_the_host_lists_it() -> Result<(), String> {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let request = Request {
            profile: Some(Profile::Browser),
            ..Request::default()
        };
        let tmp = Path::new("/private/tmp/cs-sandbox-9-9");
        let run = |host: &HostPolicy| {
            intersect(
                host,
                &request,
                &ws,
                tmp,
                Path::new("/code"),
                Path::new("/me"),
                &no_env,
            )
            .map_err(|failure| failure.message().to_owned())
        };
        let (effective, narrowed) = run(&host(&[]))?;
        assert_eq!(effective.profile, None);
        assert!(!effective.read.contains(&PathBuf::from("/me/browsers")));
        assert_eq!(
            narrowed,
            vec![Narrowing {
                capability: "profile".to_owned(),
                requested: "browser".to_owned(),
                granted: "none".to_owned(),
                reason: "host profiles does not list browser".to_owned(),
            }]
        );
        let mut allowing = host(&[]);
        allowing.profiles.browser = Some(ProfileGrant {
            read: RuntimeRead::All(vec!["$HOME/browsers".to_owned()]),
        });
        let (effective, narrowed) = run(&allowing)?;
        assert_eq!(effective.profile, Some(Profile::Browser));
        assert!(effective.read.contains(&PathBuf::from("/me/browsers")));
        assert!(!effective.write.contains(&PathBuf::from("/me/browsers")));
        assert_eq!(narrowed, Vec::new());
        let (effective, _) = intersect(
            &allowing,
            &Request::default(),
            &ws,
            tmp,
            Path::new("/code"),
            Path::new("/me"),
            &no_env,
        )
        .map_err(|failure| failure.message().to_owned())?;
        assert_eq!(effective.profile, None);
        assert!(!effective.read.contains(&PathBuf::from("/me/browsers")));
        Ok(())
    }

    #[test]
    fn a_socket_is_granted_only_when_listed_and_a_live_socket() -> Result<(), String> {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let state = tempfile::tempdir_in("/tmp").map_err(|error| error.to_string())?;
        let live = state.path().join("live.sock");
        let _listener =
            std::os::unix::net::UnixListener::bind(&live).map_err(|error| error.to_string())?;
        std::fs::write(state.path().join("file.sock"), "").map_err(|error| error.to_string())?;
        let mut policy = host(&[Net::None]);
        for name in ["live", "file", "gone"] {
            let path = state.path().join(format!("{name}.sock"));
            policy
                .sockets
                .insert(format!("b-{name}"), path.display().to_string());
        }
        let request = Request {
            sockets: ["b-live", "b-file", "b-gone", "b-unlisted"]
                .map(str::to_owned)
                .to_vec(),
            ..Request::default()
        };
        let tmp = Path::new("/private/tmp/cs-sandbox-9-9");
        let (effective, narrowed) = intersect(
            &policy,
            &request,
            &ws,
            tmp,
            Path::new("/code"),
            Path::new("/home/me"),
            &no_env,
        )
        .map_err(|failure| failure.message().to_owned())?;
        assert_eq!(
            effective.sockets,
            BTreeMap::from([("b-live".to_owned(), live.clone())])
        );
        assert_eq!(effective.net, Net::None);
        let refused: Vec<(&str, &str)> = narrowed
            .iter()
            .map(|n| (n.capability.as_str(), n.requested.as_str()))
            .collect();
        assert_eq!(
            refused,
            [
                ("socket", "b-file"),
                ("socket", "b-gone"),
                ("socket", "b-unlisted")
            ]
        );
        assert!(narrowed[0].reason.ends_with("is not a socket"));
        assert_eq!(narrowed[2].reason, "host sockets does not list b-unlisted");
        policy
            .sockets
            .insert("b-rel".to_owned(), "rel.sock".to_owned());
        let relative = Request {
            sockets: vec!["b-rel".to_owned()],
            ..Request::default()
        };
        let refused = intersect(
            &policy,
            &relative,
            &ws,
            tmp,
            Path::new("/code"),
            Path::new("/home/me"),
            &no_env,
        );
        assert_eq!(refused.err().map(|f| f.code()), Some(101));
        Ok(())
    }

    #[test]
    fn socket_names_are_checked_and_become_variables() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let file = dir.path().join("p.json");
        let good = r#"{"version":1,"runtime_read":["/usr"],"allow_net":["none"],"sockets":{"browser-public":"$HOME/.contextstack/b.sock"}}"#;
        std::fs::write(&file, good).map_err(|error| error.to_string())?;
        let parsed = load_host(&file).map_err(|failure| failure.message().to_owned())?;
        assert_eq!(parsed.sockets.len(), 1);
        for bad in ["Browser", "-x", "a_b", "a b", ""] {
            let text = format!(
                r#"{{"version":1,"runtime_read":["/usr"],"allow_net":["none"],"sockets":{{"{bad}":"/s"}}}}"#
            );
            std::fs::write(&file, text).map_err(|error| error.to_string())?;
            assert_eq!(load_host(&file).err().map(|f| f.code()), Some(101), "{bad}");
        }
        assert_eq!(socket_var("browser-public"), "CS_SOCKET_BROWSER_PUBLIC");
        let parsed: Request = serde_json::from_str(r#"{"sockets":["browser-public"]}"#)
            .map_err(|error| error.to_string())?;
        assert_eq!(parsed.sockets, vec!["browser-public".to_owned()]);
        Ok(())
    }

    #[test]
    fn relative_runtime_read_is_refused() -> Result<(), String> {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let mut bad = host(&[]);
        bad.runtime_read = RuntimeRead::All(vec!["tools".to_owned()]);
        let code = intersect(
            &bad,
            &Request::default(),
            &ws,
            Path::new("/private/tmp/cs-sandbox-5-6"),
            Path::new("/c"),
            Path::new("/h"),
            &no_env,
        )
        .err()
        .map(|failure| failure.code());
        assert_eq!(code, Some(101));
        Ok(())
    }

    #[test]
    fn escapes_are_refused_lexically_and_through_symlinks() -> Result<(), String> {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let outside = tempfile::tempdir().map_err(|error| error.to_string())?;
        std::os::unix::fs::symlink(outside.path(), ws.join("linkdir"))
            .map_err(|error| error.to_string())?;
        for relative in [
            "../x",
            "/abs",
            "",
            "sub/../../x",
            "linkdir",
            "linkdir/new.txt",
        ] {
            let code = resolve_inside(&ws, relative)
                .err()
                .map(|failure| failure.code());
            assert_eq!(code, Some(101), "{relative}");
        }
        assert_eq!(resolve_inside(&ws, "sub"), Ok(ws.join("sub")));
        assert_eq!(resolve_inside(&ws, "./a.txt"), Ok(ws.join("a.txt")));
        assert_eq!(
            resolve_inside(&ws, "sub/deep/new"),
            Ok(ws.join("sub/deep/new"))
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn runtime_reads_are_canonical_on_macos() -> Result<(), String> {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let mut policy = host(&[]);
        policy.runtime_read = RuntimeRead::All(vec!["/etc".to_owned(), "/tmp".to_owned()]);
        let (effective, _) = intersect(
            &policy,
            &Request::default(),
            &ws,
            Path::new("/private/tmp/cs-sandbox-5-6"),
            Path::new("/c"),
            Path::new("/h"),
            &no_env,
        )
        .map_err(|failure| failure.message().to_owned())?;
        assert!(effective.read.contains(&PathBuf::from("/private/etc")));
        assert!(effective.read.contains(&PathBuf::from("/private/tmp")));
        Ok(())
    }

    #[test]
    fn write_env_is_granted_under_its_root_and_narrowed_otherwise() -> Result<(), String> {
        let dir = workspace()?;
        let ws = canonical(&dir)?;
        let home_dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let home = home_dir
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let state = home.join(".contextstack/lab/browser-state");
        let mut policy = host(&[Net::None]);
        policy
            .write_env
            .insert("LAB_X".to_owned(), vec!["$HOME/.contextstack".to_owned()]);
        let request = Request {
            write_env: vec!["LAB_X".to_owned(), "LAB_UNLISTED".to_owned()],
            ..Request::default()
        };
        let tmp = Path::new("/private/tmp/cs-sandbox-9-9");
        let set = |name: &str| (name == "LAB_X").then(|| state.clone().into_os_string());
        let (effective, narrowed) =
            intersect(&policy, &request, &ws, tmp, Path::new("/code"), &home, &set)
                .map_err(|failure| failure.message().to_owned())?;
        assert_eq!(effective.write_env.get("LAB_X"), Some(&state));
        assert!(effective.write.contains(&state));
        assert!(effective.read.contains(&state));
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].capability, "write_env");
        assert_eq!(narrowed[0].requested, "LAB_UNLISTED");
        assert!(narrowed[0].reason.contains("does not list"));
        let (unset, narrowed) = intersect(
            &policy,
            &request,
            &ws,
            tmp,
            Path::new("/code"),
            &home,
            &no_env,
        )
        .map_err(|failure| failure.message().to_owned())?;
        assert!(unset.write_env.is_empty());
        assert!(!unset.write.contains(&state));
        assert_eq!(narrowed[0].reason, "LAB_X is unset");
        policy
            .write_env
            .insert("LAB_X".to_owned(), vec!["relative/root".to_owned()]);
        let code = intersect(&policy, &request, &ws, tmp, Path::new("/code"), &home, &set)
            .err()
            .map(|failure| failure.code());
        assert_eq!(code, Some(101));
        Ok(())
    }
}
