//! The Linux backend (phase 3b): bwrap with an empty root built from the mount plan, plus the
//! seccomp denylist compiled with `seccompiler` and handed to bwrap on an inherited memfd
//! (`--seccomp FD`; bwrap sets `PR_SET_NO_NEW_PRIVS` and installs it just before exec'ing the
//! tool). bwrap's `--json-status-fd` tells a sandbox that never started (exit 101) from a tool
//! that exited non-zero. No `unsafe`: the fds are inherited through rustix's safe fd calls.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use rustix::fs::{MemfdFlags, memfd_create};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::pipe::{PipeFlags, pipe_with};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use crate::bwrap;
use crate::failure::{Failure, unenforceable};
use crate::policy::Net;
use crate::seccomp::{self, Domain};

/// Where bwrap is looked for, in order. Fixed paths, not `PATH`: Ubuntu's `AppArmor` profile that
/// lets bwrap create a user namespace is attached to `/usr/bin/bwrap` by path.
pub const BWRAP_CANDIDATES: [&str; 2] = ["/usr/bin/bwrap", "/usr/local/bin/bwrap"];
/// Ubuntu's switch restricting unprivileged user namespaces to AppArmor-profiled programs.
const APPARMOR_USERNS: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";

/// The bwrap binary, or None when it is not installed.
pub fn bwrap_path() -> Option<PathBuf> {
    BWRAP_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn syscall_number(name: &str) -> Option<i64> {
    let number = match name {
        "keyctl" => libc::SYS_keyctl,
        "add_key" => libc::SYS_add_key,
        "request_key" => libc::SYS_request_key,
        "bpf" => libc::SYS_bpf,
        "perf_event_open" => libc::SYS_perf_event_open,
        "userfaultfd" => libc::SYS_userfaultfd,
        "pidfd_getfd" => libc::SYS_pidfd_getfd,
        #[cfg(target_arch = "x86_64")]
        "link" => libc::SYS_link,
        "linkat" => libc::SYS_linkat,
        #[cfg(target_arch = "x86_64")]
        "symlink" => libc::SYS_symlink,
        "symlinkat" => libc::SYS_symlinkat,
        "ptrace" => libc::SYS_ptrace,
        "process_vm_readv" => libc::SYS_process_vm_readv,
        "process_vm_writev" => libc::SYS_process_vm_writev,
        "io_uring_setup" => libc::SYS_io_uring_setup,
        "io_uring_enter" => libc::SYS_io_uring_enter,
        "io_uring_register" => libc::SYS_io_uring_register,
        "connect" => libc::SYS_connect,
        "accept" => libc::SYS_accept,
        "accept4" => libc::SYS_accept4,
        "bind" => libc::SYS_bind,
        "listen" => libc::SYS_listen,
        "getpeername" => libc::SYS_getpeername,
        "getsockname" => libc::SYS_getsockname,
        "shutdown" => libc::SYS_shutdown,
        "sendto" => libc::SYS_sendto,
        "sendmmsg" => libc::SYS_sendmmsg,
        "recvmmsg" => libc::SYS_recvmmsg,
        "getsockopt" => libc::SYS_getsockopt,
        "setsockopt" => libc::SYS_setsockopt,
        "socket" => libc::SYS_socket,
        "socketpair" => libc::SYS_socketpair,
        _ => return None,
    };
    Some(number)
}

fn domain_rule(domain: Domain) -> Result<Vec<SeccompRule>, seccompiler::BackendError> {
    let (op, family) = match domain {
        Domain::Any => return Ok(Vec::new()), // empty: the call always matches
        Domain::NotUnix => (SeccompCmpOp::Ne, libc::AF_UNIX),
        Domain::NotIp | Domain::NotIpOrUnix => {
            let mut families = vec![libc::AF_INET, libc::AF_INET6];
            if domain == Domain::NotIpOrUnix {
                families.push(libc::AF_UNIX);
            }
            let conditions = families
                .into_iter()
                .map(|family| {
                    SeccompCondition::new(
                        0,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::Ne,
                        u64::try_from(family).unwrap_or_default(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(vec![SeccompRule::new(conditions)?]);
        }
        Domain::Vsock => (SeccompCmpOp::Eq, libc::AF_VSOCK),
    };
    let family = u64::try_from(family).unwrap_or_default();
    let condition = SeccompCondition::new(0, SeccompCmpArgLen::Dword, op, family)?;
    Ok(vec![SeccompRule::new(vec![condition])?])
}

/// The denylist for `net` as raw BPF bytes (`struct sock_filter` array, native endian), which is
/// what bwrap's `--seccomp` reads.
pub fn bpf(net: Net) -> Result<Vec<u8>, Failure> {
    compile_bpf(&seccomp::rules(net))
}

fn compile_bpf(denylist: &[seccomp::Deny]) -> Result<Vec<u8>, Failure> {
    let failed = |error: &dyn std::fmt::Display| {
        unenforceable(format!(
            "seccomp filter could not be built: {error}; the tool was not run"
        ))
    };
    let arch = if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else {
        return Err(unenforceable(format!(
            "no seccomp filter for {}; the tool was not run",
            std::env::consts::ARCH
        )));
    };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for deny in denylist {
        let number = syscall_number(deny.syscall)
            .ok_or_else(|| failed(&format!("unknown syscall {}", deny.syscall)))?;
        rules.insert(number, domain_rule(deny.domain).map_err(|e| failed(&e))?);
    }
    let errno = u32::try_from(libc::EPERM).unwrap_or(1);
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(errno),
        arch,
    )
    .map_err(|e| failed(&e))?;
    let mut program: BpfProgram = filter.try_into().map_err(|e| failed(&e))?;
    if cfg!(target_arch = "x86_64") {
        use seccompiler::sock_filter;
        // x32 shares the x86_64 audit architecture, but sets bit 30 in the syscall number.
        // Load seccomp_data.nr, skip the errno return for native syscall numbers.
        program.splice(
            ..0,
            [
                sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    k: 0,
                },
                sock_filter {
                    code: 0x35,
                    jt: 0,
                    jf: 1,
                    k: 0x4000_0000,
                },
                sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: SeccompAction::Errno(errno).into(),
                },
            ],
        );
    }
    let mut bytes = Vec::with_capacity(program.len() * 8);
    for instruction in &program {
        bytes.extend_from_slice(&instruction.code.to_ne_bytes());
        bytes.push(instruction.jt);
        bytes.push(instruction.jf);
        bytes.extend_from_slice(&instruction.k.to_ne_bytes());
    }
    Ok(bytes)
}

/// An inheritable memfd holding `bytes`, rewound: bwrap reads the program from its offset.
fn seccomp_fd(bytes: &[u8]) -> Result<File, Failure> {
    let cannot = |error: &dyn std::fmt::Display| {
        unenforceable(format!(
            "cannot hand the seccomp filter to bwrap: {error}; the tool was not run"
        ))
    };
    // No MFD_CLOEXEC: bwrap must inherit it.
    let fd: OwnedFd =
        memfd_create("cs-sandbox-seccomp", MemfdFlags::empty()).map_err(|e| cannot(&e))?;
    let mut file = File::from(fd);
    file.write_all(bytes).map_err(|e| cannot(&e))?;
    file.seek(SeekFrom::Start(0)).map_err(|e| cannot(&e))?;
    Ok(file)
}

/// A status pipe: the read end stays close-on-exec, the write end is inherited by bwrap.
fn status_pipe() -> Result<(File, OwnedFd), Failure> {
    let cannot = |error: &dyn std::fmt::Display| {
        unenforceable(format!(
            "cannot open bwrap's status pipe: {error}; the tool was not run"
        ))
    };
    let (reader, writer) = pipe_with(PipeFlags::CLOEXEC).map_err(|e| cannot(&e))?;
    fcntl_setfd(&writer, FdFlags::empty()).map_err(|e| cannot(&e))?;
    Ok((File::from(reader), writer))
}

/// What one bwrap launch returns to the shared report code.
pub struct Ran {
    /// bwrap's exit status (the tool's code, or 128+n when a signal killed it).
    pub status: ExitStatus,
    /// bwrap's pid, for the report file name.
    pub pid: u32,
    /// SHA-256 input: the argv words and the BPF program, so a report identifies both.
    pub identity: Vec<u8>,
}

/// One call: what bwrap's argv needs besides the plan and the fds.
pub struct Call<'a> {
    /// The policy's network capability.
    pub net: Net,
    /// For `net: gateway`, (canonical socket, `CS_GATEWAY_SOCKET` as the tool sees it).
    pub gateway: Option<(&'a Path, &'a Path)>,
    /// Granted named sockets, each (canonical socket, path the tool sees).
    pub sockets: &'a [(PathBuf, PathBuf)],
    /// The canonical workspace.
    pub workspace: &'a Path,
    /// The resolved tool.
    pub tool: &'a Path,
    /// The tool's arguments.
    pub args: &'a [String],
    /// Variables the wrapper adds to the tool's environment (TMPDIR and friends).
    pub env: &'a [(&'a str, &'a std::ffi::OsStr)],
}

/// The seccomp mode: `none` with a granted named socket needs `connect` on unix sockets, which is
/// the gateway list (no socket but `AF_UNIX`); the network namespace still holds no other endpoint.
fn seccomp_net(net: Net, sockets: bool) -> Net {
    if net == Net::None && sockets {
        Net::Gateway
    } else {
        net
    }
}

/// Run the tool under bwrap.
pub fn run(bwrap: &Path, plan: &bwrap::Plan, call: &Call<'_>) -> Result<Ran, Failure> {
    let program = bpf(seccomp_net(call.net, !call.sockets.is_empty()))?;
    let filter = seccomp_fd(&program)?;
    let (mut status_reader, status_writer) = status_pipe()?;
    let launch = bwrap::Launch {
        net: call.net,
        gateway: call.gateway,
        sockets: call.sockets,
        workspace: call.workspace,
        seccomp_fd: filter.as_raw_fd(),
        status_fd: status_writer.as_raw_fd(),
        tool: call.tool,
        args: call.args,
    };
    let argv: Vec<OsString> = bwrap::argv(plan, &launch)?;
    let mut identity = Vec::new();
    for word in &argv {
        identity.extend_from_slice(word.as_encoded_bytes());
        identity.push(0);
    }
    identity.extend_from_slice(&program);
    let mut command = Command::new(bwrap);
    cs_child_signals::reset_on_exec(&mut command);
    command.args(&argv).current_dir(call.workspace);
    for (name, value) in call.env {
        command.env(name, value);
    }
    let mut child = command.spawn().map_err(|error| {
        unenforceable(format!(
            "cannot start {}: {error}; the tool was not run",
            bwrap.display()
        ))
    })?;
    // Only bwrap may hold the write end, so EOF on the read end means bwrap is gone.
    drop(status_writer);
    drop(filter);
    let (status, report) =
        crate::json_status::wait_with_status_drain(&mut child, &mut status_reader)?;
    if !bwrap::started(&report) {
        return Err(unenforceable(format!(
            "bwrap could not build the sandbox (exit {}); the tool was not run",
            status
                .code()
                .map_or_else(|| "signal".to_owned(), |c| c.to_string())
        )));
    }
    Ok(Ran {
        status,
        pid: child.id(),
        identity,
    })
}

/// Launch an executor with an empty root, projected mounts and private IP loopback.
pub fn run_executor(
    request: &crate::cli_executor::ExecutorRequest,
    arguments: &[String],
) -> Result<ExitStatus, Failure> {
    let path = bwrap_path()
        .ok_or_else(|| unenforceable("bwrap is required for Linux executor confinement"))?;
    if !userns_works(&path) {
        return Err(unenforceable(
            "bwrap cannot create an unprivileged user namespace; executor refused",
        ));
    }
    let (mut plan, deny_symlinks) = crate::executor_mounts::plan(request)?;
    let mut sockets = crate::executor_mounts::named_sockets(request)?;
    let ports = crate::linux_relay::checked_ports(request)?;
    let requires_landlock = !sockets.is_empty() || !ports.is_empty();
    let use_landlock = (requires_landlock || request.check_scratch.is_some())
        && cs_landlock::supports_named_sockets();
    if requires_landlock && !use_landlock {
        return Err(unenforceable(
            "named Unix sockets require Linux Landlock ABI 9; executor refused",
        ));
    }
    let relay = crate::linux_relay::HostRelay::start(&ports)
        .map_err(|e| unenforceable(format!("cannot start host loopback relay: {e}")))?;
    sockets.extend(relay.endpoints.iter().map(|(_, path)| path.clone()));
    let mut command = arguments.to_vec();
    if use_landlock {
        let source = std::env::current_exe().map_err(|e| unenforceable(e.to_string()))?;
        let target = PathBuf::from("/.cs-executor-launch");
        command.insert(0, "--".to_owned());
        let socket_json =
            serde_json::to_string(&sockets).map_err(|e| unenforceable(e.to_string()))?;
        let relay_json =
            serde_json::to_string(&relay.endpoints).map_err(|e| unenforceable(e.to_string()))?;
        command.splice(
            0..0,
            [
                target.display().to_string(),
                "--exec-with-sockets".to_owned(),
                socket_json,
                relay_json,
            ],
        );
        plan.mounts.push(bwrap::Mount::RoAt { source, target });
    }
    let (command, args) = command
        .split_first()
        .ok_or_else(|| unenforceable("executor command is missing"))?;
    let filter = seccomp_fd(&compile_bpf(&seccomp::executor_rules(
        use_landlock,
        deny_symlinks,
    ))?)?;
    let sockets: Vec<_> = sockets
        .into_iter()
        .map(|path| (path.clone(), path))
        .collect();
    let (mut status_reader, status_writer) = status_pipe()?;
    let launch = bwrap::Launch {
        net: Net::Loopback,
        gateway: None,
        sockets: &sockets,
        workspace: &request.workspace,
        seccomp_fd: filter.as_raw_fd(),
        status_fd: status_writer.as_raw_fd(),
        tool: Path::new(command),
        args,
    };
    let mut process = Command::new(&path);
    cs_child_signals::reset_on_exec(&mut process);
    let mut mask_files = Vec::new();
    let bwrap_args = bwrap::executor_argv(&plan, &launch, || {
        // bwrap closes each data descriptor after copying it.
        let fd = memfd_create("cs-sandbox-empty-mask", MemfdFlags::empty())
            .map_err(|e| unenforceable(format!("cannot create empty executor file mask: {e}")))?;
        let raw_fd = fd.as_raw_fd();
        mask_files.push(fd);
        Ok(raw_fd)
    })?;
    let mut child = process
        .args(bwrap_args)
        .spawn()
        .map_err(|e| unenforceable(format!("cannot start bwrap: {e}")))?;
    drop(mask_files);
    drop(status_writer);
    drop(filter);
    let (status, report) =
        crate::json_status::wait_with_status_drain(&mut child, &mut status_reader)?;
    if !bwrap::started(&report) {
        return Err(unenforceable(format!(
            "bwrap could not build the executor sandbox ({status})"
        )));
    }
    Ok(status)
}

/// Apply the socket rules after bwrap has built the mount namespace.
pub fn exec_with_sockets(args: &[String]) -> Result<i32, Failure> {
    use std::os::unix::process::CommandExt;
    let [socket_json, relay_json, separator, binary, rest @ ..] = args else {
        return Err(unenforceable("invalid executor socket bootstrap arguments"));
    };
    if separator != "--" {
        return Err(unenforceable("invalid executor socket bootstrap separator"));
    }
    let sockets: Vec<PathBuf> = serde_json::from_str(socket_json)
        .map_err(|e| unenforceable(format!("invalid executor sockets: {e}")))?;
    let endpoints: Vec<(u16, PathBuf)> = serde_json::from_str(relay_json)
        .map_err(|e| unenforceable(format!("invalid executor relay endpoints: {e}")))?;
    cs_landlock::restrict_unix_sockets(&sockets)
        .map_err(|e| unenforceable(format!("cannot restrict Unix sockets with Landlock: {e}")))?;
    for (port, socket) in endpoints {
        crate::linux_relay::start_sandbox(port, &socket).map_err(|e| {
            unenforceable(format!("cannot forward executor loopback port {port}: {e}"))
        })?;
    }
    let error = Command::new(binary).args(rest).exec();
    Err(unenforceable(format!(
        "cannot execute confined command: {error}"
    )))
}

/// Whether this host lets bwrap create an unprivileged user namespace: the trivial sandbox
/// `bwrap --unshare-user --ro-bind / / true`, run for real.
pub fn userns_works(bwrap: &Path) -> bool {
    let mut command = Command::new(bwrap);
    cs_child_signals::reset_on_exec(&mut command);
    command
        .args(["--unshare-user", "--ro-bind", "/", "/", "/bin/true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `kernel.apparmor_restrict_unprivileged_userns` (Ubuntu), or None where the knob is absent.
pub fn apparmor_userns_restriction() -> Option<i64> {
    std::fs::read_to_string(APPARMOR_USERNS)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_socket_lifts_none_to_the_unix_only_list() {
        assert_eq!(seccomp_net(Net::None, true), Net::Gateway);
        assert_eq!(seccomp_net(Net::None, false), Net::None);
        assert_eq!(seccomp_net(Net::All, true), Net::All);
    }

    #[test]
    fn every_listed_syscall_has_a_number_and_every_mode_compiles() -> Result<(), String> {
        for net in [Net::None, Net::Gateway, Net::Loopback, Net::All] {
            for deny in seccomp::rules(net) {
                assert!(syscall_number(deny.syscall).is_some(), "{}", deny.syscall);
            }
            let bytes = bpf(net).map_err(|failure| failure.message().to_owned())?;
            assert!(!bytes.is_empty() && bytes.len() % 8 == 0);
        }
        Ok(())
    }
}
