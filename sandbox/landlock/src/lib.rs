//! Minimal Landlock ABI for allowing named Unix sockets on Linux.
#![cfg(target_os = "linux")]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MAKE_SOCK: u64 = 1 << 9;
const REFER: u64 = 1 << 13;
const RESOLVE_UNIX: u64 = 1 << 16;
const ABSTRACT_UNIX: u64 = 1;

#[repr(C, packed)]
struct PathRule {
    allowed_access: u64,
    parent_fd: libc::c_int,
}

/// Report whether this kernel can scope pathname Unix sockets.
#[allow(unsafe_code)]
#[must_use]
pub fn supports_named_sockets() -> bool {
    // SAFETY: A version query passes no pointer or buffer to the kernel.
    unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, 0, 0, 1) >= 9 }
}

fn errno_on_failure(result: libc::c_long) -> io::Result<libc::c_long> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

#[allow(unsafe_code)]
fn allow(ruleset: &File, path: &Path, access: u64) -> io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(path)?;
    let rule = PathRule {
        allowed_access: access,
        parent_fd: file.as_raw_fd(),
    };
    // SAFETY: The packed UAPI structure is initialized and valid for the synchronous call.
    errno_on_failure(unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            1,
            &raw const rule,
            0,
        )
    })?;
    Ok(())
}

/// Scope Unix socket connections to this domain and the listed socket inodes.
/// Block pathname socket creation throughout the sandbox.
///
/// # Errors
/// Returns the OS error when creating or enforcing the ruleset fails.
#[allow(unsafe_code)]
pub fn restrict_unix_sockets(paths: &[impl AsRef<Path>]) -> io::Result<()> {
    let attributes = [REFER | RESOLVE_UNIX | MAKE_SOCK, 0, ABSTRACT_UNIX];
    // SAFETY: The three UAPI fields are initialized and live through the call.
    let fd = errno_on_failure(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            attributes.as_ptr(),
            size_of_val(&attributes),
            0,
        )
    })?;
    let fd = i32::try_from(fd).map_err(io::Error::other)?;
    // SAFETY: A successful create_ruleset returns a new owned fd.
    let ruleset = unsafe { File::from_raw_fd(fd) };
    // Rulesets implicitly restrict reparenting. Leave those operations to the mount policy.
    allow(&ruleset, Path::new("/"), REFER)?;
    for path in paths {
        allow(&ruleset, path.as_ref(), RESOLVE_UNIX)?;
    }
    // SAFETY: The fd remains open. This is called by the single-threaded sandbox bootstrap.
    errno_on_failure(unsafe {
        libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0)
    })?;
    Ok(())
}
