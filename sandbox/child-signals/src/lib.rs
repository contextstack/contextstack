//! Signal setup shared by the Unix child launchers.

use std::{io, mem::MaybeUninit, os::unix::process::CommandExt, process::Command};

/// Restore default dispositions and unblock signals in the child before exec.
#[allow(unsafe_code)]
pub fn reset_on_exec(command: &mut Command) {
    // SAFETY: The callback runs only in the child and calls async-signal-safe
    // POSIX functions. It uses stack storage and reports errors through errno.
    unsafe {
        command.pre_exec(|| {
            for signal in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGPIPE,
                libc::SIGCHLD,
            ] {
                if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
            }

            let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
            if libc::sigemptyset(mask.as_mut_ptr()) == -1
                || libc::sigprocmask(libc::SIG_SETMASK, mask.as_ptr(), std::ptr::null_mut()) == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
