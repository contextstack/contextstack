//! `yaml-json FILE`: what `ruby -ryaml -rjson -e 'puts JSON.generate(YAML.safe_load(File.read(ARGV[0])))' FILE`
//! prints (Ruby 4.0, Psych 5.3.1, json 2.18.0), for the ops helpers that once shelled out to it
//! stdout is byte for byte; an uncaught Ruby exception is one stderr line,
//! `yaml-json: <message> (<Class>)`, where Ruby prints a backtrace whose first line ends the same
//! way; exit 1 either way. Not a model-facing tool: no call log, no descriptor.

use crate::fpconv;
use crate::okf::errno;
use crate::okf::psych::{self, Options};
use crate::okf::rb::{Raise, Val, json_str, to_s};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

/// json's default `max_nesting`.
const MAX_NESTING: usize = 100;

const BINARY_WARNING: &str = "warning: JSON.generate: UTF-8 string passed as BINARY, this will raise an encoding error in json 3.0";

/// What the process prints and its exit code.
pub struct Outcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: u8,
}

/// Run on `ARGV[0]` (`None` when absent, as Ruby's `File.read(nil)` sees it).
pub fn run(path: Option<&OsStr>) -> Outcome {
    let mut generator = Generator::default();
    let result = convert(path, &mut generator);
    let mut stderr = String::new();
    for warning in &generator.warnings {
        let _ = writeln!(stderr, "yaml-json: {warning}");
    }
    match result {
        Ok(json) => Outcome {
            stdout: format!("{json}\n").into_bytes(),
            stderr: stderr.into_bytes(),
            code: 0,
        },
        Err(r) => {
            let _ = writeln!(stderr, "yaml-json: {} ({})", r.message, r.class);
            Outcome {
                stdout: Vec::new(),
                stderr: stderr.into_bytes(),
                code: 1,
            }
        }
    }
}

fn convert(path: Option<&OsStr>, generator: &mut Generator) -> Result<String, Raise> {
    let Some(path) = path.map(Path::new) else {
        return Err(Raise::new(
            "TypeError",
            "no implicit conversion of nil into String",
        ));
    };
    let bytes = fs::read(path).map_err(|e| {
        let call = if e.kind() == io::ErrorKind::IsADirectory {
            "io_fread"
        } else {
            "rb_sysopen"
        };
        errno(&e, call, path)
    })?;
    match psych::load(&bytes, Options::BARE)? {
        // safe_load's fallback for an empty stream is nil.
        None => Ok("null".to_string()),
        Some(value) => {
            generator.value(&value)?;
            Ok(std::mem::take(&mut generator.out))
        }
    }
}

/// `JSON.generate` with json 2.18's default state.
#[derive(Default)]
struct Generator {
    out: String,
    depth: usize,
    warnings: Vec<String>,
}

fn generator_error(message: String) -> Raise {
    Raise::new("JSON::GeneratorError", message)
}

