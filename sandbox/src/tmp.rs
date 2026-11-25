//! The call's private tmp directory: created fresh outside the workspace, removed after the tool.
//! A wrapper killed by its runner (`SIGKILL`, a timed-out call) never runs `Drop`, so each start first
//! sweeps `cs-sandbox-<pid>-<nanos>` dirs whose owning pid is dead (`sweep_orphans`).

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::failure::{Failure, unenforceable};

/// `<parent>/cs-sandbox-<pid>-<nanos>`, mode 0700, canonical (on macOS `/var` is `/private/var`).
/// Dropping it removes the tree; a failed removal is one stderr line, never a changed exit.
#[derive(Debug)]
pub struct CallTmp {
    path: PathBuf,
}

/// The name prefix of every call tmp dir this tool creates.
const PREFIX: &str = "cs-sandbox-";

/// The owning pid when `name` is exactly `cs-sandbox-<pid>-<nanos>` (both decimal), the shape
/// `CallTmp::create` writes; anything else is not ours and is left alone.
pub fn owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(PREFIX)?;
    let (pid, nanos) = rest.split_once('-')?;
    let decimal = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !decimal(pid) || !decimal(nanos) {
        return None;
    }
    pid.parse().ok().filter(|pid| *pid > 0)
}

/// Whether a process with this pid exists (`kill(pid, 0)`: `ESRCH` means gone; `EPERM` means it
/// exists under another user, so it counts as alive).
fn pid_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    !matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

/// Remove call tmp dirs under `parent` left by a wrapper that died without cleaning up. A dir is
/// removed only when its name is this tool's shape (`owner_pid`), it is a real directory (not a
/// symlink) owned by the current user, and `alive` says its pid is gone. Returns the removed
/// paths. Best effort: an unreadable parent or a failed removal is skipped, never a failed call.
pub fn sweep_orphans_with(parent: &Path, alive: &dyn Fn(u32) -> bool) -> Vec<PathBuf> {
    let me = std::process::id();
    let uid = rustix::process::getuid().as_raw();
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(owner_pid) else {
            continue;
        };
        if pid == me || alive(pid) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_dir() || meta.uid() != uid {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

/// `sweep_orphans_with` using the real pid liveness check.
pub fn sweep_orphans(parent: &Path) -> Vec<PathBuf> {
    sweep_orphans_with(parent, &pid_alive)
}

impl CallTmp {
    /// Create the directory under `parent` (the caller passes `std::env::temp_dir()`).
    pub fn create(parent: &Path) -> Result<Self, Failure> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("{PREFIX}{}-{nanos}", std::process::id());
        let given = parent.join(name);
        let cannot = |error: std::io::Error| {
            unenforceable(format!(
                "cannot create tmp dir {}: {error}",
                given.display()
            ))
        };
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&given)
            .map_err(cannot)?;
        // The umask may narrow the mode further; set it exactly.
        std::fs::set_permissions(&given, std::fs::Permissions::from_mode(0o700)).map_err(cannot)?;
        match given.canonicalize() {
            Ok(path) => Ok(Self { path }),
            Err(error) => {
                let _ = std::fs::remove_dir(&given);
                Err(cannot(error))
            }
        }
    }

    /// The canonical path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CallTmp {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            eprintln!(
                "cs-sandbox: tmp dir not removed: {}: {error}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_private_canonical_and_removed_on_drop() -> Result<(), String> {
        let parent = tempfile::tempdir().map_err(|error| error.to_string())?;
        let tmp = CallTmp::create(parent.path()).map_err(|f| f.message().to_owned())?;
        let path = tmp.path().to_path_buf();
        assert!(path.is_dir());
        assert_eq!(
            path.canonicalize().map_err(|error| error.to_string())?,
            path
        );
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let pid_part = format!("cs-sandbox-{}-", std::process::id());
        assert!(name.starts_with(&pid_part), "{name}");
        let mode = std::fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::create_dir(path.join("nested")).map_err(|error| error.to_string())?;
        std::fs::write(path.join("nested/f"), "x").map_err(|error| error.to_string())?;
        drop(tmp);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn owner_pid_only_parses_our_exact_shape() {
        assert_eq!(owner_pid("cs-sandbox-42-1700000000"), Some(42));
        for other in [
            "cs-sandbox-",
            "cs-sandbox-42",
            "cs-sandbox-42-",
            "cs-sandbox--1",
            "cs-sandbox-0-1",
            "cs-sandbox-x-1",
            "cs-sandbox-42-1-2",
            "cs-sandbox-42-1x",
            "other-42-1",
            "cs-sandboxer-42-1",
        ] {
            assert_eq!(owner_pid(other), None, "{other}");
        }
    }

    #[test]
    fn sweep_removes_only_dead_owned_call_dirs() -> Result<(), String> {
        let parent = tempfile::tempdir().map_err(|error| error.to_string())?;
        let p = parent.path();
        let dead = p.join("cs-sandbox-999001-1");
        let live = p.join("cs-sandbox-999002-1");
        let mine = p.join(format!("cs-sandbox-{}-1", std::process::id()));
        let foreign = p.join("cs-sandbox-999001-x");
        let file = p.join("cs-sandbox-999003-1");
        let target = p.join("keep");
        let link = p.join("cs-sandbox-999004-1");
        for dir in [&dead, &live, &mine, &foreign, &target] {
            std::fs::create_dir(dir).map_err(|error| error.to_string())?;
        }
        std::fs::write(dead.join("leak"), "x").map_err(|error| error.to_string())?;
        std::fs::write(&file, "x").map_err(|error| error.to_string())?;
        std::fs::write(target.join("f"), "x").map_err(|error| error.to_string())?;
        std::os::unix::fs::symlink(&target, &link).map_err(|error| error.to_string())?;
        let removed = sweep_orphans_with(p, &|pid| pid == 999_002);
        assert_eq!(removed, vec![dead.clone()]);
        assert!(!dead.exists());
        for kept in [&live, &mine, &foreign, &file, &link, &target.join("f")] {
            assert!(kept.exists(), "{}", kept.display());
        }
        Ok(())
    }

    #[test]
    fn a_killed_wrappers_dir_is_swept_by_the_next_start() -> Result<(), String> {
        let parent = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .map_err(|error| error.to_string())?;
        let pid = child.id();
        child.wait().map_err(|error| error.to_string())?;
        assert!(!pid_alive(pid));
        assert!(pid_alive(std::process::id()));
        let orphan = parent.path().join(format!("cs-sandbox-{pid}-123"));
        std::fs::create_dir(&orphan).map_err(|error| error.to_string())?;
        assert_eq!(sweep_orphans(parent.path()), vec![orphan.clone()]);
        assert!(!orphan.exists());
        Ok(())
    }

    #[test]
    fn missing_parent_is_unenforceable() {
        let code = CallTmp::create(Path::new("/no/such/parent"))
            .err()
            .map(|f| f.code());
        assert_eq!(code, Some(101));
    }
}
