//! Workspace confinement for Rust tools. The workspace is the canonical working directory.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub const WORKSPACE_OPEN_DENIED: &str = "path escapes workspace or changed before open";

/// Reject parent components, and admit absolute paths only under an allowed canonical root.
pub fn lexically_escapes(p: &str, repo: Option<&Path>) -> bool {
    if p.split('/').any(|part| part == "..") {
        return true;
    }
    let path = Path::new(p);
    if !path.is_absolute() {
        return false;
    }
    let Ok(workspace) = env::current_dir().and_then(fs::canonicalize) else {
        return true;
    };
    for ancestor in path.ancestors() {
        match fs::canonicalize(ancestor) {
            Ok(target) => {
                return !target.starts_with(&workspace)
                    && !repo.is_some_and(|root| target.starts_with(root));
            }
            // New files may have missing parents. A dangling link is an existing
            // entry with an unknown destination and must not be skipped.
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && fs::symlink_metadata(ancestor)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) => {}
            Err(_) => return true,
        }
    }
    true
}

/// The sandbox launcher may expose a second tree for reads.
pub fn repo_view() -> Option<std::path::PathBuf> {
    env::var_os("CS_REPO_VIEW").and_then(|path| fs::canonicalize(path).ok())
}

pub fn read_resolves_outside(path: &Path, repo: Option<&Path>) -> bool {
    resolves_outside(path)
        && !repo.is_some_and(|root| fs::canonicalize(path).is_ok_and(|p| p.starts_with(root)))
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
    let repo = (!writable).then(repo_view).flatten();
    open_resolved_after(
        path,
        writable,
        |p| p.starts_with(&workspace) || repo.as_ref().is_some_and(|root| p.starts_with(root)),
        after_check,
    )
}

fn open_resolved_after(
    path: &Path,
    writable: bool,
    allowed: impl Fn(&Path) -> bool,
    after_check: impl FnOnce(),
) -> io::Result<File> {
    let target = fs::canonicalize(path)?;
    let denied = || io::Error::new(io::ErrorKind::PermissionDenied, WORKSPACE_OPEN_DENIED);
    if !allowed(&target) {
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
    if !allowed(&checked)
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

/// Open a regular file confined to the canonical repository snapshot root.
pub fn open_in_repo(path: &Path, root: &Path) -> io::Result<File> {
    open_resolved_after(path, false, |p| p == path && p.starts_with(root), || {})
}

#[cfg(test)]
static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct CwdRestore {
    prev: std::path::PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl CwdRestore {
    pub(crate) fn chdir(path: &Path) -> Self {
        let lock = CWD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = env::current_dir().unwrap_or_else(|e| panic!("cwd: {e}"));
        env::set_current_dir(path).unwrap_or_else(|e| panic!("chdir: {e}"));
        Self { prev, _lock: lock }
    }
}

#[cfg(test)]
impl Drop for CwdRestore {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.prev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::os::unix::fs::symlink;

    #[test]
    fn final_component_swap_is_refused_before_read_or_truncate() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("{e}"));
        let _cwd = CwdRestore::chdir(dir.path());
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
    }

    #[test]
    fn lexical_rule_matches_python() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let _cwd = CwdRestore::chdir(dir.path());
        assert!(lexically_escapes("/etc/passwd", None));
        assert!(lexically_escapes("../x", None));
        assert!(lexically_escapes("a/../b", None));
        assert!(!lexically_escapes("a/..b", None));
        assert!(!lexically_escapes("a.txt", None));
        assert!(!lexically_escapes("", None));
    }

    #[test]
    fn missing_paths_are_not_refused_here() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let missing = dir.path().join("missing");
        assert!(!resolves_outside(&missing));
        assert!(!link_leaves_workspace(&missing));
    }

    #[test]
    fn resolves_outside_refuses_everything_when_workspace_is_gone() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let gone = dir.path().join("inner");
        std::fs::create_dir(&gone).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let _cwd = CwdRestore::chdir(&gone);
        assert!(resolves_outside(Path::new("/tmp")));
    }

    #[test]
    fn open_for_read_follows_in_workspace_symlink() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"inside\n").unwrap_or_else(|e| panic!("write: {e}"));
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap_or_else(|e| panic!("symlink: {e}"));
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        let _cwd = CwdRestore::chdir(&root);
        let mut file = open_for_read(Path::new("link.txt")).unwrap_or_else(|e| panic!("read: {e}"));
        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .unwrap_or_else(|e| panic!("read body: {e}"));
        assert_eq!(buf, "inside\n");
    }

    #[test]
    fn open_for_read_uses_nofollow_on_regular_files() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, b"x").unwrap_or_else(|e| panic!("write: {e}"));
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        let _cwd = CwdRestore::chdir(&root);
        open_for_read(Path::new("plain.txt")).unwrap_or_else(|e| panic!("open: {e}"));
    }
}
