//! `fs_read`: read a file (offset/limit window, 2000 lines / 50 KB truncation) or list a
//! directory.

use crate::fs_edit;
use crate::jail;
use crate::py::{self, Failure};
use serde_json::Value;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

/// Run the tool on decoded args; `Ok` is stdout (exit 0).
pub fn run(a: &Value) -> Result<Vec<u8>, Failure> {
    let p = py::getitem(a, "path")
        .and_then(py::fspath)
        .map_err(|error| {
            Failure::exit(format!(
                "{}. Pass path as a string naming a workspace file or directory.",
                error.0
            ))
        })?;
    let path = Path::new(p);
    let repo = jail::repo_view();
    let with_path_help = |failure: Failure| {
        let help = repo.as_ref().map_or_else(
            || "Use a path relative to the workspace.".to_owned(),
            |root| format!("Relative paths use the workspace. For snapshot files use an absolute path under {} or repo_search.", root.display()),
        );
        Failure::exit(format!("{}. {help}", failure.0))
    };
    let escape = || with_path_help(Failure::exit(format!("path escapes workspace: {p}")));
    if jail::lexically_escapes(p, repo.as_deref())
        || jail::read_resolves_outside(path, repo.as_deref())
    {
        return Err(escape());
    }
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) {
                return Err(escape());
            }
            return Err(with_path_help(Failure::exit(format!(
                "not a file: {p} (path does not exist)"
            ))));
        }
        Err(e) => {
            if fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) {
                return Err(escape());
            }
            return Err(with_path_help(py::os_error(&e, p)));
        }
    };
    if !meta.is_dir() && !meta.is_file() {
        return Err(Failure::exit(format!(
            "not a file: {p}. Select a regular file or directory."
        )));
    }
    let result = if meta.is_dir() {
        list(a, p)
    } else {
        let file = jail::open_for_read(path).map_err(|e| with_path_help(open_error(&e, p)))?;
        read_file(a, p, file).map(String::into_bytes)
    };
    result.map_err(|failure| Failure::exit(format!("{}. Path {p:?} exists.", failure.0)))
}

fn open_error(error: &io::Error, path: &str) -> Failure {
    if error.kind() == io::ErrorKind::PermissionDenied
        && error.to_string().contains("path escapes workspace")
    {
        return Failure::exit(format!("path escapes workspace: {path}"));
    }
    if error.kind() == io::ErrorKind::PermissionDenied {
        return Failure::exit(format!("cannot open {path} safely: {error}"));
    }
    if error.kind() == io::ErrorKind::InvalidInput && error.to_string() == "not a regular file" {
        return Failure::exit(format!("not a file: {path}"));
    }
    py::os_error(error, path)
}

/// One name per line, sorted as Python sorts `os.listdir`, directories marked with `/`.
fn list(a: &Value, p: &str) -> Result<Vec<u8>, Failure> {
    let mut names = read_names(p).map_err(|e| {
        let detail = py::os_error_text(&e, p);
        Failure::exit(format!("cannot list directory {p}: {detail}"))
    })?;
    names.sort_by_cached_key(|name| sort_key(name.as_bytes()));
    if names.is_empty() {
        return Ok(b"(empty directory)\n".to_vec());
    }
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for name in &names {
        let mut row = name.as_bytes().to_vec();
        if Path::new(p).join(name).is_dir() {
            row.push(b'/');
        }
        lines.push(row);
    }
    format_list(a, &lines, lines.len())
}

