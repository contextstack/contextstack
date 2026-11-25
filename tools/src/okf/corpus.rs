//! okf's corpus: the root (`corpus_root`), the markdown files under it (Ruby's
//! `Dir.glob(root/**/*.md, File::FNM_DOTMATCH)` sorted as `Pathname#<=>` sorts), and
//! the port's confinement preflight.

use super::rb::Raise;
use crate::jail;
use std::env;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

/// How the root failed to resolve: an okf refusal or an uncaught Ruby error.
pub enum RootError {
    Fail(&'static str),
    Raise(Raise),
}

/// `Errno::X` text as Ruby prints a system call error.
pub fn errno(e: &io::Error, call: &str, path: &Path) -> Raise {
    let class = match e.kind() {
        io::ErrorKind::NotFound => "Errno::ENOENT",
        io::ErrorKind::PermissionDenied => "Errno::EACCES",
        io::ErrorKind::IsADirectory => "Errno::EISDIR",
        io::ErrorKind::NotADirectory => "Errno::ENOTDIR",
        _ => "SystemCallError",
    };
    let text = e.to_string();
    let detail = text.split(" (os error").next().unwrap_or(&text);
    let message = format!("{detail} @ {call} - {}", path.display());
    Raise::new(class, message)
}

/// `File.expand_path(raw)` against the cwd: `~` expansion and lexical `.`/`..` folding.
fn expand_path(raw: &str, cwd: &Path) -> Result<PathBuf, Raise> {
    if raw.contains('\0') {
        return Err(Raise::new("ArgumentError", "string contains null byte"));
    }
    let joined = if raw == "~" || raw.starts_with("~/") {
        let home = env::var("HOME").map_err(|_| {
            Raise::new(
                "ArgumentError",
                "couldn't find HOME environment -- expanding '~'",
            )
        })?;
        PathBuf::from(home).join(raw.trim_start_matches('~').trim_start_matches('/'))
    } else if let Some(user) = raw.strip_prefix('~') {
        let name = user.split('/').next().unwrap_or(user);
        return Err(Raise::new(
            "ArgumentError",
            format!("user {name} doesn't exist"),
        ));
    } else {
        cwd.join(raw)
    };
    let mut out = PathBuf::from("/");
    for part in joined.components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Ok(out)
}

/// `corpus_root`: expand, resolve, then refuse a root outside the workspace or not a
/// directory. Confinement is the crate's jail (resolved paths, component-wise).
pub fn root(raw: &str) -> Result<PathBuf, RootError> {
    let raw = if raw.is_empty() { "." } else { raw };
    let cwd = env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|e| RootError::Raise(errno(&e, "getcwd", Path::new("."))))?;
    let expanded = expand_path(raw, &cwd).map_err(RootError::Raise)?;
    let candidate = match fs::canonicalize(&expanded) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(RootError::Fail("root does not exist"));
        }
        Err(e) => return Err(RootError::Raise(errno(&e, "realpath_rec", &expanded))),
    };
    if jail::resolves_outside(&candidate) {
        return Err(RootError::Fail("root escapes workspace"));
    }
    if !candidate.is_dir() {
        return Err(RootError::Fail("root is not a directory"));
    }
    Ok(candidate)
}

/// Ruby on macOS globs case-insensitively (the volume is); elsewhere case matters.
fn is_markdown(name: &[u8]) -> bool {
    let Some(stem_len) = name.len().checked_sub(3) else {
        return false;
    };
    let ext = &name[stem_len..];
    if cfg!(target_os = "macos") {
        ext.eq_ignore_ascii_case(b".md")
    } else {
        ext == b".md"
    }
}

/// `**/*.md` with dot-matching: every entry named `*.md` (any type), descending into real
/// directories but never through a symlinked one. Unreadable directories are skipped.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_markdown(entry.file_name().as_bytes()) {
            out.push(path.clone());
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&path, out);
        }
    }
}

/// `Pathname#<=>`: bytewise, with `/` sorting before everything.
fn path_key(p: &Path) -> Vec<u8> {
    p.as_os_str()
        .as_bytes()
        .iter()
        .map(|b| if *b == b'/' { 0 } else { *b })
        .collect()
}

/// `relative_path_from(root)` for a path under the root.
pub fn relative(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().into_owned()
}

/// `markdown_files`: the sorted corpus, `.git` and `.cs-*` components excluded.
pub fn markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, &mut found);
    found.retain(|p| {
        let rel = relative(p, root);
        !rel.split('/')
            .any(|part| part == ".git" || part.starts_with(".cs-"))
    });
    found.sort_by_cached_key(|p| path_key(p));
    found
}

