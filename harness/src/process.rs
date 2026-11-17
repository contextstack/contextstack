//! Process boundary shared by tools, hooks, and trusted operations.
use rustix::process::{Pid, Signal, kill_process_group};
use std::{
    collections::BTreeMap,
    io,
    path::Path,
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::pipe::Receiver,
    process::Command,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use std::ffi::OsString;

#[cfg(any(test, feature = "fixtures"))]
use std::cell::RefCell;

#[cfg(any(test, feature = "fixtures"))]
thread_local! {
    static TEST_OVERLAY: RefCell<BTreeMap<String, String>> = const {
        RefCell::new(BTreeMap::new())
    };
}

#[cfg(any(test, feature = "fixtures"))]
fn overlay(name: &str) -> Option<String> {
    TEST_OVERLAY.with(|map| map.borrow().get(name).cloned())
}

fn read_var(name: &str) -> Option<String> {
    #[cfg(any(test, feature = "fixtures"))]
    if let Some(value) = overlay(name) {
        return Some(value);
    }
    std::env::var(name).ok()
}

/// Read an environment variable; tests may layer values without mutating the process environment.
pub fn var_os(name: &str) -> Option<OsString> {
    read_var(name)
        .map(OsString::from)
        .or_else(|| std::env::var_os(name))
}

/// Tests pin environment values for the current task without `set_var`.
#[cfg(any(test, feature = "fixtures"))]
pub struct ScopedEnv {
    previous: BTreeMap<String, String>,
}

#[cfg(any(test, feature = "fixtures"))]
impl ScopedEnv {
    pub fn set(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let previous = TEST_OVERLAY.with(|map| map.replace(vars.into_iter().collect()));
        Self { previous }
    }
}

#[cfg(any(test, feature = "fixtures"))]
impl Drop for ScopedEnv {
    fn drop(&mut self) {
        TEST_OVERLAY.with(|map| map.replace(std::mem::take(&mut self.previous)));
    }
}

const DEFAULT_ENV: &str = "PATH HOME USER LOGNAME TERM LANG LC_ALL LC_CTYPE TMPDIR TMP TEMP SHELL HOMEBREW_PREFIX HOMEBREW_CELLAR LAB_JOB_ID LAB_TRIAL LAB_SCHEDULE_DATE LAB_SCHEDULE_SLOT LAB_SCHEDULE_TIMEZONE LAB_VIDEO_MUTATION LAB_STEP_TTY LAB_CODE_ROOT LAB_CONFIG_ROOT LAB_STATE_ROOT LAB_WORKSPACES_ROOT CS_TOOL_PREFIX CS_SANDBOX_BIN LAB_BROWSER_STATE LAB_TOOL_TIMEOUT_SECONDS";

/// The names the launchers (tick, run-spec) keep identical under both spellings; every other LAB name,
/// such as the validated roots, is read only as spelled, so an inherited CS value cannot bypass the launcher.
const ALIASED: &str =
    "CHILD_ENV VIDEO_MUTATION JOB_ID TRIAL SCHEDULE_DATE SCHEDULE_SLOT SCHEDULE_TIMEZONE";

/// Prefer a CS alias, including an explicitly empty value, to its legacy LAB spelling.
pub fn env(name: &str) -> Option<String> {
    name.strip_prefix("LAB_")
        .filter(|suffix| ALIASED.split(' ').any(|aliased| aliased == *suffix))
        .and_then(|suffix| read_var(&format!("CS_{suffix}")))
        .or_else(|| read_var(name))
}

/// Run-level cancellation, what cancelled it, and the optional remaining wall-clock budget.
#[derive(Clone, Default)]
pub struct Control {
    pub deadline: Option<Instant>,
    pub cancel: CancellationToken,
    /// The signal that cancelled the run (terminal.json `cancel_reason`).
    pub cause: Arc<OnceLock<&'static str>>,
}

impl Control {
    /// Record the first cause, then stop the in-flight model call and kill the tool's process group.
    pub fn cancel_with(&self, cause: &'static str) {
        let _ = self.cause.set(cause);
        self.cancel.cancel();
    }

    pub fn reason(&self) -> Option<&'static str> {
        if self.cancel.is_cancelled() {
            Some("interrupted")
        } else if self.deadline.is_some_and(|d| Instant::now() >= d) {
            Some("budget:seconds")
        } else {
            None
        }
    }

    pub async fn stopped(&self) {
        if self.reason().is_some() {
            return;
        }
        tokio::select! {
            () = self.cancel.cancelled() => {},
            () = async { match self.deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }} => {},
        }
    }
}

