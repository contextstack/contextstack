//! Bounded literal search of the workspace or a read-only repository snapshot.
use crate::{jail, py::Failure};
use serde_json::Value;
use std::{
    fmt::Write as _,
    fs, io,
    io::Read as _,
    path::{Component, Path},
};

const MAX_ENTRIES: usize = 20_000;
const MAX_RESULTS: usize = 20;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 32;

pub fn run(args: &Value) -> Result<Vec<u8>, Failure> {
    let view = jail::repo_view();
    let scope = if view.is_some() {
        "repository view"
    } else {
        "workspace"
    };
    let root = match view {
        Some(root) => root,
        None => std::env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(|error| Failure::exit(format!("Cannot resolve workspace: {error}")))?,
    };
    let mode = match args["mode"].as_str() {
        Some("files") => Mode::Files,
        Some("text") => Mode::Text,
        _ => return Err(Failure::exit("Set mode to files or text.")),
    };
    let query = args["query"]
        .as_str()
        .filter(|query| !query.is_empty() && !query.contains(['\r', '\n']))
        .ok_or_else(|| Failure::exit("Pass a nonempty, single-line literal query."))?;
    let requested = match args.get("path") {
        None => ".",
        Some(path) => path.as_str().ok_or_else(|| {
            Failure::exit(format!(
                "Pass path as a relative or absolute directory inside the {scope}."
            ))
        })?,
    };
    let path = Path::new(requested);
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(outside(requested, scope));
    }
    let relative = if path.is_absolute() {
        path.strip_prefix(&root)
            .ok()
            .or_else(|| {
                // The configured spelling can be an alias for the canonical mount.
                let view = std::env::var_os("CS_REPO_VIEW")?;
                path.strip_prefix(view).ok()
            })
            .ok_or_else(|| outside(requested, scope))?
    } else {
        path
    };
    let mut directory = root.clone();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        directory.push(part);
        let metadata = fs::symlink_metadata(&directory).map_err(|error| {
            Failure::exit(format!(
                "Cannot search {requested:?}: {error}. Use a directory inside the {scope}."
            ))
        })?;
        if metadata.is_symlink() {
            return Err(outside(requested, scope));
        }
    }

    let mut search = Search {
        root: &root,
        mode,
        query,
        entries: 0,
        results: 0,
        bytes_left: MAX_READ_BYTES,
        incomplete: false,
        output: String::new(),
    };
    search.walk(&directory, 0).map_err(|error| {
        Failure::exit(format!(
            "Cannot search {requested:?}: {error}. Use an accessible repository directory."
        ))
    })?;
    if search.output.is_empty() {
        search.output.push_str("No matches in searched files.\n");
    }
    if search.incomplete || search.full() {
        search
            .output
            .push_str("Search limited. Narrow path or query for more results.\n");
    }
    Ok(search.output.into_bytes())
}

fn outside(requested: &str, scope: &str) -> Failure {
    Failure::exit(format!(
        "Cannot search {requested:?} outside the {scope} or through a symlink. Use a relative or absolute directory inside the {scope}."
    ))
}

enum Mode {
    Files,
    Text,
}

struct Search<'a> {
    root: &'a Path,
    mode: Mode,
    query: &'a str,
    entries: usize,
    results: usize,
    bytes_left: u64,
    incomplete: bool,
    output: String,
}

impl Search<'_> {
    fn full(&self) -> bool {
        self.entries >= MAX_ENTRIES || self.results >= MAX_RESULTS || self.bytes_left == 0
    }

    fn walk(&mut self, directory: &Path, depth: usize) -> io::Result<()> {
        if depth >= MAX_DEPTH {
            self.incomplete = true;
            return Ok(());
        }
        if directory.canonicalize()? != directory {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "symlink directory",
            ));
        }
        let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_cached_key(fs::DirEntry::file_name);
        for entry in entries {
            if self.full() {
                break;
            }
            self.entries += 1;
            if self.visit(&entry, depth).is_err() {
                self.incomplete = true;
            }
        }
        Ok(())
    }

    fn visit(&mut self, entry: &fs::DirEntry, depth: usize) -> io::Result<()> {
        let kind = entry.file_type()?;
        let path = entry.path();
        if entry.file_name() == ".git" {
            return Ok(());
        }
        if kind.is_dir() {
            self.walk(&path, depth + 1)?;
        } else if kind.is_file() {
            match self.mode {
                Mode::Files if entry.file_name().to_string_lossy().contains(self.query) => {
                    self.push(format_args!("{}", path.display()));
                }
                Mode::Text => self.grep(&path)?,
                Mode::Files => {}
            }
        }
        Ok(())
    }

    fn push(&mut self, result: std::fmt::Arguments<'_>) {
        let start = self.output.len();
        let _ = writeln!(self.output, "{result}");
        if self.output.len() > MAX_OUTPUT_BYTES {
            self.output.truncate(start);
            self.results = MAX_RESULTS;
        } else {
            self.results += 1;
        }
    }

    fn grep(&mut self, path: &Path) -> io::Result<()> {
        let file = jail::open_in_repo(path, self.root)?;
        let size = file.metadata()?.len();
        if size > MAX_FILE_BYTES || size >= self.bytes_left {
            self.incomplete = true;
            return Ok(());
        }
        let mut bytes = Vec::new();
        let read = file.take(size + 1).read_to_end(&mut bytes)?;
        self.bytes_left -= read as u64;
        if let Ok(text) = std::str::from_utf8(&bytes)
            && !text.contains('\0')
        {
            for (index, line) in text.lines().enumerate() {
                if line.contains(self.query) {
                    let preview: String = line.chars().take(240).collect();
                    self.push(format_args!("{}:{}: {preview}", path.display(), index + 1));
                    if self.full() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}