/// A root entry named `index.md` in any case that is a file (`root.children.find`), in
/// directory order.
pub fn index_entry(root: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    entries.flatten().map(|e| e.path()).find(|p| {
        p.is_file()
            && p.file_name()
                .is_some_and(|n| n.to_string_lossy().to_lowercase() == "index.md")
    })
}

/// Stronger than the Ruby: refuse before touching anything when a corpus file (or the
/// root index okf would rewrite) is a symlink that dangles or resolves outside the
/// workspace. The Ruby would read and write through it. Returns the offending path.
pub fn escaping(root: &Path, files: &[PathBuf]) -> Option<String> {
    let index = [root.join("index.md")];
    let candidate = index_entry(root);
    files
        .iter()
        .chain(index.iter())
        .chain(candidate.iter())
        .find(|p| jail::link_leaves_workspace(p) || jail::resolves_outside(p))
        .map(|p| relative(p, root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jail::CwdRestore;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn in_workspace<F: FnOnce(&Path)>(f: F) {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let _cwd = CwdRestore::chdir(dir.path());
        f(dir.path());
    }
    #[test]
    fn root_refuses_missing_path_and_non_directory() {
        in_workspace(|ws| {
            assert!(matches!(
                root("nope"),
                Err(RootError::Fail("root does not exist"))
            ));
            let file = ws.join("file.txt");
            fs::write(&file, "x").unwrap_or_else(|e| panic!("write: {e}"));
            assert!(matches!(
                root("file.txt"),
                Err(RootError::Fail("root is not a directory"))
            ));
        });
    }
    #[test]
    fn root_resolves_relative_paths_inside_workspace() {
        in_workspace(|ws| {
            let corpus = ws.join("corpus");
            fs::create_dir(&corpus).unwrap_or_else(|e| panic!("mkdir: {e}"));
            fs::write(corpus.join("a.md"), "# a\n").unwrap_or_else(|e| panic!("write: {e}"));
            let Ok(resolved) = root("corpus") else {
                panic!("corpus root should resolve");
            };
            assert_eq!(
                resolved,
                fs::canonicalize(&corpus).unwrap_or_else(|e| panic!("canon: {e}"))
            );
            let files = markdown_files(&resolved);
            assert_eq!(files.len(), 1);
            assert_eq!(relative(&files[0], &resolved), "a.md");
        });
    }
    #[test]
    fn markdown_walk_skips_dot_git_and_spill_prefixes() {
        in_workspace(|ws| {
            let corpus = ws.join("c");
            fs::create_dir_all(corpus.join(".git/objects"))
                .unwrap_or_else(|e| panic!("mkdir: {e}"));
            fs::write(corpus.join(".git/objects/x.md"), "secret\n")
                .unwrap_or_else(|e| panic!("write: {e}"));
            fs::create_dir(corpus.join(".cs-hidden")).unwrap_or_else(|e| panic!("mkdir: {e}"));
            fs::write(corpus.join(".cs-hidden/h.md"), "h\n")
                .unwrap_or_else(|e| panic!("write: {e}"));
            fs::write(corpus.join("visible.md"), "v\n").unwrap_or_else(|e| panic!("write: {e}"));
            let files = markdown_files(&corpus);
            assert_eq!(files.len(), 1);
            assert_eq!(relative(&files[0], &corpus), "visible.md");
        });
    }
    #[test]
    fn escaping_flags_symlinks_that_leave_workspace() {
        in_workspace(|ws| {
            let outside = TempDir::new().unwrap_or_else(|e| panic!("outside: {e}"));
            fs::write(outside.path().join("out.md"), "# out\n")
                .unwrap_or_else(|e| panic!("write: {e}"));
            let corpus = ws.join("corpus");
            fs::create_dir(&corpus).unwrap_or_else(|e| panic!("mkdir: {e}"));
            fs::write(corpus.join("local.md"), "# local\n")
                .unwrap_or_else(|e| panic!("write: {e}"));
            symlink(outside.path().join("out.md"), corpus.join("link.md"))
                .unwrap_or_else(|e| panic!("symlink: {e}"));
            let root = fs::canonicalize(&corpus).unwrap_or_else(|e| panic!("canon: {e}"));
            let files = markdown_files(&root);
            assert_eq!(escaping(&root, &files).as_deref(), Some("link.md"));
        });
    }
    #[test]
    fn expand_path_rejects_tilde_user_paths() {
        in_workspace(|_| {
            let Err(err) = expand_path("~nobody", Path::new(".")) else {
                panic!("expected tilde user to fail");
            };
            assert_eq!(err.class, "ArgumentError");
            assert!(err.message.contains("doesn't exist"));
        });
    }
    #[test]
    fn expand_path_rejects_embedded_null_byte() {
        in_workspace(|_| {
            let Err(err) = expand_path("a\0b", Path::new(".")) else {
                panic!("expected null byte refusal");
            };
            assert_eq!(err.class, "ArgumentError");
            assert!(err.message.contains("null byte"));
        });
    }
    #[test]
    fn root_refuses_directory_outside_workspace() {
        in_workspace(|ws| {
            let outside = TempDir::new().unwrap_or_else(|e| panic!("outside: {e}"));
            let link = ws.join("escape");
            symlink(outside.path(), &link).unwrap_or_else(|e| panic!("symlink: {e}"));
            let raw = format!("{}", link.display());
            assert!(matches!(
                root(&raw),
                Err(RootError::Fail("root escapes workspace"))
            ));
        });
    }
    #[test]
    fn index_entry_finds_case_insensitive_index_on_macos() {
        in_workspace(|ws| {
            let corpus = ws.join("c");
            fs::create_dir(&corpus).unwrap_or_else(|e| panic!("mkdir: {e}"));
            fs::write(corpus.join("INDEX.md"), "# idx\n").unwrap_or_else(|e| panic!("write: {e}"));
            assert!(index_entry(&corpus).is_some());
        });
    }
    #[test]
    fn expand_path_folds_parent_dir_components() {
        in_workspace(|ws| {
            let base = ws.join("deep").join("dir");
            fs::create_dir_all(&base).unwrap_or_else(|e| panic!("mkdir: {e}"));
            let expanded =
                expand_path("deep/dir/../dir/leaf", ws).unwrap_or_else(|e| panic!("{}", e.message));
            assert_eq!(expanded, ws.join("deep/dir/leaf"));
        });
    }
    #[test]
    fn errno_maps_not_found_to_enoent() {
        let err = io::Error::from(io::ErrorKind::NotFound);
        let raise = errno(&err, "rb_sysopen", Path::new("x.md"));
        assert_eq!(raise.class, "Errno::ENOENT");
        assert!(raise.message.contains("@ rb_sysopen"));
    }
    #[test]
    fn errno_maps_common_kinds_to_ruby_subclasses() {
        for (kind, class) in [
            (io::ErrorKind::PermissionDenied, "Errno::EACCES"),
            (io::ErrorKind::IsADirectory, "Errno::EISDIR"),
            (io::ErrorKind::NotADirectory, "Errno::ENOTDIR"),
        ] {
            let raise = errno(&io::Error::from(kind), "rb_sysopen", Path::new("p"));
            assert_eq!(raise.class, class);
        }
        let raise = errno(
            &io::Error::from(io::ErrorKind::Other),
            "rb_sysopen",
            Path::new("p"),
        );
        assert_eq!(raise.class, "SystemCallError");
    }

    #[test]
    fn expand_path_expands_tilde_slash_against_home() {
        in_workspace(|ws| {
            let home = std::env::var("HOME").unwrap_or_else(|e| panic!("HOME: {e}"));
            let expanded = expand_path("~/notes", ws).unwrap_or_else(|e| panic!("{}", e.message));
            assert_eq!(expanded, PathBuf::from(home).join("notes"));
        });
    }

    #[test]
    fn is_markdown_rejects_names_shorter_than_dot_md() {
        assert!(!is_markdown(b"."));
        assert!(!is_markdown(b"a"));
        assert!(is_markdown(b"note.md"));
    }

    #[test]
    fn walk_ignores_unreadable_directories() {
        in_workspace(|ws| {
            let corpus = ws.join("c");
            fs::create_dir(&corpus).unwrap_or_else(|e| panic!("mkdir: {e}"));
            fs::write(corpus.join("top.md"), "# top\n").unwrap_or_else(|e| panic!("write: {e}"));
            let secret = corpus.join("secret");
            fs::create_dir(&secret).unwrap_or_else(|e| panic!("mkdir: {e}"));
            fs::write(secret.join("inner.md"), "# in\n").unwrap_or_else(|e| panic!("write: {e}"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&secret)
                    .unwrap_or_else(|e| panic!("meta: {e}"))
                    .permissions();
                perms.set_mode(0o0);
                fs::set_permissions(&secret, perms).unwrap_or_else(|e| panic!("chmod: {e}"));
            }
            let files = markdown_files(&corpus);
            assert_eq!(files.len(), 1);
            assert_eq!(relative(&files[0], &corpus), "top.md");
        });
    }
}