fn format_list(a: &Value, rows: &[Vec<u8>], total: usize) -> Result<Vec<u8>, Failure> {
    let off = py::get(a, "offset")?;
    let start = match off {
        Some(v) if py::truthy(v) => usize::try_from((py::int(v)? - 1).max(0)).unwrap_or(0),
        _ => 0,
    };
    if start >= total {
        let shown = off.map_or_else(|| "None".to_owned(), py::str_of);
        return Err(Failure::exit(format!(
            "Offset {shown} is beyond end of file ({total} lines total)"
        )));
    }
    let user_lim = match py::get(a, "limit")? {
        Some(v) if !v.is_null() => Some(py::int(v)?),
        _ => None,
    };
    let from = start.min(total);
    let mut window = &rows[from..];
    if let Some(lim) = user_lim {
        window = &window[..slice_end(window.len(), lim)];
    }
    let (shown, truncated, huge) = take_bytes(window);
    if huge {
        return Err(Failure::exit(format!(
            "lines {}-{} of {total} — re-read with offset (line exceeds 50KB)",
            start + 1,
            start + 1
        )));
    }
    let mut body = Vec::new();
    for row in &window[..shown] {
        body.extend_from_slice(row);
        body.push(b'\n');
    }
    let start_disp = start + 1;
    let end_disp = start + shown;
    if truncated || end_disp < total {
        let next = end_disp + 1;
        let mut output =
            format!("lines {start_disp}-{end_disp} of {total} — re-read with offset={next}\n\n")
                .into_bytes();
        output.extend(body);
        return Ok(output);
    }
    Ok(body)
}

fn read_names(p: &str) -> std::io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(p)? {
        names.push(entry?.file_name());
    }
    Ok(names)
}

/// Python's sort order for a name decoded with surrogateescape: code points, with each
/// undecodable byte standing for U+DC00 + byte.
fn sort_key(name: &[u8]) -> Vec<u32> {
    let mut key = Vec::with_capacity(name.len());
    for chunk in name.utf8_chunks() {
        key.extend(chunk.valid().chars().map(u32::from));
        key.extend(chunk.invalid().iter().map(|b| 0xDC00 + u32::from(*b)));
    }
    key
}

fn utf8_line_error(path: &str, bytes: &[u8], error: &std::str::Utf8Error) -> Failure {
    let mut failure = fs_edit::decode_error(bytes, error);
    let _ = write!(
        failure.0,
        ". {path} looks binary; fs_read only handles UTF-8 text"
    );
    failure
}

/// Count only for negative limits, whose Python slicing semantics depend on EOF.
#[allow(clippy::naive_bytecount)] // Fixed-size buffers; no extra dependency for counting newlines.
fn count_lines(reader: &mut BufReader<fs::File>, path: &str) -> Result<usize, Failure> {
    let mut total = 1;
    loop {
        let buf = reader.fill_buf().map_err(|e| py::os_error(&e, path))?;
        if buf.is_empty() {
            break;
        }
        total += buf.iter().filter(|&&b| b == b'\n').count();
        let len = buf.len();
        reader.consume(len);
    }
    reader.rewind().map_err(|e| py::os_error(&e, path))?;
    Ok(total)
}

/// Read or skip one line with bounded storage, even if it has no newline.
fn bounded_line(
    reader: &mut BufReader<fs::File>,
    path: &str,
    skip: bool,
) -> Result<(Vec<u8>, bool), Failure> {
    let mut line = Vec::new();
    let read = reader
        .by_ref()
        .take((MAX_BYTES + 2) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|e| py::os_error(&e, path))?;
    let complete = line.last() == Some(&b'\n');
    if complete {
        line.pop();
    }
    if !complete && read > MAX_BYTES && skip {
        loop {
            let buf = reader.fill_buf().map_err(|e| py::os_error(&e, path))?;
            if buf.is_empty() {
                return Ok((line, true));
            }
            let end = buf.iter().position(|&b| b == b'\n');
            let len = end.map_or(buf.len(), |n| n + 1);
            reader.consume(len);
            if end.is_some() {
                return Ok((line, false));
            }
        }
    }
    Ok((line, !complete && read <= MAX_BYTES))
}

