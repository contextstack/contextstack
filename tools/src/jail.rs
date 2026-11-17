//! Workspace confinement. The Python tools apply only the lexical rule; the port also resolves
//! symlinks and refuses any existing path that lands outside the workspace (the cwd).

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// The Python tools' rule: an absolute path or a `..` component escapes.
pub fn lexically_escapes(p: &str) -> bool {
    p.starts_with('/') || p.split('/').any(|part| part == "..")
}

/// True when `p` exists and, with every symlink resolved, lies outside the resolved workspace.
/// A path that does not resolve is left to the tool's own not-found handling, as in the Python.
/// A workspace that cannot itself be resolved refuses everything.
pub fn resolves_outside(p: &Path) -> bool {
    let Ok(target) = fs::canonicalize(p) else {
        return false;
    };
    match env::current_dir().and_then(fs::canonicalize) {
        Ok(workspace) => !target.starts_with(&workspace),
        Err(_) => true,
    }
}

/// True when `p` is a symlink that is dangling or resolves outside the workspace.
pub fn link_leaves_workspace(p: &Path) -> bool {
    match fs::symlink_metadata(p) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(p).is_err() || resolves_outside(p)
        }
        _ => false,
    }
}

/// Resolve documented in-workspace links, then open the resolved final component with
/// `O_NOFOLLOW`. Match the descriptor to the checked regular file before any I/O.
fn open_checked(path: &Path, writable: bool) -> io::Result<File> {
    open_checked_after(path, writable, || {})
}

fn open_checked_after(path: &Path, writable: bool, after_check: impl FnOnce()) -> io::Result<File> {
    let workspace = fs::canonicalize(env::current_dir()?)?;
    let target = fs::canonicalize(path)?;
    let denied = || {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path escapes workspace or changed before open",
        )
    };
    if !target.starts_with(&workspace) {
        return Err(denied());
    }
    let expected = fs::symlink_metadata(&target)?;
    if !expected.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    after_check();
    let file = OpenOptions::new()
        .read(!writable)
        .write(writable)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&target)?;
    let opened = file.metadata()?;
    let checked = fs::canonicalize(path)?;
    let current = fs::symlink_metadata(&checked)?;
    if !checked.starts_with(&workspace)
        || !opened.is_file()
        || opened.dev() != expected.dev()
        || opened.ino() != expected.ino()
        || opened.dev() != current.dev()
        || opened.ino() != current.ino()
    {
        return Err(denied());
    }
    Ok(file)
}

pub fn open_for_read(path: &Path) -> io::Result<File> {
    open_checked(path, false)
}

/// Does not truncate: the caller must validate identity before changing contents.
pub fn open_for_write(path: &Path) -> io::Result<File> {
    open_checked(path, true)
}

#[cfg(test)]
pub(crate) static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::os::unix::fs::symlink;

    #[test]
    fn final_component_swap_is_refused_before_read_or_truncate() {
        let _lock = CWD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("{e}"));
        let prev = env::current_dir().unwrap_or_else(|e| panic!("{e}"));
        env::set_current_dir(dir.path()).unwrap_or_else(|e| panic!("{e}"));
        for writable in [false, true] {
            fs::write("target", "original").unwrap_or_else(|e| panic!("{e}"));
            fs::write("other", "untouched").unwrap_or_else(|e| panic!("{e}"));
            let result = open_checked_after(Path::new("target"), writable, || {
                fs::remove_file("target").unwrap_or_else(|e| panic!("{e}"));
                symlink("other", "target").unwrap_or_else(|e| panic!("{e}"));
            });
            assert!(result.is_err());
            assert_eq!(
                fs::read_to_string("other").unwrap_or_else(|e| panic!("{e}")),
                "untouched"
            );
            fs::remove_file("target").unwrap_or_else(|e| panic!("{e}"));
        }
        env::set_current_dir(prev).unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn lexical_rule_matches_python() {
        assert!(lexically_escapes("/etc/passwd"));
        assert!(lexically_escapes("../x"));
        assert!(lexically_escapes("a/../b"));
        assert!(!lexically_escapes("a/..b"));
        assert!(!lexically_escapes("a.txt"));
        assert!(!lexically_escapes(""));
    }

    #[test]
    fn missing_paths_are_not_refused_here() {
        assert!(!resolves_outside(Path::new("no-such-file-cs-tools-test")));
        assert!(!link_leaves_workspace(Path::new(
            "no-such-file-cs-tools-test"
        )));
    }

    #[test]
    fn resolves_outside_refuses_everything_when_workspace_is_gone() {
        use super::CWD_TEST_LOCK;
        let _lock = CWD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let gone = dir.path().join("inner");
        std::fs::create_dir(&gone).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let prev = env::current_dir().unwrap_or_else(|e| panic!("cwd: {e}"));
        env::set_current_dir(&gone).unwrap_or_else(|e| panic!("chdir: {e}"));
        assert!(resolves_outside(Path::new("/tmp")));
        env::set_current_dir(&prev).unwrap_or_else(|e| panic!("restore cwd: {e}"));
    }

    #[test]
    fn open_for_read_follows_in_workspace_symlink() {
        use super::CWD_TEST_LOCK;
        let _lock = CWD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"inside\n").unwrap_or_else(|e| panic!("write: {e}"));
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap_or_else(|e| panic!("symlink: {e}"));
        let prev = env::current_dir().unwrap_or_else(|e| panic!("cwd: {e}"));
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        env::set_current_dir(&root).unwrap_or_else(|e| panic!("chdir: {e}"));
        let mut file = open_for_read(Path::new("link.txt")).unwrap_or_else(|e| panic!("read: {e}"));
        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .unwrap_or_else(|e| panic!("read body: {e}"));
        let _ = env::set_current_dir(&prev);
        assert_eq!(buf, "inside\n");
    }

    #[test]
    fn open_for_read_uses_nofollow_on_regular_files() {
        use super::CWD_TEST_LOCK;
        let _lock = CWD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, b"x").unwrap_or_else(|e| panic!("write: {e}"));
        let prev = env::current_dir().unwrap_or_else(|e| panic!("cwd: {e}"));
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        env::set_current_dir(&root).unwrap_or_else(|e| panic!("chdir: {e}"));
        open_for_read(Path::new("plain.txt")).unwrap_or_else(|e| panic!("open: {e}"));
        let _ = env::set_current_dir(&prev);
    }
}
