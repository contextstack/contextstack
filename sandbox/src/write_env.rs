//! `write_env`: a tool may write to the directory an environment variable names, when the host
//! permits that variable and the directory lies under one of its allowed roots (the state root).

use crate::cli_executor::PROTECTED_READS;
use crate::failure::{Failure, unenforceable};
use crate::policy::repo_root;
use std::ffi::OsString;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};

/// The deepest existing ancestor canonicalised, with the missing names appended lexically.
/// The path must be absolute and free of `..`; the caller checks that first.
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut missing: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        missing.push(name.to_owned());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent.to_path_buf();
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    resolved
}

/// Where a write target may not be, and where it must be.
#[derive(Debug)]
pub struct Bounds<'a> {
    /// The canonical workspace: never a `write_env` target.
    pub workspace: &'a Path,
    /// The code root: never a `write_env` target.
    pub code_root: &'a Path,
    /// The owner home directory.
    pub home: &'a Path,
    /// The variable's allowed roots, expanded and absolute.
    pub roots: &'a [PathBuf],
}

#[derive(Debug)]
pub enum GrantError {
    Narrowed(String),
    Unenforceable(Failure),
}

impl From<String> for GrantError {
    fn from(reason: String) -> Self {
        Self::Narrowed(reason)
    }
}

impl Bounds<'_> {
    fn check(&self, var: &str, path: &Path) -> Result<(), GrantError> {
        let refuse = |reason| {
            GrantError::Unenforceable(unenforceable(format!(
                "unenforceable write_env: {var} names {}: {reason}",
                path.display()
            )))
        };
        let home = canonical_prefix(self.home);
        for (root, label) in [(self.workspace, "workspace"), (self.code_root, "code root")] {
            let root = canonical_prefix(root);
            if root.starts_with(path) {
                return Err(refuse(format!("directory equals or contains the {label}")));
            }
            if path.starts_with(&root) {
                return Err(format!("{var} names a directory inside the {label}").into());
            }
        }
        for (root, label) in [
            (repo_root(self.code_root), "repo root"),
            (home.clone(), "home directory"),
            (home.join(".contextstack"), "state root"),
        ] {
            if root.starts_with(path) || canonical_prefix(&root).starts_with(path) {
                return Err(refuse(format!("directory equals or contains the {label}")));
            }
        }
        for relative in PROTECTED_READS {
            let store = home.join(relative);
            let resolved = canonical_prefix(&store);
            if store.starts_with(path) || resolved.starts_with(path) || path.starts_with(&resolved)
            {
                return Err(refuse(format!(
                    "directory overlaps protected path {}",
                    store.display()
                )));
            }
        }
        let mut under_a_root = false;
        for root in self.roots {
            let root = canonical_prefix(root);
            if path == root {
                return Err(refuse("directory equals an allowed root".to_owned()));
            }
            under_a_root |= path.starts_with(root);
        }
        if !under_a_root {
            return Err(
                format!("{var} names {}, outside its allowed roots", path.display()).into(),
            );
        }
        Ok(())
    }
}

