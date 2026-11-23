//! Identify explicit output files in the frozen goal or check.
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::MetadataExt as _,
    path::{Component, Path},
};

pub(crate) type FileStamp = (u64, u64, u64, i64, i64);

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
        .map(|word| word.trim_end_matches('.'))
        .filter(|word| !word.is_empty())
}

fn filename(word: &str) -> Option<&str> {
    let word = word.strip_prefix("./").unwrap_or(word);
    let path = Path::new(word);
    (path.extension().is_some()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_))))
    .then_some(word)
}

pub(crate) fn named_files<'a>(goal: &'a str, check: &'a str) -> BTreeSet<&'a str> {
    let mut outputs = BTreeSet::new();
    for line in goal.lines() {
        let tokens: Vec<_> = words(line).collect();
        for (index, word) in tokens.iter().enumerate() {
            let verb = word.to_ascii_lowercase();
            let mut rest = &tokens[index + 1..];
            if matches!(rest.first(), Some(&("the" | "your"))) {
                rest = &rest[1..];
            }
            let path = match (verb.as_str(), rest) {
                (
                    "write" | "save" | "put" | "store",
                    [
                        "it" | "answer" | "report" | "results" | "output",
                        "to" | "in",
                        path,
                        ..,
                    ],
                )
                | ("create" | "deliverable" | "output", ["file", path, ..])
                | (
                    "write" | "save" | "create" | "produce" | "output" | "deliverable",
                    [path, ..],
                ) => Some(path),
                _ => None,
            };
            outputs.extend(path.and_then(|path| filename(path)));
        }
    }
    for line in check.lines() {
        let tokens: Vec<_> = words(line).collect();
        if let ["test", "-s" | "-f", path] | ["-s" | "-f", path] = tokens.as_slice() {
            outputs.extend(filename(path));
        }
    }
    outputs
}

pub(crate) fn stamp(workspace: &Path, name: &str) -> Option<FileStamp> {
    let workspace = workspace.canonicalize().ok()?;
    let candidate = workspace.join(name);
    let path = candidate.canonicalize().ok()?;
    if path != candidate {
        return None;
    }
    let meta = fs::symlink_metadata(path).ok()?;
    meta.is_file().then(|| {
        (
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_explicit_output_files() {
        let goal = "Read archive.md, then write `docs/answer.md`. Leave ../../outside.md alone.";
        let check = "grep -qx 'snapshot fact' './docs/answer.md' # ../../outside.md";
        assert_eq!(named_files(goal, check), BTreeSet::from(["docs/answer.md"]));
        assert_eq!(named_files(goal, ""), BTreeSet::from(["docs/answer.md"]));
        assert_eq!(
            named_files("Write the report to `reports/answer.md`.", ""),
            BTreeSet::from(["reports/answer.md"])
        );
        assert_eq!(
            named_files("Report the fact.", "test -s answer.md"),
            BTreeSet::from(["answer.md"])
        );
        assert!(
            named_files(
                "Fix a bug in Cargo.toml.",
                "cargo test --manifest-path Cargo.toml"
            )
            .is_empty()
        );
    }

    #[test]
    fn stamps_missing_existing_and_changed_files() -> std::io::Result<()> {
        let workspace = tempfile::tempdir()?;
        let path = workspace.path().join("answer.md");
        assert_eq!(stamp(workspace.path(), "answer.md"), None);
        fs::write(&path, "first")?;
        let first = stamp(workspace.path(), "answer.md");
        assert!(first.is_some());
        assert_eq!(stamp(workspace.path(), "answer.md"), first);
        fs::write(path, "changed")?;
        assert_ne!(stamp(workspace.path(), "answer.md"), first);
        Ok(())
    }
}
