//! The call's private tmp directory: created fresh outside the workspace, removed after the tool.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::failure::{Failure, unenforceable};

/// `<parent>/cs-sandbox-<pid>-<nanos>`, mode 0700, canonical (on macOS `/var` is `/private/var`).
/// Dropping it removes the tree; a failed removal is one stderr line, never a changed exit.
#[derive(Debug)]
pub struct CallTmp {
    path: PathBuf,
}

impl CallTmp {
    /// Create the directory under `parent` (the caller passes `std::env::temp_dir()`).
    pub fn create(parent: &Path) -> Result<Self, Failure> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("cs-sandbox-{}-{nanos}", std::process::id());
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
    fn missing_parent_is_unenforceable() {
        let code = CallTmp::create(Path::new("/no/such/parent"))
            .err()
            .map(|f| f.code());
        assert_eq!(code, Some(101));
    }
}
