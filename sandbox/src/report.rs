//! The per-call report written under `--report-dir`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::policy::{Effective, Narrowing};

/// What one call did, as JSON.
#[derive(Debug, Serialize)]
pub struct Report<'a> {
    /// Always `seatbelt` on macOS.
    pub backend: &'static str,
    /// The tool as resolved, an absolute path.
    pub tool: String,
    /// The words after TOOL, so a runtime exe descriptor's call (one binary, several tools) is named.
    pub args: &'a [String],
    /// The canonical workspace.
    pub cwd: &'a Path,
    /// The policy in force.
    pub effective_policy: &'a Effective,
    /// SHA-256 of the profile text handed to `sandbox-exec`.
    pub profile_sha256: String,
    /// Every request the host policy narrowed.
    pub narrowed: &'a [Narrowing],
    /// The tool's exit code, when it exited.
    pub exit: Option<i32>,
    /// The signal that killed the tool, when one did.
    pub signal: Option<i32>,
    /// Wall-clock milliseconds from spawn to exit.
    pub duration_ms: u64,
}

/// Milliseconds since the Unix epoch, for the file name.
pub fn unix_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// Write `DIR/<unix-ms>-<pid>.json`; the error text is for one stderr line, never an exit code.
pub fn write(dir: &Path, pid: u32, report: &Report<'_>) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let path = dir.join(format!("{}-{pid}.json", unix_ms()));
    let mut body = serde_json::to_vec_pretty(report).map_err(|error| error.to_string())?;
    body.push(b'\n');
    std::fs::write(&path, body).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Net;

    #[test]
    fn report_file_has_the_contract_fields() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let effective = Effective {
            developer_dir: None,
            workspace: PathBuf::from("/ws"),
            read: vec![PathBuf::from("/ws")],
            write: vec![PathBuf::from("/ws")],
            net: Net::None,
            tmp: PathBuf::from("/private/tmp/cs-sandbox-1-2"),
            write_env: std::collections::BTreeMap::new(),
            profile: None,
            sockets: std::collections::BTreeMap::new(),
        };
        let narrowed = vec![Narrowing {
            capability: "net".to_owned(),
            requested: "all".to_owned(),
            granted: "none".to_owned(),
            reason: "host allow_net does not list all".to_owned(),
        }];
        let report = Report {
            backend: "seatbelt",
            tool: "/bin/echo".to_owned(),
            args: &["hi".to_owned()],
            cwd: Path::new("/ws"),
            effective_policy: &effective,
            profile_sha256: "ab".repeat(32),
            narrowed: &narrowed,
            exit: Some(0),
            signal: None,
            duration_ms: 7,
        };
        let path = write(&dir.path().join("nested"), 4242, &report)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(name.ends_with("-4242.json"), "{name}");
        let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| error.to_string())?;
        for key in [
            "backend",
            "tool",
            "args",
            "cwd",
            "effective_policy",
            "profile_sha256",
            "narrowed",
            "exit",
            "signal",
            "duration_ms",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert_eq!(value["narrowed"][0]["requested"], "all");
        assert_eq!(value["args"][0], "hi");
        assert_eq!(value["effective_policy"]["net"], "none");
        assert_eq!(
            value["effective_policy"]["tmp"],
            "/private/tmp/cs-sandbox-1-2"
        );
        Ok(())
    }

    #[test]
    fn unwritable_dir_is_an_error_text_not_a_panic() {
        let effective = Effective {
            developer_dir: None,
            workspace: PathBuf::from("/ws"),
            read: vec![],
            write: vec![],
            net: Net::None,
            tmp: PathBuf::from("/private/tmp/cs-sandbox-1-2"),
            write_env: std::collections::BTreeMap::new(),
            profile: None,
            sockets: std::collections::BTreeMap::new(),
        };
        let report = Report {
            backend: "seatbelt",
            tool: String::new(),
            args: &[],
            cwd: Path::new("/ws"),
            effective_policy: &effective,
            profile_sha256: String::new(),
            narrowed: &[],
            exit: None,
            signal: Some(9),
            duration_ms: 0,
        };
        assert!(write(Path::new("/dev/null/reports"), 1, &report).is_err());
    }
}