/// Process result with Go-compatible exit wording at the observation boundary.
pub struct Output {
    pub bytes: Vec<u8>,
    pub code: i32,
    pub error: Option<String>,
}

fn environment(extra: &[(String, String)]) -> BTreeMap<String, String> {
    let names = env("LAB_CHILD_ENV").unwrap_or_else(|| DEFAULT_ENV.to_owned());
    let mut values: BTreeMap<_, _> = names
        .split_whitespace()
        .filter_map(|key| env(key).map(|v| (key.to_owned(), v)))
        .collect();
    values.extend(extra.iter().cloned());
    values
}

const TEXT_LIMIT: usize = 8192;
const CLIP_HEAD: usize = 3 * 1024;
const CLIP_TAIL: usize = 5 * 1024;
const IMAGE_LIMIT: usize = 16 * 1024 * 1024;
const EXEC_SPILL: &str = "full output in .lab-out/";

/// Select the stream and retained output needed by the caller.
pub enum Capture {
    /// Trusted operations read their full stdout reply.
    Stdout,
    /// Keep the caller’s group for the built-in terminal confirmation hook.
    Foreground,
    /// Text tools and unattended hooks run in their own process group.
    Text,
    /// Image observations need a complete JSON envelope, up to 16 MiB.
    Image,
}

async fn drain(reader: &mut Receiver, bytes: &mut Vec<u8>, limit: u64) -> io::Result<()> {
    reader.take(limit).read_to_end(bytes).await?;
    tokio::io::copy(reader, &mut tokio::io::sink())
        .await
        .map(|_| ())
}

fn stop_error(control: Option<&Control>) -> &'static str {
    match control.and_then(Control::reason) {
        Some("interrupted") => "context canceled",
        _ => "context deadline exceeded",
    }
}

/// Spawn directly and bound the child wait and both pipes by the same stop conditions.
#[allow(
    clippy::too_many_arguments,
    reason = "The single process boundary keeps launch semantics explicit."
)]
pub async fn command(
    bin: &Path,
    args: &[&str],
    cwd: &Path,
    input: &[u8],
    extra: &[(String, String)],
    control: Option<&Control>,
    timeout: Option<Duration>,
    capture: Capture,
) -> Result<Output, String> {
    if control.and_then(Control::reason).is_some() {
        return Err(stop_error(control).into());
    }
    let (reader, stdout) = io::pipe().map_err(|e| e.to_string())?;
    let mut reader = Receiver::from_owned_fd(reader.into()).map_err(|e| e.to_string())?;
    let stderr = if matches!(capture, Capture::Stdout) {
        Stdio::null()
    } else {
        Stdio::from(stdout.try_clone().map_err(|e| e.to_string())?)
    };
    let mut command = Command::new(bin);
    command.args(args);
    let isolated = !matches!(capture, Capture::Foreground);
    if isolated {
        command.process_group(0);
    }
    let mut child = command
        .current_dir(cwd)
        .env_clear()
        .envs(environment(extra))
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("fork/exec {}: {}", bin.display(), io_error(&e)))?;
    drop(command);
    let stdin = child.stdin.take();
    let writer = async move {
        if let Some(mut pipe) = stdin {
            let _ = pipe.write_all(input).await;
        }
        Ok(())
    };
    let limit = match capture {
        Capture::Stdout => u64::MAX,
        Capture::Foreground | Capture::Text => TEXT_LIMIT as u64 + 1,
        Capture::Image => IMAGE_LIMIT as u64 + 1,
    };
    let mut bytes = Vec::new();
    let result = tokio::select! {
        biased;
        () = async { match control { Some(c) => c.stopped().await, None => std::future::pending().await }} => None,
        () = async { match timeout { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await }} => None,
        result = async {
            // Keep the group leader's PID reserved until inherited pipes close.
            tokio::try_join!(drain(&mut reader, &mut bytes, limit), writer)?;
            child.wait().await
        } => Some(result),
    };
    let stopped = result.is_none();
    let status = match result {
        Some(Ok(status)) => Ok(status),
        interrupted => {
            if isolated
                && let Some(group) = child.id().and_then(|id| Pid::from_raw(id.cast_signed()))
            {
                let _ = kill_process_group(group, Signal::KILL);
            }
            let _ = child.start_kill();
            interrupted.unwrap_or(child.wait().await)
        }
    }
    .map_err(|e| e.to_string())?;
    let image_too_large = matches!(capture, Capture::Image) && bytes.len() > IMAGE_LIMIT;
    if image_too_large {
        bytes.clear();
    }
    let (code, error) = match status.code() {
        Some(0) if stopped => (-1, Some(stop_error(control).into())),
        Some(0) if image_too_large => (-1, Some(format!("output exceeds {IMAGE_LIMIT} bytes"))),
        Some(0) => (0, None),
        Some(code) => (code, Some(format!("exit status {code}"))),
        None => (-1, Some(signal_error(status))),
    };
    Ok(Output { bytes, code, error })
}

