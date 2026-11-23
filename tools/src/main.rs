//! `cs-tools`: the CS core tools as one multi-call binary. Each tool matches the historical exec
//! contract: JSON args on stdin, the workspace as cwd, the observation on stdout/stderr.
//! `yaml-json FILE` is not a model-facing tool but an ops helper: Ruby-compatible YAML-to-JSON output.
//!
//! The tool is chosen by the basename of argv[0] (an install symlink named `fs_read`) or, for
//! `cs-tools` itself, by the first argument (`cs-tools fs_read`).

mod calllog;
mod difflib;
mod fpconv;
mod fs_edit;
mod fs_read;
mod jail;
mod node_install_lock;
mod okf;
mod py;
mod pyjson;
mod repo_search;
mod yaml_json;

use py::Failure;
use std::env;
use std::ffi::OsString;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::process::ExitCode;

/// The tools this binary carries.
#[derive(Clone, Copy)]
enum Tool {
    FsRead,
    RepoSearch,
    FsEdit,
    Okf,
    YamlJson,
}

impl Tool {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "repo_search" => Some(Self::RepoSearch),
            "fs_read" => Some(Self::FsRead),
            "fs_edit" => Some(Self::FsEdit),
            "okf" => Some(Self::Okf),
            "yaml-json" => Some(Self::YamlJson),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::RepoSearch => "repo_search",
            Self::FsRead => "fs_read",
            Self::FsEdit => "fs_edit",
            Self::Okf => "okf",
            Self::YamlJson => "yaml-json",
        }
    }
}

/// argv[0]'s basename wins; otherwise the first argument names the tool. Also the index of
/// the tool's own first argument.
fn select(argv: &[OsString]) -> Option<(Tool, usize)> {
    let invoked_as = argv
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
        .and_then(Tool::from_name);
    if let Some(tool) = invoked_as {
        return Some((tool, 1));
    }
    argv.get(1)
        .and_then(|arg| arg.to_str())
        .and_then(Tool::from_name)
        .map(|tool| (tool, 2))
}

/// Python's order: decode stdin, record the call, then run the tool.
fn run(tool: Tool, input: &[u8]) -> Result<Vec<u8>, Failure> {
    let args = pyjson::load(input)?;
    if !matches!(tool, Tool::RepoSearch) {
        calllog::record(tool.name(), &args);
    }
    match tool {
        Tool::RepoSearch => repo_search::run(&args),
        Tool::FsRead => fs_read::run(&args),
        Tool::FsEdit => fs_edit::run(&args),
        // main hands okf and yaml-json off before this point: no call log, their own exits.
        Tool::Okf | Tool::YamlJson => Ok(Vec::new()),
    }
}

/// okf's and yaml-json's contract: stdout and stderr may both carry output, no call log.
fn emit(stdout: &[u8], stderr: &[u8], code: u8) -> ExitCode {
    let mut out = io::stdout().lock();
    let _ = out.write_all(stdout);
    let _ = out.flush();
    let mut err = io::stderr().lock();
    let _ = err.write_all(stderr);
    let _ = err.flush();
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let argv: Vec<OsString> = env::args_os().collect();
    if argv.get(1).is_some_and(|arg| arg == "node-install-lock") {
        return match node_install_lock::run(&argv[2..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("node-install: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let Some((tool, first_arg)) = select(&argv) else {
        let _ = writeln!(
            io::stderr(),
            "usage: cs-tools fs_read|fs_edit|repo_search|okf < args.json, or cs-tools yaml-json FILE (or invoke through a symlink named for the tool)"
        );
        return ExitCode::from(2);
    };
    if let Tool::YamlJson = tool {
        let o = yaml_json::run(argv.get(first_arg).map(OsString::as_os_str));
        return emit(&o.stdout, &o.stderr, o.code);
    }
    let mut input = Vec::new();
    if let Err(e) = io::stdin().read_to_end(&mut input) {
        let _ = writeln!(io::stderr(), "OSError: {e}");
        return ExitCode::from(1);
    }
    if let Tool::Okf = tool {
        let o = okf::run(&input);
        return emit(&o.stdout, &o.stderr, o.code);
    }
    match run(tool, &input) {
        Ok(stdout) => {
            let mut out = io::stdout().lock();
            let _ = out.write_all(&stdout);
            let _ = out.flush();
            ExitCode::SUCCESS
        }
        Err(Failure(text)) => {
            let _ = writeln!(io::stderr(), "{text}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::{Tool, run, select};
    use std::ffi::OsString;

    #[test]
    fn select_resolves_tool_from_argv0_or_first_argument() {
        let via_name = vec![OsString::from("/tmp/fs_read"), OsString::from("ignored")];
        let picked = select(&via_name);
        assert!(matches!(picked, Some((Tool::FsRead, 1))));
        let via_arg = vec![OsString::from("cs-tools"), OsString::from("fs_edit")];
        assert!(matches!(select(&via_arg), Some((Tool::FsEdit, 2))));
        assert!(Tool::from_name("nope").is_none());
        assert_eq!(Tool::YamlJson.name(), "yaml-json");
        assert_eq!(Tool::Okf.name(), "okf");
        assert_eq!(Tool::FsEdit.name(), "fs_edit");
        let okf_argv = vec![OsString::from("cs-tools"), OsString::from("okf")];
        assert!(matches!(select(&okf_argv), Some((Tool::Okf, 2))));
    }

    #[test]
    fn run_returns_empty_for_tools_main_handles_before_pyjson_path() {
        match run(Tool::Okf, b"{}") {
            Ok(empty) => assert_eq!(empty, [] as [u8; 0]),
            Err(failure) => panic!("okf arm is a no-op inside run: {}", failure.0),
        }
        match run(Tool::YamlJson, b"{}") {
            Ok(empty) => assert_eq!(empty, [] as [u8; 0]),
            Err(failure) => panic!("yaml-json arm is a no-op inside run: {}", failure.0),
        }
    }
}