fn read_file(a: &Value, path: &str, file: fs::File) -> Result<String, Failure> {
    let off = py::get(a, "offset")?;
    let start = match off {
        Some(v) if py::truthy(v) => usize::try_from((py::int(v)? - 1).max(0)).unwrap_or(0),
        _ => 0,
    };
    let limit = match py::get(a, "limit")? {
        Some(v) if !v.is_null() => Some(py::int(v)?),
        _ => None,
    };
    let small = file.metadata().map_err(|e| py::os_error(&e, path))?.len() <= MAX_BYTES as u64;
    let mut reader = BufReader::new(file);
    let known_total = if small || limit.is_some_and(|n| n < 0) {
        Some(count_lines(&mut reader, path)?)
    } else {
        None
    };
    let requested = limit.map_or(usize::MAX, |n| {
        if n < 0 {
            slice_end(known_total.unwrap_or(0).saturating_sub(start), n)
        } else {
            usize::try_from(n).unwrap_or(usize::MAX)
        }
    });
    let mut lines = Vec::new();
    let mut used = 0;
    let mut total = 0;
    let mut eof;
    loop {
        let (line, at_end) = bounded_line(&mut reader, path, total < start)?;
        eof = at_end;
        if total >= start && line.len() > MAX_BYTES {
            if lines.is_empty() {
                return Err(Failure::exit(format!(
                    "line exceeds 50KB; re-read with offset={}",
                    total + 2
                )));
            }
            total += 1;
            break;
        }
        total += 1;
        if total > start {
            let text = std::str::from_utf8(&line).map_err(|e| utf8_line_error(path, &line, &e))?;
            let extra = line.len() + usize::from(!lines.is_empty());
            if lines.len() >= requested || lines.len() >= MAX_LINES || used + extra > MAX_BYTES {
                break;
            }
            used += extra;
            lines.push(text.to_owned());
        }
        if eof {
            break;
        }
    }
    let total_label = known_total.map_or_else(
        || {
            if eof {
                total.to_string()
            } else {
                format!("at least {total}")
            }
        },
        |n| n.to_string(),
    );
    if start >= total {
        let shown = off.map_or_else(|| "None".to_owned(), py::str_of);
        return Err(Failure::exit(format!(
            "Offset {shown} is beyond end of file ({total_label} lines total)"
        )));
    }
    let end = start + lines.len();
    let body = lines.join("\n");
    if !eof || known_total.is_some_and(|n| end < n) {
        return Ok(format!(
            "lines {}-{end} of {total_label} — re-read with offset={}\n\n{body}",
            start + 1,
            end + 1
        ));
    }
    Ok(body)
}

#[cfg(test)]
fn read(a: &Value, all_lines: &[&str]) -> Result<String, Failure> {
    use std::io::Write as _;
    let mut file = tempfile::tempfile().unwrap_or_else(|e| panic!("tempfile: {e}"));
    for (i, line) in all_lines.iter().enumerate() {
        if i > 0 {
            file.write_all(b"\n")
                .unwrap_or_else(|e| panic!("write: {e}"));
        }
        file.write_all(line.as_bytes())
            .unwrap_or_else(|e| panic!("write: {e}"));
    }
    file.rewind().unwrap_or_else(|e| panic!("rewind: {e}"));
    read_file(a, "test", file)
}

fn to_i128(n: usize) -> i128 {
    i128::try_from(n).unwrap_or(i128::MAX)
}

/// The end index Python's `seq[:lim]` keeps, negative limits counting from the end.
fn slice_end(len: usize, lim: i128) -> usize {
    let len_i = to_i128(len);
    let end = if lim < 0 {
        (len_i + lim).max(0)
    } else {
        lim.min(len_i)
    };
    usize::try_from(end).unwrap_or(0)
}

