//! The Linux seccomp denylist as data, so it is reviewable and tested on any OS; `linux.rs`
//! compiles it to BPF with `seccompiler` and hands it to bwrap.
//!
//! Adapted from the Codex CLI's `codex-rs/linux-sandbox/src/seccomp.rs` (`OpenAI`, Apache-2.0),
//! vendored unchanged as `vendor/codex/linux_sandbox_seccomp.rs.txt`. Its three network modes map onto ours: `none` is
//! Codex's `Restricted` (no socket but `AF_UNIX`, no connect, bind, send...), `gateway` keeps
//! `AF_UNIX` connect for the one bind-mounted socket and refuses every other socket family, and
//! `all` is Codex's `VmSocketRestricted` (no `AF_VSOCK`). Unlike Codex, ptrace and
//! `process_vm_*` stay denied in every mode, since no policy here grants full-disk access. A
//! matching call fails with `EPERM`; everything else is allowed. The namespaces (bwrap's
//! `--unshare-net`, `--disable-userns`) are the first layer; this list is the second.

use crate::policy::Net;

/// Which `socket`/`socketpair` domains a conditional rule matches (argument 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    /// Every call, unconditionally.
    Any,
    /// Every domain except `AF_UNIX`.
    NotUnix,
    /// `AF_VSOCK` only.
    Vsock,
}

/// One denied syscall, by its Linux name, with the domain condition that makes it match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deny {
    /// The syscall name as in `<asm/unistd.h>` (`libc::SYS_<name>`).
    pub syscall: &'static str,
    /// When the call matches.
    pub domain: Domain,
}

const fn any(syscall: &'static str) -> Deny {
    Deny {
        syscall,
        domain: Domain::Any,
    }
}

/// Denied in every mode: tracing other processes, and `io_uring` (which can open sockets
/// without a `socket()` call, so it would bypass the socket rules).
pub const ALWAYS: [Deny; 6] = [
    any("ptrace"),
    any("process_vm_readv"),
    any("process_vm_writev"),
    any("io_uring_setup"),
    any("io_uring_enter"),
    any("io_uring_register"),
];

/// `net: none`: Codex's `Restricted` list. `recvfrom` stays allowed (socketpair-based
/// subprocess plumbing needs it), as in Codex.
pub const NONE: [Deny; 13] = [
    any("connect"),
    any("accept"),
    any("accept4"),
    any("bind"),
    any("listen"),
    any("getpeername"),
    any("getsockname"),
    any("shutdown"),
    any("sendto"),
    any("sendmmsg"),
    any("recvmmsg"),
    any("getsockopt"),
    any("setsockopt"),
];

/// The socket-creating calls, restricted per mode.
const SOCKET_CALLS: [&str; 2] = ["socket", "socketpair"];

/// The full denylist for one network mode.
pub fn rules(net: Net) -> Vec<Deny> {
    let mut list = ALWAYS.to_vec();
    let socket_domain = match net {
        Net::None => {
            list.extend(NONE);
            Domain::NotUnix
        }
        Net::Gateway => Domain::NotUnix,
        Net::All => Domain::Vsock,
    };
    list.extend(SOCKET_CALLS.iter().map(|&syscall| Deny {
        syscall,
        domain: socket_domain,
    }));
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(net: Net) -> Vec<&'static str> {
        rules(net).iter().map(|deny| deny.syscall).collect()
    }

    #[test]
    fn every_mode_denies_tracing_and_io_uring() {
        for net in [Net::None, Net::Gateway, Net::All] {
            let list = names(net);
            for syscall in ["ptrace", "process_vm_readv", "io_uring_setup"] {
                assert!(list.contains(&syscall), "{net:?} {syscall}");
            }
        }
    }

    #[test]
    fn none_denies_connect_and_non_unix_sockets() {
        let list = rules(Net::None);
        assert!(list.contains(&any("connect")));
        assert!(list.contains(&any("sendto")));
        assert!(!list.iter().any(|deny| deny.syscall == "recvfrom"));
        let socket = list.iter().find(|deny| deny.syscall == "socket");
        assert_eq!(socket.map(|deny| deny.domain), Some(Domain::NotUnix));
    }

    #[test]
    fn gateway_keeps_unix_connect_and_all_only_drops_vsock() {
        let gateway = rules(Net::Gateway);
        assert!(!gateway.iter().any(|deny| deny.syscall == "connect"));
        let socket = gateway.iter().find(|deny| deny.syscall == "socket");
        assert_eq!(socket.map(|deny| deny.domain), Some(Domain::NotUnix));
        let all = rules(Net::All);
        assert_eq!(all.len(), ALWAYS.len() + 2);
        assert!(
            all.iter()
                .filter(|deny| SOCKET_CALLS.contains(&deny.syscall))
                .all(|deny| deny.domain == Domain::Vsock)
        );
    }

    #[test]
    fn no_syscall_is_listed_twice() {
        for net in [Net::None, Net::Gateway, Net::All] {
            let mut list = names(net);
            let len = list.len();
            list.sort_unstable();
            list.dedup();
            assert_eq!(list.len(), len, "{net:?}");
        }
    }
}
