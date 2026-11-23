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

#[cfg(test)]
macro_rules! process_event {
    ($($arg:tt)*) => {
        eprintln!(
            "process {} {}:{} [{}] unix_us={} {}",
            std::process::id(),
            file!(),
            line!(),
            std::thread::current().name().unwrap_or("<unnamed>"),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros(),
            format_args!($($arg)*)
        )
    };
}

#[cfg(test)]
struct ChildDropTrace {
    child: tokio::process::Child,
    test: std::thread::Thread,
    kill_on_drop: bool,
}

#[cfg(test)]
impl Drop for ChildDropTrace {
    fn drop(&mut self) {
        // An ID means Tokio hasn't collected an exit status. The child could
        // already be a zombie. Polling here would change the behavior we trace.
        if self.kill_on_drop
            && let Some(pid) = self.child.id()
        {
            let owner = self.test.name().unwrap_or("<unnamed>");
            process_event!("kill_on_drop signal=KILL pid=Some({pid}) owner={owner}");
        }
    }
}

#[cfg(any(test, feature = "fixtures"))]
use std::cell::RefCell;

#[cfg(any(test, feature = "fixtures"))]
thread_local! {
    static TEST_OVERLAY: RefCell<BTreeMap<String, String>> = const {
        RefCell::new(BTreeMap::new())
    };
}

/// Read the canonical environment variable, including test overlays.
pub fn env(name: &str) -> Option<String> {
    #[cfg(any(test, feature = "fixtures"))]
    if let Some(value) = TEST_OVERLAY.with(|map| map.borrow().get(name).cloned()) {
        return Some(value);
    }
    std::env::var(name).ok()
}

/// Read an environment variable; tests may layer values without mutating the process environment.
pub fn var_os(name: &str) -> Option<OsString> {
    env(name)
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

const DEFAULT_ENV: &str = "PATH HOME USER LOGNAME TERM LANG LC_ALL LC_CTYPE TMPDIR TMP TEMP SHELL HOMEBREW_PREFIX HOMEBREW_CELLAR CS_JOB_ID CS_TRIAL CS_SCHEDULE_DATE CS_SCHEDULE_SLOT CS_SCHEDULE_TIMEZONE CS_VIDEO_MUTATION CS_STEP_TTY CS_CODE_ROOT CS_CONFIG_ROOT CS_STATE_ROOT CS_WORKSPACES_ROOT CS_TOOL_PREFIX CS_SANDBOX_BIN CS_BROWSER_STATE CS_TOOL_TIMEOUT_SECONDS";

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
    pub timed_out: bool,
}

