//! Deny reads of secret env files (`.env`, `.envrc`, `*.env`, `.env.*` except `.env.example`)
//! under readable roots, leaving `.env.example` readable only inside those roots.
//!
//! Seatbelt note (macOS): Seatbelt file rules match by pathname, not vnode identity. In addition to
//! `file-read*`, `seatbelt_rules` denies `file-write*` (including data writes, truncation, creation,
//! rename, and unlink) and `file-link` (hard links) on secret env names. This prevents a
//! tool inside a writable workspace from reading `.env` by renaming it (`mv .env x; cat x`) or
//! hard-linking it (`ln .env x; cat x`). Residual risk: if a parent directory inside a writable
//! root itself holds a `.env` file, renaming that parent directory is not blocked unless the parent
//! is the workspace root; however, the file's basename inside the renamed directory remains `.env`
//! and continues to match the `file-read*`, `file-write*`, and `file-link` regexes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::failure::{Failure, unenforceable};
use crate::sbpl;

/// Maximum directory recursion depth when scanning a mask root for secret env files.
pub const MAX_WALK_DEPTH: usize = 32;
/// Maximum filesystem entries inspected under one mask root (each root has its own budget).
pub const MAX_WALK_ENTRIES: usize = 50_000;

/// Build-output and dependency trees the mask walk does not descend into: they are large (a
/// `target/` or `node_modules` alone can exceed the entry cap) and hold generated or vendored
/// content, not the repo's own secret env files.
pub const SKIPPED_DIRS: &[&str] = &["target", "node_modules", ".git", "vendor"];

/// Whether the walk skips a directory with this basename.
pub fn is_skipped_dir(name: &std::ffi::OsStr) -> bool {
    SKIPPED_DIRS.iter().any(|skip| name == *skip)
}

/// Whether a basename is a secret env file the host policy must not expose.
pub fn is_secret_env(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            if lower == ".env.example" {
                return false;
            }
            lower == ".env"
                || lower.starts_with(".envrc")
                || lower.rsplit_once('.').is_some_and(|(_, ext)| ext == "env")
                || lower
                    .strip_prefix(".env")
                    .is_some_and(|rest| rest.starts_with(['.', '_', '-']) && rest.len() > 1)
                || lower
                    .rsplit_once(".env.")
                    .is_some_and(|(pre, suf)| !pre.is_empty() && !suf.is_empty())
        })
}

/// Every secret env file under `roots` (the caller passes the writable and repo-view roots, see
/// `bwrap::mask_roots`), without following symlinks (`lstat`) and without descending into
/// `SKIPPED_DIRS`. Any root is walked, including one under `/var` or `/opt`. Fails with exit 101
/// (`Failure::Unenforceable`) if a directory cannot be read or if the walk of one root exceeds
/// `MAX_WALK_DEPTH` or `MAX_WALK_ENTRIES`.
pub fn masked_paths(roots: &[PathBuf]) -> Result<Vec<PathBuf>, Failure> {
    let mut found = BTreeSet::new();
    for root in roots {
        let mut entries_seen = 0usize;
        let Ok(meta) = std::fs::symlink_metadata(root) else {
            continue;
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk_secret_env(root, 0, &mut entries_seen, &mut found)?;
        } else if ft.is_file() && is_secret_env(root) {
            found.insert(root.clone());
        }
    }
    Ok(found.into_iter().collect())
}

fn walk_secret_env(
    dir: &Path,
    depth: usize,
    entries_seen: &mut usize,
    out: &mut BTreeSet<PathBuf>,
) -> Result<(), Failure> {
    if depth > MAX_WALK_DEPTH {
        return Err(unenforceable(format!(
            "env mask walk exceeded max depth ({MAX_WALK_DEPTH}) at {}; the tool was not run",
            dir.display()
        )));
    }
    let entries = std::fs::read_dir(dir).map_err(|error| {
        unenforceable(format!(
            "env mask walk cannot read {}: {error}; the tool was not run",
            dir.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            unenforceable(format!(
                "env mask walk entry error in {}: {error}; the tool was not run",
                dir.display()
            ))
        })?;
        *entries_seen += 1;
        if *entries_seen > MAX_WALK_ENTRIES {
            return Err(unenforceable(format!(
                "env mask walk exceeded max entries ({MAX_WALK_ENTRIES}) under {}; the tool was not run",
                dir.display()
            )));
        }
        let path = entry.path();
        let ft = entry.file_type().map_err(|error| {
            unenforceable(format!(
                "env mask walk cannot inspect {}: {error}; the tool was not run",
                path.display()
            ))
        })?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if is_skipped_dir(&entry.file_name()) {
                continue;
            }
            walk_secret_env(&path, depth + 1, entries_seen, out)?;
        } else if ft.is_file() && is_secret_env(&path) {
            out.insert(path);
        }
    }
    Ok(())
}