impl Generator {
    fn value(&mut self, v: &Val) -> Result<(), Raise> {
        match v {
            Val::Nil => self.out.push_str("null"),
            Val::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            Val::Int(i) => self.out.push_str(i),
            Val::Float(f) if !f.is_finite() => {
                let Val::Str(text) = to_s(v) else {
                    unreachable!("Float#to_s is a String")
                };
                return Err(generator_error(format!("{text} not allowed in JSON")));
            }
            Val::Float(f) => self.out.push_str(&fpconv::dtoa(*f)),
            Val::Str(s) => self.out.push_str(&json_str(s)),
            Val::Bin(b) => {
                let s = self.binary(b)?;
                self.out.push_str(&json_str(&s));
            }
            Val::Array(items) => {
                self.nest()?;
                self.out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.push(',');
                    }
                    self.value(item)?;
                }
                self.out.push(']');
                self.depth -= 1;
            }
            Val::Hash(pairs) => {
                self.nest()?;
                self.out.push('{');
                for (i, (k, item)) in pairs.iter().enumerate() {
                    if i > 0 {
                        self.out.push(',');
                    }
                    // A non-String key is its `to_s`, checked as any String is.
                    let key = match to_s(k) {
                        Val::Bin(b) => self.binary(&b)?,
                        Val::Str(s) => s,
                        _ => String::new(),
                    };
                    self.out.push_str(&json_str(&key));
                    self.out.push(':');
                    self.value(item)?;
                }
                self.out.push('}');
                self.depth -= 1;
            }
            // Generic objects generate as `to_s.to_json`. A safe_load without permitted
            // classes or aliases never yields these; kept total for the okf loader's values.
            Val::Date(_) | Val::Time(_) => {
                let Val::Str(s) = to_s(v) else {
                    unreachable!("Date#to_s and Time#to_s are Strings")
                };
                self.out.push_str(&json_str(&s));
            }
            Val::Recursive(_) => {
                self.depth = MAX_NESTING;
                self.nest()?;
            }
        }
        Ok(())
    }

    /// `increase_depth`.
    fn nest(&mut self) -> Result<(), Raise> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            self.depth -= 1;
            let message = format!(
                "nesting of {} is too deep. Did you try to serialize objects with circular references?",
                self.depth
            );
            return Err(Raise::new("JSON::NestingError", message));
        }
        Ok(())
    }

    /// `ensure_valid_encoding` for an ASCII-8BIT string: ASCII passes, valid UTF-8 passes
    /// with a warning, anything else fails `encode` at its first high byte.
    fn binary(&mut self, bytes: &[u8]) -> Result<String, Raise> {
        let Ok(s) = std::str::from_utf8(bytes) else {
            let high = bytes.iter().find(|b| **b >= 0x80).copied().unwrap_or(0);
            return Err(generator_error(format!(
                "\"\\x{high:02X}\" from ASCII-8BIT to UTF-8"
            )));
        };
        if !s.is_ascii() {
            self.warnings.push(BINARY_WARNING.to_string());
        }
        Ok(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn run_on(text: &[u8]) -> (String, String, u8) {
        let mut file = tempfile::NamedTempFile::new().unwrap_or_else(|e| panic!("temp file: {e}"));
        file.write_all(text)
            .unwrap_or_else(|e| panic!("write: {e}"));
        let o = run(Some(file.path().as_os_str()));
        (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            o.code,
        )
    }

    #[test]
    fn documents_print_as_ruby_prints_them() {
        let ok = |yaml: &str, json: &str| {
            assert_eq!(
                run_on(yaml.as_bytes()),
                (format!("{json}\n"), String::new(), 0)
            );
        };
        ok("", "null");
        ok("a: &x 1", "{\"a\":1}");
        ok("--- 1\n--- 2", "1");
        ok(
            "{1: a, 2.5: b, null: c, [1, 2]: e}",
            "{\"1\":\"a\",\"2.5\":\"b\",\"\":\"c\",\"[1, 2]\":\"e\"}",
        );
        ok(
            "[1.0e+16, 1.5e-7, 0x1F, 1_000, '2026-01-01']",
            "[1e+16,0.00000015,31,1000,\"2026-01-01\"]",
        );
    }

    #[test]
    fn refusals_name_rubys_exception() {
        let err = |yaml: &[u8], line: &str| {
            assert_eq!(
                run_on(yaml),
                (String::new(), format!("yaml-json: {line}\n"), 1)
            );
        };
        err(
            b"2026-02-30",
            "Tried to load unspecified class: Date (Psych::DisallowedClass)",
        );
        err(
            b"t: 2026-01-01 10:00:00",
            "Tried to load unspecified class: Time (Psych::DisallowedClass)",
        );
        err(
            b":sym",
            "Tried to load unspecified class: Symbol (Psych::DisallowedClass)",
        );
        err(
            b"a: &x 1\nb: *x",
            "Alias parsing was not enabled. To enable it, pass `aliases: true` to `Psych::load` or `Psych::safe_load`. (Psych::AliasesNotEnabled)",
        );
        err(b".nan", "NaN not allowed in JSON (JSON::GeneratorError)");
        err(
            b"!!binary /w==",
            "\"\\xFF\" from ASCII-8BIT to UTF-8 (JSON::GeneratorError)",
        );
        err(
            b"a: \xff",
            "(<unknown>): invalid leading UTF-8 octet at line 1 column 1 (Psych::SyntaxError)",
        );
    }
    #[test]
    fn run_on_directory_reports_ruby_open_error() {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let o = run(Some(dir.path().as_os_str()));
        assert_eq!(o.code, 1);
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(stderr.contains("yaml-json:"));
        assert!(stderr.contains("Is a directory") || stderr.contains("io_fread"));
    }

    #[test]
    fn run_with_missing_path_argument_reports_type_error() {
        let o = run(None);
        assert_eq!(o.code, 1);
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(stderr.contains("TypeError"));
        assert!(stderr.contains("no implicit conversion of nil"));
    }

    #[test]
    fn utf8_binary_scalar_emits_json_warning_and_encodes() {
        let (stdout, stderr, code) = run_on(b"text: !!binary 8J+SmQo=");
        assert_eq!(code, 0);
        assert!(stderr.contains(BINARY_WARNING));
        assert!(stdout.contains("text"));
    }

    #[test]
    fn recursive_value_reports_json_nesting_error() {
        use crate::okf::rb::Val;
        let mut generator = Generator::default();
        let Err(err) = generator.value(&Val::Recursive(true)) else {
            panic!("recursive Ruby value must not serialize to JSON");
        };
        assert_eq!(err.class, "JSON::NestingError");
        assert!(err.message.contains("nesting"));
    }
}

