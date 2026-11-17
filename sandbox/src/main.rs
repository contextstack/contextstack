//! `cs-sandbox`: run one CS tool under OS confinement (Seatbelt on macOS, bwrap plus seccomp on Linux).
//!
//! The wrapper reads a host policy and the tool's descriptor request, intersects them, and runs
//! the tool under the host's backend: on macOS a deny-default Seatbelt profile exec'd through
//! `sandbox-exec`; on Linux bwrap with an empty root built from the policy plus a seccomp
//! denylist. Stdin, stdout, stderr and the environment pass through. On success it prints nothing
//! and exits with the tool's code (128+n on a signal). Exit 2 is a usage error; exit 101 means
//! the policy cannot be enforced on this host, and the tool never ran.

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod bwrap;
mod cli;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod cli_executor;
mod env_deny;
mod failure;
mod json_status;
#[cfg(target_os = "linux")]
mod linux;
mod policy;
mod probe;
mod profile;
mod report;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod sbpl;
mod seatbelt;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod seccomp;
mod ssh_agent_deny;
mod tmp;
mod write_env;

use std::env;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::process::ExitStatus;
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::failure::{Failure, unenforceable, usage};
use crate::policy::{Effective, Narrowing, Net};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = match run(args) {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("cs-sandbox: {}", failure.message());
            failure.code()
        }
    };
    std::process::exit(code);
}

fn run(args: Vec<String>) -> Result<i32, Failure> {
    match cli::parse(args)? {
        cli::Command::Probe { policy } => probe(&policy),
        cli::Command::EmitCliExecutor { policy, request } => emit_cli_executor(&policy, &request),
        cli::Command::Run(run) => execute(&run),
    }
}

fn emit_cli_executor(policy_path: &Path, request_path: &Path) -> Result<i32, Failure> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (policy_path, request_path);
        Err(unenforceable(
            "cli-executor profile emit is only implemented on macOS".to_owned(),
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let host = policy::load_host(policy_path)?;
        cli_executor::assert_host_allows_cli_executor(&host)?;
        let body = if request_path == Path::new("-") {
            use std::io::Read;
            let mut body = String::new();
            std::io::stdin().read_to_string(&mut body).map(|_| body)
        } else {
            std::fs::read_to_string(request_path)
        }
        .map_err(|error| {
            unenforceable(format!("cannot read {}: {error}", request_path.display()))
        })?;
        let request: cli_executor::ExecutorRequest =
            serde_json::from_str(&body).map_err(|error| {
                unenforceable(format!(
                    "invalid executor request {}: {error}",
                    request_path.display()
                ))
            })?;
        let profile = cli_executor::seatbelt_profile(&request)?;
        print!("{profile}");
        Ok(0)
    }
}

