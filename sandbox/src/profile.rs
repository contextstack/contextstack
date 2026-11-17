//! Named profiles: what a class of tool needs beyond paths and a network word.
//! A descriptor asks for one with `"profile"`; the host grants it only by listing it under
//! `profiles`, with the host-specific read paths. The rules each profile adds are fixed here, found
//! empirically, not configurable: the host decides whether, never how much.
//!
//! `browser` (gstack browse driving Playwright's headless Chromium):
//! - Chromium's browser process registers `org.chromium.Chromium.MachPortRendezvousServer.<pid>`
//!   and every child looks it up; without both, the browser aborts (`bootstrap_check_in`) or the
//!   children exit (`bootstrap_look_up ... Permission denied`).
//! - Bun's `realpathSync` opens every directory it resolves, and browse resolves each component
//!   of an `--out` path, so the ancestors of the call's tmp dir get `file-read-data` (a directory
//!   listing of those directories only, nothing under them).
//! - Chromium's own sandbox cannot start: Seatbelt refuses a second profile under ours
//!   (`sandbox initialization failed: Operation not permitted`), and bwrap refuses the nested user
//!   namespace Chromium needs on Linux. The profile sets `GSTACK_CHROMIUM_NO_SANDBOX=1`, so browse
//!   launches Chromium with `--no-sandbox`, and this wrapper is the only sandbox around the browser,
//!   renderers included. That is a security decision recorded in the README.
//! - Linux: the home directory is not mounted, so Chromium cannot create its crashpad database
//!   and traps at start (`chrome_crashpad_handler: --database is required`); the profile points
//!   `XDG_CONFIG_HOME` into the call's tmp dir.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A named profile a descriptor may request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Headless Chromium through gstack browse.
    Browser,
    /// A coding-agent CLI run under the executor.
    #[serde(rename = "cli-executor")]
    CliExecutor,
}

impl Profile {
    /// The policy word for this profile.
    pub fn word(self) -> &'static str {
        match self {
            Profile::Browser => "browser",
            Profile::CliExecutor => "cli-executor",
        }
    }

    /// Variables the wrapper adds to the tool's environment when the profile is granted;
    /// `tmp` is the call's tmp dir, `os` a `std::env::consts::OS` value.
    pub fn env(self, tmp: &Path, os: &str) -> Vec<(&'static str, OsString)> {
        match self {
            Profile::CliExecutor => Vec::new(),
            Profile::Browser => {
                let mut env = vec![("GSTACK_CHROMIUM_NO_SANDBOX", OsString::from("1"))];
                if os == "linux" {
                    env.push(("XDG_CONFIG_HOME", tmp.join("xdg-config").into_os_string()));
                }
                env
            }
        }
    }

    /// Seatbelt rules the profile adds; `tmp` is the call's canonical tmp dir.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn seatbelt_rules(
        self,
        tmp: &Path,
        quoted: impl Fn(&Path) -> Result<String, crate::failure::Failure>,
    ) -> Result<Vec<String>, crate::failure::Failure> {
        match self {
            Profile::CliExecutor => Ok(Vec::new()),
            Profile::Browser => {
                let rendezvous = "\"org.chromium.Chromium.MachPortRendezvousServer.\"";
                let mut rules = vec![
                    "; profile browser (Chromium's own sandbox is off; see cs-sandbox README)"
                        .to_owned(),
                    format!("(allow mach-register (global-name-prefix {rendezvous}))"),
                    format!("(allow mach-lookup (global-name-prefix {rendezvous}))"),
                ];
                rules.extend(
                    strict_ancestors(tmp)
                        .iter()
                        .map(|dir| Ok(format!("(allow file-read-data (literal {}))", quoted(dir)?)))
                        .collect::<Result<Vec<_>, _>>()?,
                );
                Ok(rules)
            }
        }
    }
}

/// Every ancestor of `path` except `/`, nearest first.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn strict_ancestors(path: &Path) -> Vec<PathBuf> {
    path.ancestors()
        .skip(1)
        .filter(|dir| dir.parent().is_some())
        .map(Path::to_path_buf)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_rules_cover_the_rendezvous_and_the_tmp_ancestors_only()
    -> Result<(), crate::failure::Failure> {
        let rules = Profile::Browser
            .seatbelt_rules(Path::new("/private/var/T/cs-sandbox-1"), |p| {
                Ok(format!("\"{}\"", p.display()))
            })?;
        assert_eq!(
            rules[1..],
            [
                "(allow mach-register (global-name-prefix \"org.chromium.Chromium.MachPortRendezvousServer.\"))",
                "(allow mach-lookup (global-name-prefix \"org.chromium.Chromium.MachPortRendezvousServer.\"))",
                "(allow file-read-data (literal \"/private/var/T\"))",
                "(allow file-read-data (literal \"/private/var\"))",
                "(allow file-read-data (literal \"/private\"))",
            ]
        );
        Ok(())
    }

    #[test]
    fn browser_turns_chromium_sandbox_off_and_parses_by_name() {
        let tmp = Path::new("/tmp/cs-sandbox-1");
        let off = ("GSTACK_CHROMIUM_NO_SANDBOX", OsString::from("1"));
        assert_eq!(
            Profile::Browser.env(tmp, "macos"),
            std::slice::from_ref(&off)
        );
        let config = (
            "XDG_CONFIG_HOME",
            OsString::from("/tmp/cs-sandbox-1/xdg-config"),
        );
        assert_eq!(Profile::Browser.env(tmp, "linux"), [off, config]);
        let parsed: Result<Profile, _> = serde_json::from_str("\"browser\"");
        assert_eq!(parsed.ok(), Some(Profile::Browser));
        assert_eq!(
            serde_json::from_str::<Profile>("\"cli-executor\"").ok(),
            Some(Profile::CliExecutor)
        );
    }
}