/// The canonical directory `var` names, created (mkdir -p, 0700) when absent.
/// Checked before anything is created and again after canonicalising what was created,
/// so a symlink under an allowed root that points elsewhere is refused.
pub fn grant(
    var: &str,
    value: Option<OsString>,
    bounds: &Bounds<'_>,
) -> Result<PathBuf, GrantError> {
    let path = match value {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => return Err(format!("{var} is unset").into()),
    };
    let dotdot = path.components().any(|c| c == Component::ParentDir);
    if !path.is_absolute() || dotdot {
        return Err(format!("{var} is not an absolute path without ..").into());
    }
    bounds.check(var, &canonical_prefix(&path))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&path)
        .map_err(|error| format!("{var}: cannot create {}: {error}", path.display()))?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("{var}: {}: {error}", path.display()))?;
    bounds.check(var, &canonical)?;
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Layout {
        _dir: tempfile::TempDir,
        base: PathBuf,
        home: PathBuf,
        workspace: PathBuf,
        code: PathBuf,
        roots: Vec<PathBuf>,
    }

    fn layout() -> Result<Layout, String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let base = dir
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let workspace = base.join("ws");
        let code = base.join("code");
        let state = base.join("state-root");
        for d in [&workspace, &code, &state] {
            std::fs::create_dir_all(d).map_err(|error| error.to_string())?;
        }
        Ok(Layout {
            _dir: dir,
            home: base.join("owner"),
            base,
            workspace,
            code,
            roots: vec![state],
        })
    }

    fn try_grant(fx: &Layout, value: Option<&Path>) -> Result<PathBuf, String> {
        let bounds = Bounds {
            workspace: &fx.workspace,
            code_root: &fx.code,
            home: &fx.home,
            roots: &fx.roots,
        };
        grant("CS_X", value.map(|p| p.as_os_str().to_owned()), &bounds).map_err(|error| match error
        {
            GrantError::Narrowed(reason) => reason,
            GrantError::Unenforceable(failure) => {
                format!("{}: {}", failure.code(), failure.message())
            }
        })
    }

    #[test]
    fn the_allowed_root_itself_is_refused() -> Result<(), String> {
        let fx = layout()?;
        assert!(try_grant(&fx, Some(&fx.roots[0])).is_err());
        Ok(())
    }

    #[test]
    fn a_directory_under_the_root_is_created_private_and_granted() -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let fx = layout()?;
        let want = fx.roots[0].join("cs/browser-state");
        assert_eq!(try_grant(&fx, Some(&want)), Ok(want.clone()));
        assert!(want.is_dir());
        let mode = std::fs::metadata(&want)
            .map_err(|error| error.to_string())?
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
        Ok(())
    }

    #[test]
    fn protected_ancestors_and_allowed_root_are_refused() -> Result<(), String> {
        let mut fx = layout()?;
        let root = fx.roots[0].clone();
        fx.workspace = root.join("projects/workspace");
        fx.code = root.join("sources/repo/code");
        fx.home = root.join("homes/owner");
        let state = fx.home.join(".contextstack");
        let alias = root.join("alias");
        std::fs::create_dir_all(&state).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(&state, &alias).map_err(|e| e.to_string())?;
        for (path, expected) in [
            (root.join("projects"), "workspace"),
            (fx.workspace.clone(), "workspace"),
            (root.join("sources/repo"), "code root"),
            (fx.code.clone(), "code root"),
            (root.join("homes"), "home directory"),
            (fx.home.clone(), "home directory"),
            (state.clone(), "state root"),
            (alias, "state root"),
            (state.join("credentials"), "protected path"),
            (fx.home.join(".ssh"), "protected path"),
            (fx.home.join(".config"), "protected path"),
            (fx.home.join("Library"), "protected path"),
            (root.clone(), "workspace"),
        ] {
            let reason = try_grant(&fx, Some(&path))
                .err()
                .ok_or_else(|| format!("granted {}", path.display()))?;
            assert!(
                reason.starts_with("101:") && reason.contains(expected),
                "{}: {reason}",
                path.display()
            );
        }
        // A symlinked code root can have a repo root outside its canonical ancestry.
        let repo = root.join("separate-repo");
        std::fs::create_dir(&repo).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&fx.code).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(&fx.code, repo.join("code")).map_err(|e| e.to_string())?;
        fx.code = repo.join("code");
        let reason = try_grant(&fx, Some(&repo))
            .err()
            .ok_or("granted repo root")?;
        assert!(
            reason.starts_with("101:") && reason.contains("repo root"),
            "{reason}"
        );

        let extra_root = root.join("extra");
        fx.roots.push(extra_root.clone());
        let reason = try_grant(&fx, Some(&extra_root))
            .err()
            .ok_or("granted the allowed root")?;
        assert!(
            reason.starts_with("101:") && reason.contains("equals an allowed root"),
            "{reason}"
        );
        let sibling = state.join("browser");
        assert_eq!(try_grant(&fx, Some(&sibling)), Ok(sibling));
        Ok(())
    }

    #[test]
    fn unset_relative_and_dotdot_are_narrowed() -> Result<(), String> {
        let fx = layout()?;
        assert_eq!(try_grant(&fx, None), Err("CS_X is unset".to_owned()));
        assert_eq!(
            try_grant(&fx, Some(Path::new(""))),
            Err("CS_X is unset".to_owned())
        );
        for bad in ["rel/state", "/a/../b"] {
            let reason = try_grant(&fx, Some(Path::new(bad)))
                .err()
                .unwrap_or_default();
            assert!(reason.contains("not an absolute path"), "{bad}: {reason}");
        }
        Ok(())
    }

    #[test]
    fn outside_the_root_inside_workspace_or_code_root_is_narrowed_and_not_created()
    -> Result<(), String> {
        let fx = layout()?;
        let cases = [
            (fx.base.join("elsewhere/state"), "outside its allowed roots"),
            (fx.workspace.join("state"), "inside the workspace"),
            (fx.code.join("state"), "inside the code root"),
        ];
        for (path, needle) in cases {
            let reason = try_grant(&fx, Some(&path)).err().unwrap_or_default();
            assert!(reason.contains(needle), "{}: {reason}", path.display());
            assert!(!path.exists(), "{} was created", path.display());
        }
        Ok(())
    }

    #[test]
    fn a_symlink_under_the_root_pointing_elsewhere_is_narrowed() -> Result<(), String> {
        let fx = layout()?;
        let elsewhere = fx.base.join("elsewhere");
        std::fs::create_dir(&elsewhere).map_err(|error| error.to_string())?;
        let link = fx.roots[0].join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).map_err(|error| error.to_string())?;
        let reason = try_grant(&fx, Some(&link.join("state")))
            .err()
            .unwrap_or_default();
        assert!(reason.contains("outside its allowed roots"), "{reason}");
        assert!(!elsewhere.join("state").exists());
        let into_ws = fx.roots[0].join("ws-link");
        std::os::unix::fs::symlink(&fx.workspace, &into_ws).map_err(|error| error.to_string())?;
        let reason = try_grant(&fx, Some(&into_ws.join("state")))
            .err()
            .unwrap_or_default();
        assert!(reason.contains("inside the workspace"), "{reason}");
        Ok(())
    }
}
