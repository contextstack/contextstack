//! `write_env`: a tool may write to the directory an environment variable names, when the host
//! permits that variable and the directory lies under one of its allowed roots (the state root).

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
    /// The variable's allowed roots, expanded and absolute.
    pub roots: &'a [PathBuf],
}

impl Bounds<'_> {
    fn check(&self, var: &str, path: &Path) -> Result<(), String> {
        if path.starts_with(self.workspace) {
            return Err(format!("{var} names a directory inside the workspace"));
        }
        if path.starts_with(canonical_prefix(self.code_root)) {
            return Err(format!("{var} names a directory inside the code root"));
        }
        let under_a_root = self
            .roots
            .iter()
            .any(|root| path.starts_with(canonical_prefix(root)));
        if !under_a_root {
            return Err(format!(
                "{var} names {}, outside its allowed roots",
                path.display()
            ));
        }
        Ok(())
    }
}

/// The canonical directory `var` names, created (mkdir -p, 0700) when absent, or the narrowing
/// reason. Checked before anything is created and again after canonicalising what was created,
/// so a symlink under an allowed root that points elsewhere is refused.
pub fn grant(var: &str, value: Option<OsString>, bounds: &Bounds<'_>) -> Result<PathBuf, String> {
    let path = match value {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => return Err(format!("{var} is unset")),
    };
    let dotdot = path.components().any(|c| c == Component::ParentDir);
    if !path.is_absolute() || dotdot {
        return Err(format!("{var} is not an absolute path without .."));
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
            roots: &fx.roots,
        };
        grant("CS_X", value.map(|p| p.as_os_str().to_owned()), &bounds)
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
        let reason = try_grant(&fx, Some(&into_ws)).err().unwrap_or_default();
        assert!(reason.contains("inside the workspace"), "{reason}");
        Ok(())
    }
}
