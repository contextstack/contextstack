//! Command line: `--policy FILE [--cwd DIR] [--report-dir DIR] -- TOOL [ARGS...]`,
//! `--probe --policy FILE`, or `--policy FILE --profile cli-executor --request FILE|-`.

use std::path::PathBuf;

use crate::failure::{Failure, usage};

/// The usage line printed with every argument error.
pub const USAGE_LINE: &str = "usage: cs-sandbox --policy FILE [--cwd DIR] [--report-dir DIR] -- TOOL [ARGS...] | cs-sandbox --probe --policy FILE | cs-sandbox --policy FILE --profile cli-executor --request FILE|-";

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Report what this host can enforce.
    Probe {
        /// The host policy file, validated before the probe is printed.
        policy: PathBuf,
    },
    /// Print a coding-agent Seatbelt profile (macOS).
    EmitCliExecutor { policy: PathBuf, request: PathBuf },
    /// Run one tool under the policy.
    Run(Run),
}

/// Arguments of a tool run.
#[derive(Debug, PartialEq, Eq)]
pub struct Run {
    /// The host policy file.
    pub policy: PathBuf,
    /// The workspace; the current directory when absent.
    pub cwd: Option<PathBuf>,
    /// Where to write the per-call report; nothing is written when absent.
    pub report_dir: Option<PathBuf>,
    /// The tool, an absolute path, a relative path or a bare name on `PATH`.
    pub tool: String,
    /// The tool's own arguments.
    pub args: Vec<String>,
}

fn value(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<PathBuf, Failure> {
    match iter.next() {
        Some(text) if !text.is_empty() => Ok(PathBuf::from(text)),
        _ => Err(usage(format!("{flag} needs a value; {USAGE_LINE}"))),
    }
}

/// Parse the arguments after the program name.
pub fn parse(args: Vec<String>) -> Result<Command, Failure> {
    let mut policy = None;
    let mut cwd = None;
    let mut report_dir = None;
    let mut probe = false;
    let mut profile = None;
    let mut request = None;
    let mut tool_argv: Option<Vec<String>> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--policy" => policy = Some(value(&mut iter, "--policy")?),
            "--cwd" => cwd = Some(value(&mut iter, "--cwd")?),
            "--report-dir" => report_dir = Some(value(&mut iter, "--report-dir")?),
            "--profile" => {
                profile = Some(
                    iter.next()
                        .ok_or_else(|| usage(format!("--profile needs a value; {USAGE_LINE}")))?,
                );
            }
            "--request" => request = Some(value(&mut iter, "--request")?),
            "--probe" => probe = true,
            "--" => {
                tool_argv = Some(iter.by_ref().collect());
                break;
            }
            other => return Err(usage(format!("unknown argument {other}; {USAGE_LINE}"))),
        }
    }
    let Some(policy) = policy else {
        return Err(usage(format!("--policy FILE is required; {USAGE_LINE}")));
    };
    if probe {
        if tool_argv.is_some()
            || cwd.is_some()
            || report_dir.is_some()
            || profile.is_some()
            || request.is_some()
        {
            return Err(usage(format!("--probe takes only --policy; {USAGE_LINE}")));
        }
        return Ok(Command::Probe { policy });
    }
    if let Some(name) = profile {
        if name != "cli-executor" {
            return Err(usage(format!("unknown profile {name}; {USAGE_LINE}")));
        }
        let Some(request) = request else {
            return Err(usage(format!(
                "--profile cli-executor needs --request FILE|-; {USAGE_LINE}"
            )));
        };
        if tool_argv.is_some() || cwd.is_some() || report_dir.is_some() {
            return Err(usage(format!(
                "--profile cli-executor takes only --policy and --request; {USAGE_LINE}"
            )));
        }
        return Ok(Command::EmitCliExecutor { policy, request });
    }
    if request.is_some() {
        return Err(usage(format!(
            "--request requires --profile cli-executor; {USAGE_LINE}"
        )));
    }
    let Some(mut rest) = tool_argv else {
        return Err(usage(format!(
            "-- TOOL [ARGS...] is required; {USAGE_LINE}"
        )));
    };
    if rest.is_empty() {
        return Err(usage(format!("TOOL is missing after --; {USAGE_LINE}")));
    }
    let tool = rest.remove(0);
    Ok(Command::Run(Run {
        policy,
        cwd,
        report_dir,
        tool,
        args: rest,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn full_run_line_parses() -> Result<(), Failure> {
        let parsed = parse(words(
            "--policy p.json --cwd ws --report-dir r -- /bin/echo hi there",
        ))?;
        let expected = Command::Run(Run {
            policy: PathBuf::from("p.json"),
            cwd: Some(PathBuf::from("ws")),
            report_dir: Some(PathBuf::from("r")),
            tool: "/bin/echo".to_owned(),
            args: vec!["hi".to_owned(), "there".to_owned()],
        });
        assert_eq!(parsed, expected);
        Ok(())
    }

    #[test]
    fn tool_flags_after_separator_are_the_tools() -> Result<(), Failure> {
        let Command::Run(run) = parse(words("--policy p -- tool --cwd x"))? else {
            return Err(usage("expected a run"));
        };
        assert_eq!(run.args, vec!["--cwd".to_owned(), "x".to_owned()]);
        assert_eq!(run.cwd, None);
        Ok(())
    }

    #[test]
    fn cli_executor_emit_parses() -> Result<(), Failure> {
        let parsed = parse(words(
            "--policy p.json --profile cli-executor --request req.json",
        ))?;
        assert_eq!(
            parsed,
            Command::EmitCliExecutor {
                policy: PathBuf::from("p.json"),
                request: PathBuf::from("req.json"),
            }
        );
        Ok(())
    }

    #[test]
    fn stdin_request_parses() -> Result<(), Failure> {
        assert_eq!(
            parse(words("--policy p --profile cli-executor --request -"))?,
            Command::EmitCliExecutor {
                policy: PathBuf::from("p"),
                request: PathBuf::from("-"),
            }
        );
        Ok(())
    }

    #[test]
    fn probe_parses_and_refuses_extras() -> Result<(), Failure> {
        let probe = parse(words("--probe --policy p.json"))?;
        assert_eq!(
            probe,
            Command::Probe {
                policy: PathBuf::from("p.json")
            }
        );
        let extra = parse(words("--probe --policy p.json -- tool"));
        assert_eq!(extra.err().map(|failure| failure.code()), Some(2));
        Ok(())
    }

    #[test]
    fn usage_errors_exit_two() {
        for line in [
            "",
            "-- tool",
            "--policy",
            "--policy p",
            "--policy p --",
            "--policy p --bogus -- tool",
            "--policy p --cwd -- tool",
        ] {
            let failure = parse(words(line)).err();
            assert_eq!(failure.as_ref().map(Failure::code), Some(2), "{line:?}");
            let message = failure.map(|f| f.message().to_owned()).unwrap_or_default();
            assert!(message.contains("usage:"), "{line:?}: {message}");
        }
    }
}