// Go syscall.Signal.String spellings by signal number on Linux and macOS.
const SIGNAL_NAMES: &[(i32, i32, &str)] = &[
    (1, 1, "hangup"),
    (2, 2, "interrupt"),
    (3, 3, "quit"),
    (4, 4, "illegal instruction"),
    (5, 0, "trace/breakpoint trap"),
    (6, 0, "aborted"),
    (7, 10, "bus error"),
    (8, 8, "floating point exception"),
    (9, 9, "killed"),
    (10, 30, "user defined signal 1"),
    (11, 11, "segmentation fault"),
    (12, 31, "user defined signal 2"),
    (13, 13, "broken pipe"),
    (14, 14, "alarm clock"),
    (15, 15, "terminated"),
    (16, 0, "stack fault"),
    (17, 20, "child exited"),
    (18, 19, "continued"),
    (19, 0, "stopped (signal)"),
    (20, 0, "stopped"),
    (21, 21, "stopped (tty input)"),
    (22, 22, "stopped (tty output)"),
    (23, 16, "urgent I/O condition"),
    (24, 0, "CPU time limit exceeded"),
    (25, 0, "file size limit exceeded"),
    (26, 26, "virtual timer expired"),
    (27, 27, "profiling timer expired"),
    (28, 0, "window changed"),
    (29, 23, "I/O possible"),
    (30, 0, "power failure"),
    (31, 12, "bad system call"),
    (0, 5, "trace/BPT trap"),
    (0, 6, "abort trap"),
    (0, 7, "EMT trap"),
    (0, 17, "suspended (signal)"),
    (0, 18, "suspended"),
    (0, 24, "cputime limit exceeded"),
    (0, 25, "filesize limit exceeded"),
    (0, 28, "window size changes"),
    (0, 29, "information request"),
];

fn signal_error(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    let signal = status.signal().unwrap_or_default();
    let name = SIGNAL_NAMES.iter().find_map(|&(linux, macos, name)| {
        let matches = (cfg!(target_os = "linux") && signal == linux)
            || (cfg!(target_os = "macos") && signal == macos);
        (signal != 0 && matches).then_some(name)
    });
    let mut error = name.map_or_else(
        || format!("signal: signal {signal}"),
        |name| format!("signal: {name}"),
    );
    if status.core_dumped() {
        error.push_str(" (core dumped)");
    }
    error
}

pub fn io_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        _ => error.to_string(),
    }
}

fn exec_spill_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .find(|line| line.contains(EXEC_SPILL))
        .map(str::to_owned)
}

fn clip_marker(total: usize, head: usize, tail: usize, spill: Option<&str>) -> String {
    let omitted = total.saturating_sub(head + tail);
    let summary = format!("[truncated: {total} bytes total, {omitted} omitted]");
    match spill {
        Some(line) => format!("{line}\n{summary}"),
        None => summary,
    }
}

