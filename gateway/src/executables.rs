//! Executable lookup: host-configured locations first, one fallback search
//! path, one validation rule for every candidate. Children inherit the fallback as their PATH.

use serde::Serialize;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A resolved (or failed) executable as reported on /v1/discovery/health.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Identity {
    /// Bare name.
    pub name: String,
    /// The path the gateway runs.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// The file after symlinks.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub real_path: String,
    /// configured | fallback.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Why it failed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// Configured locations and the fallback directories.
#[derive(Debug, Clone, Default)]
pub struct Resolver {
    configured: BTreeMap<String, PathBuf>,
    dirs: Vec<PathBuf>,
}

fn bare(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

impl Resolver {
    /// Validate host configuration (bare names, absolute clean paths) and build the fallback: absolute
    /// entries of `path`, then `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`.
    pub fn new(
        configured: &BTreeMap<String, String>,
        path: &str,
        home: Option<&Path>,
    ) -> Result<Self, String> {
        let mut r = Self::default();
        for (name, p) in configured {
            if !bare(name) {
                return Err(format!("executables: {name:?} is not a bare name"));
            }
            let pb = PathBuf::from(p);
            if !pb.is_absolute()
                || crate::credentials::clean(&pb) != pb
                || p.ends_with('/') && p != "/"
            {
                return Err(format!(
                    "executables: {name} must be an absolute clean path, got {p:?}"
                ));
            }
            r.configured.insert(name.clone(), pb);
        }
        let mut add = |d: PathBuf| {
            if d.is_absolute() && !r.dirs.contains(&d) {
                r.dirs.push(d);
            }
        };
        for d in path.split(':') {
            add(PathBuf::from(d));
        }
        if let Some(h) = home {
            add(h.join(".local/bin"));
        }
        add(PathBuf::from("/opt/homebrew/bin"));
        add(PathBuf::from("/usr/local/bin"));
        Ok(r)
    }

    /// The fallback as a PATH value, existing directories only.
    pub fn search_path(&self) -> String {
        self.dirs
            .iter()
            .filter(|d| d.is_dir())
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Resolve a bare name; a configured name never falls back.
    pub fn resolve(&self, name: &str) -> Result<Identity, Identity> {
        let mut id = Identity {
            name: name.into(),
            path: String::new(),
            real_path: String::new(),
            source: String::new(),
            error: String::new(),
        };
        if !bare(name) {
            id.error = format!(
                "{name:?} is not a bare executable name; configure it under models.json executables"
            );
            return Err(id);
        }
        if let Some(p) = self.configured.get(name) {
            id.path = p.display().to_string();
            id.source = "configured".into();
            return match check(p) {
                Ok(real) => {
                    id.real_path = real;
                    Ok(id)
                }
                Err(e) => {
                    id.error = format!("configured {name}: {e}");
                    Err(id)
                }
            };
        }
        id.source = "fallback".into();
        for d in &self.dirs {
            let p = d.join(name);
            if std::fs::symlink_metadata(&p).is_err() {
                continue;
            }
            if let Ok(real) = check(&p) {
                id.path = p.display().to_string();
                id.real_path = real;
                return Ok(id);
            }
        }
        let dirs: Vec<String> = self.dirs.iter().map(|d| d.display().to_string()).collect();
        id.error = format!("{name} not found in {}", dirs.join(":"));
        Err(id)
    }

    /// The path or the error text.
    pub fn look_path(&self, name: &str) -> Result<String, String> {
        self.resolve(name).map(|i| i.path).map_err(|i| i.error)
    }
}

/// A regular, executable file after symlinks, not world-writable.
fn check(p: &Path) -> Result<String, String> {
    let real = std::fs::canonicalize(p)
        .map_err(|e| format!("lstat {}: {}", p.display(), crate::server::os_error(&e)))?;
    let meta = std::fs::metadata(&real).map_err(|e| e.to_string())?;
    let mode = meta.permissions().mode();
    let shown = p.display();
    if !meta.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    if mode & 0o111 == 0 {
        return Err(format!("{shown} is not executable"));
    }
    if mode & 0o002 != 0 {
        return Err(format!("{shown} is world-writable"));
    }
    Ok(real.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_is_validated() {
        let mut c = BTreeMap::new();
        c.insert("claude".to_string(), "bin/claude".to_string());
        assert!(
            Resolver::new(&c, "", None)
                .is_err_and(|e| e.contains("must be an absolute clean path"))
        );
        let mut c = BTreeMap::new();
        c.insert("a/b".to_string(), "/bin/sh".to_string());
        assert!(Resolver::new(&c, "", None).is_err_and(|e| e.contains("is not a bare name")));
        let mut c = BTreeMap::new();
        c.insert("sh".to_string(), "/bin/sh".to_string());
        let r = Resolver::new(&c, "/usr/bin", None);
        assert!(r.is_ok_and(|r| r.resolve("sh").is_ok() && r.resolve("x/y").is_err()));
    }
}