/// Block secret env files; leave `.env.example` readable only under `read_roots`.
pub fn seatbelt_rules(read_roots: &[impl AsRef<Path>]) -> Result<Vec<String>, Failure> {
    let mut rules = vec![
        r#"(deny file-read* file-write* file-link (regex #"/\.env$"))"#.to_owned(),
        r#"(deny file-read* file-write* file-link (regex #"/\.env\.[^/]+$"))"#.to_owned(),
        r#"(deny file-read* file-write* file-link (regex #"/\.env_[^/]+$"))"#.to_owned(),
        r#"(deny file-read* file-write* file-link (regex #"/\.env-[^/]+$"))"#.to_owned(),
        r#"(deny file-read* file-write* file-link (regex #"/\.envrc[^/]*$"))"#.to_owned(),
        r#"(deny file-read* file-write* file-link (regex #"/[^/]+\.env$"))"#.to_owned(),
        r#"(deny file-read* file-write* file-link (regex #"/[^/]+\.env\.[^/]+$"))"#.to_owned(),
    ];
    for root in read_roots {
        rules.push(format!(
            r#"(allow file-read* (require-all (subpath {}) (regex #"/\.env\.example$")))"#,
            sbpl::quote(root.as_ref())?
        ));
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn secret_env_names() {
        assert!(is_secret_env(Path::new("/x/.env")));
        assert!(is_secret_env(Path::new("/x/.envrc")));
        assert!(is_secret_env(Path::new("/x/.envrc.local")));
        assert!(is_secret_env(Path::new("/x/foo.env")));
        assert!(is_secret_env(Path::new("/x/x.env.bak")));
        assert!(is_secret_env(Path::new("/x/.env.local")));
        assert!(is_secret_env(Path::new("/x/.env_prod")));
        assert!(is_secret_env(Path::new("/x/.env-local")));
        assert!(!is_secret_env(Path::new("/x/.env.example")));
        assert!(!is_secret_env(Path::new("/x/env")));
    }

    #[test]
    fn build_and_dependency_dirs_are_skipped() {
        for skip in ["target", "node_modules", ".git", "vendor"] {
            assert!(is_skipped_dir(std::ffi::OsStr::new(skip)), "{skip}");
        }
        for walk in [
            "src",
            "targets",
            "node_module",
            ".github",
            "vendored",
            "config",
        ] {
            assert!(!is_skipped_dir(std::ffi::OsStr::new(walk)), "{walk}");
        }
    }

    #[test]
    fn walk_skips_build_trees_and_masks_the_rest() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let root = dir.path();
        for sub in [
            "target/debug",
            "node_modules/pkg",
            ".git/x",
            "vendor/lib",
            "app",
        ] {
            fs::create_dir_all(root.join(sub)).map_err(|e| e.to_string())?;
            fs::write(root.join(sub).join(".env"), "x").map_err(|e| e.to_string())?;
        }
        let masks = masked_paths(&[root.to_path_buf()]).map_err(|f| f.message().to_owned())?;
        assert_eq!(masks, vec![root.join("app/.env")]);
        Ok(())
    }

    #[test]
    fn a_large_build_tree_does_not_trip_the_cap() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let root = dir.path();
        let deps = root.join("node_modules");
        fs::create_dir(&deps).map_err(|e| e.to_string())?;
        for i in 0..(MAX_WALK_ENTRIES + 10) {
            fs::File::create(deps.join(i.to_string())).map_err(|e| e.to_string())?;
        }
        fs::write(root.join(".env"), "x").map_err(|e| e.to_string())?;
        let masks = masked_paths(&[root.to_path_buf()]).map_err(|f| f.message().to_owned())?;
        assert_eq!(masks, vec![root.join(".env")]);
        Ok(())
    }

    #[test]
    fn entry_cap_is_per_root() -> Result<(), String> {
        let a = tempfile::tempdir().map_err(|e| e.to_string())?;
        let b = tempfile::tempdir().map_err(|e| e.to_string())?;
        // Each root alone is under the cap; together they exceed it.
        for root in [a.path(), b.path()] {
            for i in 0..(MAX_WALK_ENTRIES / 2 + 10) {
                fs::File::create(root.join(i.to_string())).map_err(|e| e.to_string())?;
            }
            fs::write(root.join(".env"), "x").map_err(|e| e.to_string())?;
        }
        let masks = masked_paths(&[a.path().to_path_buf(), b.path().to_path_buf()])
            .map_err(|f| f.message().to_owned())?;
        assert_eq!(masks.len(), 2, "{masks:?}");
        Ok(())
    }

    #[test]
    fn roots_under_system_prefixes_are_still_walked() -> Result<(), String> {
        // A workspace under /var (here /var/tmp, world-writable on macOS and Linux) is a mask root
        // like any other (review F3): its `.env` is masked.
        let dir = tempfile::tempdir_in("/var/tmp").map_err(|e| e.to_string())?;
        let root = dir.path().canonicalize().map_err(|e| e.to_string())?;
        fs::write(root.join(".env"), "x").map_err(|e| e.to_string())?;
        let masks =
            masked_paths(std::slice::from_ref(&root)).map_err(|f| f.message().to_owned())?;
        assert_eq!(masks, vec![root.join(".env")]);
        Ok(())
    }

    #[test]
    fn discover_masks_under_roots_and_ignores_symlink_loops() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let root = dir.path();
        fs::write(root.join(".env"), "x").map_err(|e| e.to_string())?;
        fs::write(root.join(".env_prod"), "x").map_err(|e| e.to_string())?;
        fs::write(root.join(".env-local"), "x").map_err(|e| e.to_string())?;
        fs::write(root.join(".envrc"), "x").map_err(|e| e.to_string())?;
        fs::write(root.join(".envrc.local"), "x").map_err(|e| e.to_string())?;
        fs::write(root.join("foo.env"), "x").map_err(|e| e.to_string())?;
        fs::write(root.join("x.env.bak"), "x").map_err(|e| e.to_string())?;
        fs::create_dir(root.join("sub")).map_err(|e| e.to_string())?;
        fs::write(root.join("sub/.env.production"), "y").map_err(|e| e.to_string())?;
        fs::write(root.join("sub/.env.example"), "ok").map_err(|e| e.to_string())?;
        // Symlink loop (sub/loop -> root) and symlinked .env file must not be followed.
        symlink(root, root.join("sub/loop")).map_err(|e| e.to_string())?;
        let outside = tempfile::tempdir().map_err(|e| e.to_string())?;
        fs::write(outside.path().join(".env"), "outside").map_err(|e| e.to_string())?;
        symlink(outside.path(), root.join("sub/escape_dir")).map_err(|e| e.to_string())?;
        symlink(outside.path().join(".env"), root.join("sub/.env.link"))
            .map_err(|e| e.to_string())?;

        let masks = masked_paths(&[root.to_path_buf()]).map_err(|f| f.message().to_owned())?;
        assert_eq!(masks.len(), 8, "{masks:?}");
        for expected in [".env_prod", ".env-local", ".envrc.local", "x.env.bak"] {
            assert!(
                masks.iter().any(|p| p.ends_with(expected)),
                "missing {expected}"
            );
        }
        assert!(!masks.iter().any(|p| p.ends_with(".env.example")));
        assert!(masks.iter().any(|p| p == &root.join(".env")));
        assert!(masks.iter().any(|p| p == &root.join(".envrc")));
        assert!(masks.iter().any(|p| p == &root.join("foo.env")));
        assert!(masks.iter().any(|p| p == &root.join("sub/.env.production")));
        assert!(!masks.iter().any(|p| p.ends_with(".env.link")));
        Ok(())
    }

    #[test]
    fn walk_fails_clearly_when_depth_exceeds_cap() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let mut cur = dir.path().to_path_buf();
        for i in 0..=(MAX_WALK_DEPTH + 2) {
            cur = cur.join(format!("d{i}"));
        }
        fs::create_dir_all(&cur).map_err(|e| e.to_string())?;
        let err = masked_paths(&[dir.path().to_path_buf()])
            .err()
            .ok_or_else(|| "expected depth cap error".to_owned())?;
        assert_eq!(err.code(), 101);
        assert!(
            err.message().contains("max depth"),
            "unexpected message: {}",
            err.message()
        );
        Ok(())
    }

    #[test]
    fn walk_fails_clearly_when_entries_exceed_cap() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        fs::write(dir.path().join("a"), "").map_err(|e| e.to_string())?;
        fs::write(dir.path().join("b"), "").map_err(|e| e.to_string())?;
        let mut seen = MAX_WALK_ENTRIES - 1;
        let mut out = BTreeSet::new();
        let err = walk_secret_env(dir.path(), 0, &mut seen, &mut out)
            .err()
            .ok_or_else(|| "expected entry cap error".to_owned())?;
        assert_eq!(err.code(), 101);
        assert!(
            err.message().contains("max entries"),
            "unexpected message: {}",
            err.message()
        );
        Ok(())
    }
}
