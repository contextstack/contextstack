//! Bootstrap the installer lock on hosts that do not yet have Python.
use rustix::fs::{FlockOperation, flock};
use rustix::io::{FdFlags, fcntl_setfd};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Command};

pub fn run(args: &[OsString]) -> io::Result<()> {
    if args.is_empty() {
        println!("node-install-lock");
        return Ok(());
    }
    let [lock_dir, script, action, rest @ ..] = args else {
        return Err(io::Error::other("expected LOCK_DIR SCRIPT COMMAND"));
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(lock_dir)?;
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(Path::new(lock_dir).join("install.lock"))?;
    let fd = lock.as_raw_fd();
    // The file stays open across exec so there is no gap at the shell/Python handoff.
    if let Err(error) = flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        let error = io::Error::from(error);
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }
        let mut holder = String::new();
        lock.read_to_string(&mut holder)?;
        let holder = holder.trim();
        return Err(io::Error::other(if holder.is_empty() {
            "install lock held by another node-install invocation".into()
        } else {
            format!("install lock held by {holder}")
        }));
    }
    lock.set_len(0)?;
    writeln!(
        lock,
        "pid {} (node-install {})",
        process::id(),
        action.to_string_lossy()
    )?;
    fcntl_setfd(&lock, FdFlags::empty())?;
    Err(Command::new("/bin/sh")
        .arg(script)
        .arg(action)
        .args(rest)
        .env("NODE_INSTALL_LOCK_FD", fd.to_string())
        .exec())
}