fn environment(extra: &[(String, String)]) -> BTreeMap<String, String> {
    let names = env("CS_CHILD_ENV").unwrap_or_else(|| DEFAULT_ENV.to_owned());
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
const EXEC_SPILL: &str = "full output in .cs-out/";

/// Select the stream and retained output needed by the caller.
pub enum Capture {
    /// Trusted operations read their full stdout reply.
    Stdout,
    /// Keep the caller’s group for the built-in terminal confirmation hook.
    Foreground,
    /// Text tools and unattended hooks run in their own process group.
    Text,
    /// Keep the latest combined check output.
    Tail,
    /// Image observations need a complete JSON envelope, up to 16 MiB.
    Image,
}

async fn drain(reader: &mut Receiver, bytes: &mut Vec<u8>, limit: u64) -> io::Result<()> {
    reader.take(limit).read_to_end(bytes).await?;
    tokio::io::copy(reader, &mut tokio::io::sink())
        .await
        .map(|_| ())
}

async fn drain_tail(reader: &mut Receiver, bytes: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0; TEXT_LIMIT];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        let excess = (bytes.len() + n).saturating_sub(TEXT_LIMIT);
        bytes.drain(..excess);
        bytes.extend_from_slice(&chunk[..n]);
    }
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
#[allow(clippy::too_many_lines, reason = "Test-only process event tracing.")]
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
    cs_child_signals::reset_on_exec(command.as_std_mut());
    command.args(args);
    let isolated = !matches!(capture, Capture::Foreground);
    if isolated {
        command.process_group(0);
    }
    let child = command
        .current_dir(cwd)
        .env_clear()
        .envs(environment(extra))
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("fork/exec {}: {}", bin.display(), io_error(&e)))?;
    #[cfg(test)]
    let mut traced = ChildDropTrace {
        child,
        test: std::thread::current(),
        kill_on_drop: command.get_kill_on_drop(),
    };
    #[cfg(test)]
    let child = &mut traced.child;
    #[cfg(not(test))]
    let mut child = child;
    drop(command);
    let group = child
        .id()
        .filter(|_| isolated)
        .and_then(|id| Pid::from_raw(id.cast_signed()));
    #[cfg(test)]
    let (pid, child_group) = (
        child.id(),
        group.unwrap_or_else(rustix::process::getpgrp).as_raw_pid(),
    );
    #[cfg(test)]
    process_event!("spawn pid={pid:?} pgid={child_group}");
    let stdin = child.stdin.take();
    let writer = async move {
        if let Some(mut pipe) = stdin {
            let _ = pipe.write_all(input).await;
        }
        Ok(())
    };
    let limit = match capture {
        Capture::Stdout => u64::MAX,
        Capture::Foreground | Capture::Text | Capture::Tail => TEXT_LIMIT as u64 + 1,
        Capture::Image => IMAGE_LIMIT as u64 + 1,
    };
    let mut bytes = Vec::new();
    let (result, timed_out) = tokio::select! {
        biased;
        () = async { match control { Some(c) => c.stopped().await, None => std::future::pending().await }} => (None, false),
        () = async { match timeout { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await }} => (None, true),
        result = async {
            // Keep the group leader's PID reserved until inherited pipes close.
            let output = async {
                if matches!(capture, Capture::Tail) {
                    drain_tail(&mut reader, &mut bytes).await
                } else {
                    drain(&mut reader, &mut bytes, limit).await
                }
            };
            tokio::try_join!(output, writer)?;
            let waited = child.wait().await;
            #[cfg(test)]
            process_event!("wait pid={pid:?} pgid={child_group} result={waited:?}");
            waited
        } => (Some(result), false),
    };
    let stopped = result.is_none();
    if let Some(group) = group {
        #[cfg(test)]
        process_event!("kill_process_group signal=KILL pid={pid:?} pgid={child_group}");
        let _ = kill_process_group(group, Signal::KILL);
    }
    let status = match result {
        Some(Ok(status)) => Ok(status),
        interrupted => {
            #[cfg(test)]
            process_event!(
                "start_kill signal=KILL pid={:?} pgid={child_group}",
                child.id()
            );
            let killed = child.start_kill();
            #[cfg(test)]
            {
                // Tokio disarms its drop guard after a successful explicit kill.
                traced.kill_on_drop &= killed.is_err();
            }
            let _ = killed;
            let waited = child.wait().await;
            #[cfg(test)]
            process_event!("wait pid={pid:?} pgid={child_group} result={waited:?}");
            interrupted.unwrap_or(waited)
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
    Ok(Output {
        bytes,
        code,
        error,
        timed_out,
    })
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
    let spill_in_marker = exec_spill_line(bytes).filter(|line| {
        let head = String::from_utf8_lossy(&bytes[..CLIP_HEAD]);
        let tail = String::from_utf8_lossy(&bytes[bytes.len() - CLIP_TAIL..]);
        !head.contains(line.as_str()) && !tail.contains(line.as_str())
    });
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

    #[tokio::test]
    async fn external_reap_leaves_tokio_with_a_stale_pid() -> io::Result<()> {
        use rustix::process::{WaitOptions, waitpid};

        // Deliberately violate exclusive wait ownership, with kill-on-drop
        // disabled so this test can never signal a recycled PID.
        let mut child = Command::new("/bin/sh").args(["-c", "exit 0"]).spawn()?;
        let id = child
            .id()
            .ok_or_else(|| io::Error::other("missing child PID"))?;
        let pid =
            Pid::from_raw(id.cast_signed()).ok_or_else(|| io::Error::other("invalid child PID"))?;
        let (reaped, status) = waitpid(Some(pid), WaitOptions::empty())?
            .ok_or_else(|| io::Error::other("blocking wait returned no status"))?;
        assert_eq!(reaped, pid);
        assert_eq!(status.exit_status(), Some(0));
        assert_eq!(child.id(), Some(id));
        assert_eq!(
            child.try_wait().err().and_then(|err| err.raw_os_error()),
            Some(rustix::io::Errno::CHILD.raw_os_error())
        );
        assert_eq!(child.id(), Some(id));
        Ok(())
    }

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
    async fn normal_exit_cleans_up_background_children() -> Result<(), Box<dyn std::error::Error>> {
        let scratch = tempfile::tempdir()?;
        let output = command(
            Path::new("/bin/sh"),
            &[
                "-c",
                "(sleep 0.5; touch survived) </dev/null >/dev/null 2>&1 &",
            ],
            scratch.path(),
            &[],
            &[],
            None,
            Some(Duration::from_secs(5)),
            Capture::Text,
        )
        .await?;
        assert_eq!(output.code, 0);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!scratch.path().join("survived").exists());
        Ok(())
    }

    #[tokio::test]
    async fn inherited_output_cannot_outlast_the_timeout() -> Result<(), Box<dyn std::error::Error>>
    {
        // Give the shell time to fork and exit even when the suite is running under load.
        const TIMEOUT: Duration = Duration::from_secs(1);
        const BACKGROUND_TAIL: Duration = Duration::from_secs(30);
        const ELAPSED_LIMIT: Duration = Duration::from_secs(15);
        let start = Instant::now();
        let output = script(
            &format!(
                "(sleep {}; printf tail) & printf head; exit 0",
                BACKGROUND_TAIL.as_secs()
            ),
            Capture::Text,
            Some(TIMEOUT),
        )
        .await?;
        let elapsed = start.elapsed();
        // Fails if the pipe drain waits for the background `tail` instead of honoring the timeout.
        assert_eq!(output.bytes, b"head");
        assert_eq!(output.code, -1);
        assert_eq!(output.error.as_deref(), Some("context deadline exceeded"));
        // Allow scheduling and cleanup delays while keeping the bound below the pipe's natural EOF.
        assert!(
            elapsed < ELAPSED_LIMIT,
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
    fn env_reads_overlay_values() {
        let _env = ScopedEnv::set([("CS_TRIAL".to_string(), "7".to_string())]);
        assert_eq!(env("CS_TRIAL"), Some("7".to_string()));
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
        let spill = b"lines 1-1 of 9 \xe2\x80\x94 full output in .cs-out/1.txt\n";
        let mut body = spill.to_vec();
        body.extend(std::iter::repeat_n(b'm', 20_000));
        body.extend(std::iter::repeat_n(b't', 500));
        let clipped = clip(&body);
        assert!(clipped.contains("full output in .cs-out/"));
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
        const KILL_TIMEOUT: Duration = Duration::from_secs(1);
        const CHILD_SLEEP: Duration = Duration::from_secs(30);
        const ELAPSED_LIMIT: Duration = Duration::from_secs(15);
        for (signal, wording) in [
            ("TERM", "terminated"),
            ("KILL", "killed"),
            ("INT", "interrupt"),
            ("HUP", "hangup"),
        ] {
            // Exec keeps the target PID and resets the shell's caught signal handlers.
            process_event!("fixture exec /bin/kill signal={signal} target=own PID");
            let result = script(
                &format!("exec /bin/kill -{signal} \"$$\""),
                Capture::Text,
                None,
            )
            .await?;
            assert_eq!(
                (result.code, result.error),
                (-1, Some(format!("signal: {wording}"))),
                "SIG{signal}: {}",
                String::from_utf8_lossy(&result.bytes)
            );
        }
        let start = Instant::now();
        let result = script(
            &format!("exec sleep {}", CHILD_SLEEP.as_secs()),
            Capture::Text,
            Some(KILL_TIMEOUT),
        )
        .await?;
        let elapsed = start.elapsed();
        assert_eq!(result.error.as_deref(), Some("signal: killed"));
        // Leave room for scheduling and cleanup while keeping the bound below the child's natural exit.
        assert!(
            elapsed < ELAPSED_LIMIT,
            "sleep child not killed promptly: waited {elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn inherited_sigint_is_reset() -> Result<(), Box<dyn std::error::Error>> {
        // Re-exec the test with SIGINT ignored without changing the test runner's signals.
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "trap '' INT; exec \"$@\"", "sh"])
            .arg(std::env::current_exe()?)
            .args([
                "--exact",
                "process::tests::actual_signals_and_timeout_have_go_diagnostics",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        let child_group = rustix::process::getpgrp().as_raw_pid();
        process_event!("spawn pid={pid} pgid={child_group}");
        let output = child.wait_with_output();
        process_event!(
            "wait pid={pid} pgid={child_group} result={:?}",
            output.as_ref().map(|output| output.status)
        );
        let output = output?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}"
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