pub fn clip(bytes: &[u8]) -> String {
    if bytes.len() <= TEXT_LIMIT {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let spill = exec_spill_line(bytes);
    let head_has_spill = spill.as_ref().is_some_and(|line| {
        String::from_utf8_lossy(&bytes[..CLIP_HEAD.min(bytes.len())]).contains(line.as_str())
    });
    let tail_has_spill = spill.as_ref().is_some_and(|line| {
        let tail_start = bytes.len().saturating_sub(CLIP_TAIL);
        String::from_utf8_lossy(&bytes[tail_start..]).contains(line.as_str())
    });
    let spill_in_marker = spill.filter(|_| !head_has_spill && !tail_has_spill);
    let mut marker = clip_marker(
        bytes.len(),
        CLIP_HEAD,
        CLIP_TAIL,
        spill_in_marker.as_deref(),
    );
    let mut head = CLIP_HEAD;
    let mut tail = CLIP_TAIL;
    for _ in 0..4 {
        let budget = TEXT_LIMIT.saturating_sub(marker.len()).saturating_sub(2);
        head = budget * CLIP_HEAD / (CLIP_HEAD + CLIP_TAIL);
        tail = budget.saturating_sub(head);
        let next = clip_marker(bytes.len(), head, tail, spill_in_marker.as_deref());
        if next.len() == marker.len() {
            marker = next;
            break;
        }
        marker = next;
    }
    let head_text = String::from_utf8_lossy(&bytes[..head.min(bytes.len())]);
    let tail_text = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(tail)..]);
    format!("{head_text}\n{marker}\n{tail_text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn script(
        body: &str,
        capture: Capture,
        timeout: Option<Duration>,
    ) -> Result<Output, Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let scratch = tempfile::tempdir()?;
        let child = scratch.path().join("child");
        std::fs::write(&child, format!("#!/bin/sh\n{body}\n"))?;
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755))?;
        Ok(command(
            &child,
            &[],
            scratch.path(),
            &[],
            &[],
            None,
            timeout,
            capture,
        )
        .await?)
    }

    #[tokio::test]
    async fn captures_inherited_output_through_eof() -> Result<(), Box<dyn std::error::Error>> {
        let stdout = script(
            "printf head; (sleep 0.05; printf tail) & exit 0",
            Capture::Stdout,
            None,
        )
        .await?;
        assert_eq!(stdout.bytes, b"headtail");
        let combined = script(
            "printf head; (sleep 0.05; printf tail >&2) & exit 0",
            Capture::Text,
            None,
        )
        .await?;
        assert_eq!(combined.bytes, b"headtail");
        Ok(())
    }

    #[tokio::test]
    async fn inherited_output_cannot_outlast_the_timeout() -> Result<(), Box<dyn std::error::Error>>
    {
        const TIMEOUT: Duration = Duration::from_millis(150);
        // Background tail appears after 3s; without the output timeout we would drain for ~3s.
        const BACKGROUND_TAIL: Duration = Duration::from_secs(3);
        let start = Instant::now();
        let output = script(
            "printf head; (sleep 3; printf tail) & exit 0",
            Capture::Text,
            Some(TIMEOUT),
        )
        .await?;
        let elapsed = start.elapsed();
        // Fails if the pipe drain waits for the background `tail` instead of honoring the timeout.
        assert_eq!(output.bytes, b"head");
        assert_eq!(output.code, -1);
        assert_eq!(output.error.as_deref(), Some("context deadline exceeded"));
        // The bound must stay under BACKGROUND_TAIL, or an implementation that waits for the tail would pass. 2.5 s is
        // still over 15x the 150 ms timeout, which absorbs a loaded machine.
        assert!(
            elapsed < Duration::from_millis(2500),
            "command took {elapsed:?}; ignored timeout would wait for background output (~{BACKGROUND_TAIL:?})"
        );
        Ok(())
    }

    #[tokio::test]
    async fn oversized_text_is_drained_and_clipped() -> Result<(), Box<dyn std::error::Error>> {
        let output = script(
            "printf head >&2; head -c 131072 /dev/zero",
            Capture::Text,
            None,
        )
        .await?;
        assert_eq!(output.code, 0);
        assert_eq!(output.bytes.len(), TEXT_LIMIT + 1);
        let clipped = clip(&output.bytes);
        assert!(clipped.len() <= TEXT_LIMIT);
        assert!(clipped.starts_with("head"));
        assert!(clipped.contains("[truncated: 8193 bytes total,"));
        assert!(clipped.ends_with('\0'));
        Ok(())
    }

    #[test]
    fn io_error_maps_common_kinds() {
        assert_eq!(
            io_error(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            "no such file or directory"
        );
        assert_eq!(
            io_error(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            "permission denied"
        );
    }

    #[test]
    fn lab_aliases_prefer_cs_overlay_values() {
        let _env = ScopedEnv::set([("CS_TRIAL".to_string(), "7".to_string())]);
        assert_eq!(env("LAB_TRIAL"), Some("7".to_string()));
    }
    #[test]
    fn control_reason_reports_deadline_and_cancel() {
        let expired = Control {
            deadline: Some(tokio::time::Instant::now()),
            ..Control::default()
        };
        assert_eq!(expired.reason(), Some("budget:seconds"));
        let cancelled = Control::default();
        cancelled.cancel_with("SIGINT");
        assert_eq!(cancelled.reason(), Some("interrupted"));
    }

    #[test]
    fn clip_keeps_head_and_tail_with_exec_spill_in_marker() {
        let spill = b"lines 1-1 of 9 \xe2\x80\x94 full output in .lab-out/1.txt\n";
        let mut body = spill.to_vec();
        body.extend(std::iter::repeat_n(b'm', 20_000));
        body.extend(std::iter::repeat_n(b't', 500));
        let clipped = clip(&body);
        assert!(clipped.contains("full output in .lab-out/"));
        assert!(clipped.contains("[truncated:"));
        assert!(clipped.contains("ttt"));
        assert!(clipped.len() <= TEXT_LIMIT);
    }

    #[tokio::test]
    async fn oversized_images_fail_with_a_capture_error() -> Result<(), Box<dyn std::error::Error>>
    {
        let output = script("head -c 17825792 /dev/zero", Capture::Image, None).await?;
        assert_eq!(output.bytes.len(), 0);
        assert_eq!(
            output.error.as_deref(),
            Some("output exceeds 16777216 bytes")
        );
        Ok(())
    }

    #[tokio::test]
    async fn actual_signals_and_timeout_have_go_diagnostics()
    -> Result<(), Box<dyn std::error::Error>> {
        const KILL_TIMEOUT: Duration = Duration::from_millis(200);
        for (signal, wording) in [
            ("TERM", "terminated"),
            ("KILL", "killed"),
            ("INT", "interrupt"),
            ("HUP", "hangup"),
        ] {
            let result = script(&format!("kill -{signal} $$"), Capture::Text, None).await?;
            assert_eq!(result.code, -1);
            assert_eq!(result.error, Some(format!("signal: {wording}")));
        }
        let start = Instant::now();
        let result = script("exec sleep 10", Capture::Text, Some(KILL_TIMEOUT)).await?;
        let elapsed = start.elapsed();
        // Fails if the timeout stops draining but does not SIGKILL the child (e.g. "context deadline exceeded" while sleep still runs).
        assert_eq!(result.error.as_deref(), Some("signal: killed"));
        assert!(
            elapsed < KILL_TIMEOUT.saturating_mul(20),
            "sleep child not killed promptly: waited {elapsed:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stopped_control_never_spawns_a_child() -> Result<(), Box<dyn std::error::Error>> {
        let scratch = tempfile::tempdir()?;
        let child = scratch.path().join("child");
        std::fs::write(&child, "#!/bin/sh\ntouch marker\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755))?;
        }
        for expired in [false, true] {
            let cancel = CancellationToken::new();
            if !expired {
                cancel.cancel();
            }
            let control = Control {
                cancel,
                deadline: expired.then(Instant::now),
                ..Control::default()
            };
            let result = command(
                &child,
                &[],
                scratch.path(),
                &[],
                &[],
                Some(&control),
                None,
                Capture::Text,
            )
            .await;
            assert_eq!(
                result.err().as_deref(),
                Some(if expired {
                    "context deadline exceeded"
                } else {
                    "context canceled"
                })
            );
            assert!(!scratch.path().join("marker").exists());
        }
        Ok(())
    }
}