/// How many lines fit, whether output was cut, and whether the first line alone is too big.
fn take_bytes(lines: &[Vec<u8>]) -> (usize, bool, bool) {
    let Some(first) = lines.first() else {
        return (0, false, false);
    };
    if first.len() > MAX_BYTES {
        return (0, true, true);
    }
    let mut used = 0;
    for (i, line) in lines.iter().enumerate() {
        let extra = line.len() + 1;
        if i >= MAX_LINES || used + extra > MAX_BYTES {
            return (i, true, false);
        }
        used += extra;
    }
    (lines.len(), false, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jail::CwdRestore;
    use serde_json::json;
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    const FOUR: [&str; 4] = ["alpha", "beta", "gamma", ""];

    #[test]
    fn whole_file_has_no_header() {
        assert_eq!(
            read(&json!({"path": "a"}), &FOUR),
            Ok("alpha\nbeta\ngamma\n".to_owned())
        );
    }

    #[test]
    fn fixture_window() {
        let got = read(&json!({"path": "a", "offset": 2, "limit": 1}), &FOUR);
        assert_eq!(
            got,
            Ok("lines 2-2 of 4 — re-read with offset=3\n\nbeta".to_owned())
        );
    }

    #[test]
    fn offset_beyond_end_prints_python_str() {
        let got = read(&json!({"path": "a", "offset": 9.0}), &FOUR);
        assert_eq!(
            got,
            Err(Failure::exit(
                "Offset 9.0 is beyond end of file (4 lines total)"
            ))
        );
    }

    #[test]
    fn negative_limit_slices_from_the_end() {
        let got = read(&json!({"path": "a", "limit": -1}), &FOUR);
        assert_eq!(
            got,
            Ok("lines 1-3 of 4 — re-read with offset=4\n\nalpha\nbeta\ngamma".to_owned())
        );
    }

    #[test]
    fn read_reports_byte_cap_truncation_inside_requested_window() {
        let lines: Vec<String> = (0..200).map(|_| "z".repeat(400)).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let got = match read(&json!({"path": "w.bin"}), &refs) {
            Ok(text) => text,
            Err(failure) => panic!("truncated read: {}", failure.0),
        };
        assert!(got.contains("re-read with offset="));
        assert!(got.contains(" of at least "));
    }

    #[test]
    fn truncates_at_line_and_byte_caps() {
        let lines: Vec<Vec<u8>> = (0..2500).map(|i| i.to_string().into_bytes()).collect();
        assert_eq!(take_bytes(&lines), (2000, true, false));
        let wide = vec![b'x'; 30 * 1024];
        assert_eq!(take_bytes(&[wide.clone(), wide]), (1, true, false));
        let huge_line = vec![b'x'; MAX_BYTES + 1];
        assert_eq!(take_bytes(&[huge_line]), (0, true, true));
    }

    #[test]
    fn list_reports_directory_read_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let mut perms = fs::metadata(&locked)
            .unwrap_or_else(|e| panic!("meta: {e}"))
            .permissions();
        perms.set_mode(0o0);
        fs::set_permissions(&locked, perms).unwrap_or_else(|e| panic!("chmod: {e}"));
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        let _cwd = CwdRestore::chdir(&root);
        let err = list(&json!({"path": "locked"}), "locked");
        match err {
            Err(failure) => assert!(failure.0.contains("cannot list directory")),
            Ok(_) => panic!("expected list failure on unreadable directory"),
        }
    }

    #[test]
    fn read_window_includes_truncation_header_when_limit_not_reached() {
        let lines: Vec<&str> = (0..10).map(|_| "line").collect();
        let got = match read(&json!({"path": "x", "offset": 1, "limit": 3}), &lines) {
            Ok(text) => text,
            Err(failure) => panic!("read window: {}", failure.0),
        };
        assert!(got.contains("offset=4"));
    }

    #[test]
    fn take_bytes_on_empty_input_returns_zero_lines() {
        assert_eq!(take_bytes(&[]), (0, false, false));
    }

    #[test]
    fn sort_key_orders_like_python_surrogateescape() {
        let mut names: Vec<&[u8]> = vec![b"\xff", "\u{e000}".as_bytes(), b"B", b"a"];
        names.sort_by_key(|n| sort_key(n));
        assert_eq!(
            names,
            vec![b"B" as &[u8], b"a", b"\xff", "\u{e000}".as_bytes()]
        );
    }

    #[test]
    fn invalid_utf8_is_refused_as_binary() {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("bad.bin");
        fs::write(&path, [0xff, b'a']).unwrap_or_else(|e| panic!("write: {e}"));
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        let _cwd = CwdRestore::chdir(&root);
        let err = run(&json!({"path": "bad.bin"}))
            .err()
            .unwrap_or_else(|| panic!("expected error"));
        assert!(err.0.contains("UnicodeDecodeError"));
        assert!(err.0.contains("looks binary"));
    }

    #[test]
    fn huge_file_output_is_capped_without_loading_every_line() {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let path = dir.path().join("big.txt");
        let mut file = fs::File::create(&path).unwrap_or_else(|e| panic!("create: {e}"));
        let chunk = format!("{}\n", "y".repeat(1024));
        for _ in 0..20_000 {
            file.write_all(chunk.as_bytes())
                .unwrap_or_else(|e| panic!("write: {e}"));
        }
        drop(file);
        let mut file = fs::File::open(&path).unwrap_or_else(|e| panic!("open: {e}"));
        let clone = file.try_clone().unwrap_or_else(|e| panic!("clone: {e}"));
        let text =
            read_file(&json!({}), "big.txt", clone).unwrap_or_else(|e| panic!("read: {}", e.0));
        assert!(text.contains("re-read with offset="));
        assert!(text.len() <= MAX_BYTES + 256);
        // Clones share the descriptor offset. Even with BufReader read-ahead,
        // less than 100 KB of this 20 MB file should have been consumed.
        assert!(
            file.stream_position()
                .unwrap_or_else(|e| panic!("position: {e}"))
                < (MAX_BYTES * 2) as u64
        );
    }

    #[test]
    fn huge_unbroken_line_is_bounded_and_skippable() {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("huge");
        let mut file = fs::File::create(&path).unwrap_or_else(|e| panic!("{e}"));
        let chunk = [b'x'; 8192];
        for _ in 0..2560 {
            file.write_all(&chunk).unwrap_or_else(|e| panic!("{e}"));
        }
        file.write_all(b"\ntail\n")
            .unwrap_or_else(|e| panic!("{e}"));
        drop(file);
        let open = || fs::File::open(&path).unwrap_or_else(|e| panic!("{e}"));
        let err = read_file(&json!({}), "huge", open())
            .err()
            .unwrap_or_else(|| panic!("expected oversized line"));
        assert!(err.0.contains("line exceeds 50KB"));
        assert_eq!(
            read_file(&json!({"offset": 2}), "huge", open()),
            Ok("tail\n".into())
        );
    }

    #[test]
    fn list_truncates_large_directories() {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let list_dir = dir.path().join("many");
        fs::create_dir(&list_dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
        for i in 0..3500 {
            let name = format!("entry-{i:05}.txt");
            fs::write(list_dir.join(&name), b"x").unwrap_or_else(|e| panic!("write: {e}"));
        }
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        let _cwd = CwdRestore::chdir(&root);
        let out = run(&json!({"path": "many"})).unwrap_or_else(|e| panic!("list: {}", e.0));
        let text = String::from_utf8(out).unwrap_or_else(|e| panic!("utf8: {e}"));
        assert!(text.contains("re-read with offset="));
        assert!(text.contains(" of 3500 "));
    }

    #[test]
    fn final_symlink_to_outside_is_refused_even_after_check() {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let outside = TempDir::new().unwrap_or_else(|e| panic!("outside: {e}"));
        fs::write(outside.path().join("secret.txt"), b"NO\n")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let link = dir.path().join("trap.txt");
        symlink(outside.path().join("secret.txt"), &link)
            .unwrap_or_else(|e| panic!("symlink: {e}"));
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        let _cwd = CwdRestore::chdir(&root);
        let err = run(&json!({"path": "trap.txt"}))
            .err()
            .unwrap_or_else(|| panic!("expected error"));
        assert!(err.0.starts_with("path escapes workspace: "));
    }

    #[test]
    fn in_workspace_symlink_still_reads() {
        let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        fs::write(dir.path().join("body.txt"), b"via-link\n")
            .unwrap_or_else(|e| panic!("write: {e}"));
        symlink("body.txt", dir.path().join("alias.txt"))
            .unwrap_or_else(|e| panic!("symlink: {e}"));
        let root = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|e| panic!("canon: {e}"));
        let _cwd = CwdRestore::chdir(&root);
        let out = run(&json!({"path": "alias.txt"})).unwrap_or_else(|e| panic!("read: {}", e.0));
        assert_eq!(
            String::from_utf8(out).unwrap_or_else(|e| panic!("utf8: {e}")),
            "via-link\n"
        );
    }
}
