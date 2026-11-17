//! Seatbelt path literals for SBPL profiles.

use std::path::Path;

use crate::failure::{Failure, unenforceable};

/// Return a Seatbelt double-quoted path or subpath literal.
pub fn quote(path: &Path) -> Result<String, Failure> {
    let text = path.to_string_lossy();
    for ch in text.chars() {
        let code = u32::from(ch);
        if code == 0 || code < 0x20 || code == 0x7F {
            return Err(unenforceable(format!(
                "path contains a control character: {text}"
            )));
        }
    }
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::UNENFORCEABLE;

    #[test]
    fn quotes_escape_backslash_and_double_quote() -> Result<(), Failure> {
        let lit = quote(Path::new(r#"/tmp/a"b\c"#))?;
        assert_eq!(lit, r#""/tmp/a\"b\\c""#);
        Ok(())
    }

    #[test]
    fn newlines_are_refused() -> Result<(), Failure> {
        match quote(Path::new("/tmp/\n")) {
            Err(failure) => {
                assert_eq!(failure.code(), UNENFORCEABLE);
                assert!(failure.message().contains("control character"));
                Ok(())
            }
            Ok(_) => Err(unenforceable("expected control character refusal")),
        }
    }
}

/// Python's re.escape character set (Python 3.7 and later).
pub(crate) fn regex_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\u{b}\u{c}".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Resolve existing prefixes, retaining missing suffixes like Path.resolve(strict=False).
pub(crate) fn resolve(path: &Path) -> Result<std::path::PathBuf, Failure> {
    use std::collections::{HashSet, VecDeque};
    use std::ffi::OsString;
    use std::path::{Component, PathBuf};
    enum Step {
        Part(OsString),
        Done(PathBuf),
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| unenforceable(e.to_string()))?
            .join(path)
    };
    let mut pending: VecDeque<_> = absolute
        .components()
        .map(|c| Step::Part(c.as_os_str().to_owned()))
        .collect();
    let mut active = HashSet::new();
    let mut result = PathBuf::new();
    while let Some(step) = pending.pop_front() {
        let part = match step {
            Step::Done(path) => {
                active.remove(&path);
                continue;
            }
            Step::Part(part) => part,
        };
        match Path::new(&part).components().next() {
            Some(Component::ParentDir) => {
                result.pop();
            }
            Some(Component::CurDir) => {}
            _ => {
                result.push(&part);
                if let Ok(target) = std::fs::read_link(&result) {
                    if !active.insert(result.clone()) {
                        return Err(unenforceable("path symlink loop"));
                    }
                    pending.push_front(Step::Done(result.clone()));
                    result.pop();
                    for component in target.components().rev() {
                        pending.push_front(Step::Part(component.as_os_str().to_owned()));
                    }
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    #[test]
    fn python_regex_special_characters_include_whitespace_and_punctuation() {
        assert_eq!(
            regex_escape("a #&~\t\n\r\u{b}\u{c}/_!%"),
            "a\\ \\#\\&\\~\\\t\\\n\\\r\\\u{b}\\\u{c}/_!%"
        );
    }

    #[test]
    fn resolves_symlink_prefixes_and_missing_suffixes() -> Result<(), String> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let root = temp.path().canonicalize().map_err(|e| e.to_string())?;
        std::fs::create_dir(root.join("real")).map_err(|e| e.to_string())?;
        symlink("real/missing", root.join("alias")).map_err(|e| e.to_string())?;
        assert_eq!(
            resolve(&root.join("alias/../child")).map_err(|e| e.message().to_owned())?,
            root.join("real/child")
        );
        symlink("real", root.join("directory")).map_err(|e| e.to_string())?;
        assert_eq!(
            resolve(&root.join("directory/absent/leaf")).map_err(|e| e.message().to_owned())?,
            root.join("real/absent/leaf")
        );
        symlink("loop", root.join("loop")).map_err(|e| e.to_string())?;
        assert!(resolve(&root.join("loop/absent")).is_err());
        Ok(())
    }
}