#[cfg(target_os = "macos")]
fn backend_available() -> Result<(), Failure> {
    if !Path::new(seatbelt::SANDBOX_EXEC).is_file() {
        return Err(unenforceable(format!(
            "{} is missing; the tool was not run",
            seatbelt::SANDBOX_EXEC
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn backend_available() -> Result<(), Failure> {
    linux::bwrap_path().map(|_| ()).ok_or_else(|| {
        unenforceable(format!(
            "bwrap is not installed ({}); the tool was not run",
            linux::BWRAP_CANDIDATES.join(" or ")
        ))
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn backend_available() -> Result<(), Failure> {
    Err(unenforceable(format!(
        "no sandbox backend on {}; the tool was not run",
        env::consts::OS
    )))
}

#[cfg(not(target_os = "linux"))]
fn probe(policy_path: &Path) -> Result<i32, Failure> {
    policy::load_host(policy_path)?;
    let availability = backend_available();
    println!("{}", probe::document(availability.is_ok()));
    availability.map(|()| 0)
}

#[cfg(target_os = "linux")]
fn probe(policy_path: &Path) -> Result<i32, Failure> {
    policy::load_host(policy_path)?;
    let bwrap = linux::bwrap_path();
    let facts = probe::LinuxFacts {
        bwrap: bwrap
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        userns: bwrap.as_deref().is_some_and(linux::userns_works),
        apparmor_restrict_unprivileged_userns: linux::apparmor_userns_restriction(),
    };
    let enforceable = facts.bwrap.is_some() && facts.userns;
    println!("{}", probe::linux_document(enforceable, &facts));
    if enforceable {
        return Ok(0);
    }
    backend_available()?;
    Err(unenforceable(format!(
        "bwrap cannot create a user namespace on this host (apparmor_restrict_unprivileged_userns={}); tools cannot run confined",
        facts
            .apparmor_restrict_unprivileged_userns
            .map_or_else(|| "absent".to_owned(), |value| value.to_string())
    )))
}

fn resolve_tool(name: &str) -> Result<PathBuf, Failure> {
    let path = Path::new(name);
    if name.contains('/') {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()
                .map_err(|error| usage(format!("cannot read the current directory: {error}")))?
                .join(path)
        };
        if absolute.is_file() {
            return Ok(absolute);
        }
        return Err(usage(format!("tool not found: {name}")));
    }
    let search = env::var_os("PATH").unwrap_or_default();
    for dir in env::split_paths(&search) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(usage(format!("tool not found on PATH: {name}")))
}

fn workspace(cwd: Option<&Path>) -> Result<PathBuf, Failure> {
    let given = match cwd {
        Some(dir) => dir.to_path_buf(),
        None => env::current_dir()
            .map_err(|error| usage(format!("cannot read the current directory: {error}")))?,
    };
    let canonical = given
        .canonicalize()
        .map_err(|error| usage(format!("--cwd {}: {error}", given.display())))?;
    if !canonical.is_dir() {
        return Err(usage(format!(
            "--cwd {} is not a directory",
            given.display()
        )));
    }
    Ok(canonical)
}

fn gateway_socket(net: Net) -> Result<Option<PathBuf>, Failure> {
    if net != Net::Gateway {
        return Ok(None);
    }
    match env::var_os(seatbelt::GATEWAY_SOCKET_VAR) {
        Some(value) if !value.is_empty() => Ok(Some(PathBuf::from(value))),
        _ => Err(unenforceable(format!(
            "net gateway needs {} to name the gateway socket; the tool was not run",
            seatbelt::GATEWAY_SOCKET_VAR
        ))),
    }
}

fn outcome(status: ExitStatus) -> (Option<i32>, Option<i32>, i32) {
    if let Some(code) = status.code() {
        return (Some(code), None, code);
    }
    if let Some(signal) = status.signal() {
        return (None, Some(signal), 128 + signal);
    }
    (None, None, 1)
}

/// What every backend needs to launch one call.
struct Call<'a> {
    run: &'a cli::Run,
    tool: &'a Path,
    workspace: &'a Path,
    tmp: &'a Path,
    socket: Option<&'a Path>,
    /// Symlinks on the way to runtime paths; bwrap recreates them, Seatbelt needs none.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    links: Vec<(PathBuf, PathBuf)>,
}

/// What a backend reports back: the exit status, the pid for the report name, the profile hash.
struct Launched {
    status: ExitStatus,
    pid: u32,
    profile_sha256: String,
}

#[cfg(target_os = "macos")]
const BACKEND: &str = seatbelt::BACKEND;
#[cfg(not(target_os = "macos"))]
const BACKEND: &str = bwrap::BACKEND;

#[cfg(target_os = "macos")]
fn launch(
    call: &Call<'_>,
    effective: &mut Effective,
    _: &mut Vec<Narrowing>,
) -> Result<Launched, Failure> {
    let profile = seatbelt::profile(effective, call.socket)?;
    let profile_sha256 = format!("{:x}", Sha256::digest(profile.as_bytes()));
    let mut command = Command::new(seatbelt::SANDBOX_EXEC);
    command
        .arg("-p")
        .arg(&profile)
        .arg(call.tool)
        .args(&call.run.args);
    command.current_dir(call.workspace);
    let tmp = call.tmp;
    command.env("TMPDIR", tmp).env("TMP", tmp).env("TEMP", tmp);
    let extra = effective.profile.map(|p| p.env(tmp, env::consts::OS));
    for (name, value) in extra.unwrap_or_default() {
        command.env(name, value);
    }
    for (name, path) in &effective.sockets {
        command.env(policy::socket_var(name), path);
    }
    let mut child = command.spawn().map_err(|error| {
        unenforceable(format!("cannot start {}: {error}", seatbelt::SANDBOX_EXEC))
    })?;
    let status = child
        .wait()
        .map_err(|error| unenforceable(format!("waiting for the tool failed: {error}")))?;
    Ok(Launched {
        status,
        pid: child.id(),
        profile_sha256,
    })
}

#[cfg(target_os = "linux")]
fn launch(
    call: &Call<'_>,
    effective: &mut Effective,
    narrowed: &mut Vec<Narrowing>,
) -> Result<Launched, Failure> {
    let bwrap_bin = linux::bwrap_path()
        .ok_or_else(|| unenforceable("bwrap is not installed; the tool was not run"))?;
    let plan = bwrap::plan(effective, &call.links, call.tool);
    effective.read.retain(|path| !plan.missing.contains(path));
    narrowed.extend(bwrap::missing_narrowings(&plan.missing));
    let canonical_socket = match call.socket {
        Some(socket) => Some(socket.canonicalize().map_err(|error| {
            unenforceable(format!(
                "gateway socket {}: {error}; the tool was not run",
                socket.display()
            ))
        })?),
        None => None,
    };
    let mut sockets = Vec::new();
    let mut socket_vars = Vec::new();
    for (name, seen) in &effective.sockets {
        let canonical = seen.canonicalize().map_err(|error| {
            unenforceable(format!(
                "socket {name} {}: {error}; the tool was not run",
                seen.display()
            ))
        })?;
        sockets.push((canonical, seen.clone()));
        socket_vars.push(policy::socket_var(name));
    }
    let tmp = call.tmp;
    let mut env = vec![
        ("TMPDIR", tmp.as_os_str()),
        ("TMP", tmp.as_os_str()),
        ("TEMP", tmp.as_os_str()),
    ];
    let extra = effective
        .profile
        .map(|p| p.env(tmp, env::consts::OS))
        .unwrap_or_default();
    env.extend(extra.iter().map(|(name, value)| (*name, value.as_os_str())));
    env.extend(
        socket_vars
            .iter()
            .zip(&sockets)
            .map(|(name, (_, seen))| (name.as_str(), seen.as_os_str())),
    );
    let ran = linux::run(
        &bwrap_bin,
        &plan,
        &linux::Call {
            net: effective.net,
            gateway: canonical_socket.as_deref().zip(call.socket),
            sockets: &sockets,
            workspace: call.workspace,
            tool: call.tool,
            args: &call.run.args,
            env: &env,
        },
    )?;
    Ok(Launched {
        status: ran.status,
        pid: ran.pid,
        profile_sha256: format!("{:x}", Sha256::digest(&ran.identity)),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn launch(_: &Call<'_>, _: &mut Effective, _: &mut Vec<Narrowing>) -> Result<Launched, Failure> {
    backend_available().map(|()| unreachable!())
}

fn execute(run: &cli::Run) -> Result<i32, Failure> {
    backend_available()?;
    let host = policy::load_host(&run.policy)?;
    let code_root = policy::code_root(&run.policy);
    let home = policy::home_from_env()?;
    let workspace = workspace(run.cwd.as_deref())?;
    let tool = resolve_tool(&run.tool)?;
    let descriptor = policy::descriptor_for(&tool, &run.args, &code_root)?;
    let request = policy::load_request(descriptor.as_deref())?;
    // Removed when this function returns, on every path, after the child has exited.
    let call_tmp = tmp::CallTmp::create(&env::temp_dir())?;
    let tmp = call_tmp.path();
    let (mut effective, mut narrowed) = policy::intersect(
        &host,
        &request,
        &workspace,
        tmp,
        &code_root,
        &home,
        &|name: &str| env::var_os(name),
    )?;
    policy::grant_tool_read(&mut effective, &tool);
    let socket = gateway_socket(effective.net)?;
    let links = if cfg!(target_os = "linux") {
        policy::runtime_links(&host, effective.profile, &code_root, &home)?
    } else {
        Vec::new()
    };
    let call = Call {
        run,
        tool: &tool,
        workspace: &workspace,
        tmp,
        socket: socket.as_deref(),
        links,
    };
    let started = Instant::now();
    let launched = launch(&call, &mut effective, &mut narrowed)?;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (exit, signal, code) = outcome(launched.status);
    if let Some(dir) = &run.report_dir {
        let report = report::Report {
            backend: BACKEND,
            tool: tool.to_string_lossy().into_owned(),
            args: &run.args,
            cwd: &workspace,
            effective_policy: &effective,
            profile_sha256: launched.profile_sha256,
            narrowed: &narrowed,
            exit,
            signal,
            duration_ms,
        };
        if let Err(error) = report::write(dir, launched.pid, &report) {
            eprintln!("cs-sandbox: report not written: {error}");
        }
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_deaths_map_to_128_plus_n() {
        let killed = ExitStatus::from_raw(9);
        assert_eq!(outcome(killed), (None, Some(9), 137));
        let exited = ExitStatus::from_raw(3 << 8);
        assert_eq!(outcome(exited), (Some(3), None, 3));
    }

    #[test]
    fn home_from_env_refuses_unset_and_empty() {
        assert_eq!(
            policy::require_home(None).err().map(|f| f.code()),
            Some(crate::failure::UNENFORCEABLE),
        );
        assert_eq!(
            policy::require_home(Some(std::ffi::OsStr::new("")))
                .err()
                .map(|f| f.code()),
            Some(crate::failure::UNENFORCEABLE),
        );
        let home = env::var_os("HOME");
        if let Some(value) = home.as_ref()
            && !value.is_empty()
        {
            assert_eq!(policy::home_from_env(), Ok(PathBuf::from(value)));
        }
    }

    #[test]
    fn gateway_without_socket_is_unenforceable() {
        // SAFETY-free: std::env mutation is unsafe in edition 2024, so this test only reads.
        if env::var_os(seatbelt::GATEWAY_SOCKET_VAR).is_none() {
            assert_eq!(
                gateway_socket(Net::Gateway).err().map(|f| f.code()),
                Some(101)
            );
        }
        assert_eq!(gateway_socket(Net::None), Ok(None));
        assert_eq!(gateway_socket(Net::All), Ok(None));
    }

    #[test]
    fn bare_names_resolve_on_path_and_missing_tools_are_usage_errors() {
        let echo = resolve_tool("echo");
        assert!(echo.is_ok(), "{echo:?}");
        assert_eq!(
            resolve_tool("/no/such/tool").err().map(|f| f.code()),
            Some(2)
        );
        assert_eq!(
            resolve_tool("no-such-tool-xyz").err().map(|f| f.code()),
            Some(2)
        );
    }
}
