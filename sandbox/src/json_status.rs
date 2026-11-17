//! Drain a child's `--json-status-fd` pipe without blocking on a full kernel buffer.

use std::fs::File;
use std::io::Read;
use std::process::{Child, ExitStatus};

use crate::failure::{Failure, unenforceable};

/// Wait for `child` while a background thread reads `status_reader` to EOF.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn wait_with_status_drain(
    child: &mut Child,
    status_reader: &mut File,
) -> Result<(ExitStatus, String), Failure> {
    let mut reader = status_reader
        .try_clone()
        .map_err(|error| unenforceable(format!("cannot read bwrap status: {error}")))?;
    let collector = std::thread::spawn(move || {
        let mut report = String::new();
        let _ = reader.read_to_string(&mut report);
        report
    });
    let status = child
        .wait()
        .map_err(|error| unenforceable(format!("waiting for the tool failed: {error}")))?;
    let report = collector
        .join()
        .map_err(|_| unenforceable("bwrap status reader thread panicked"))?;
    Ok((status, report))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};

    use super::*;
    use crate::bwrap;

    #[test]
    fn status_longer_than_a_pipe_buffer_still_drains() -> Result<(), String> {
        let (read_half, write_half) = UnixStream::pair().map_err(|e| e.to_string())?;
        let mut writer = write_half;
        std::thread::spawn(move || {
            let chunk = "0123456789abcdef".repeat(1024);
            for _ in 0..0x100 {
                let _ = writer.write_all(chunk.as_bytes());
            }
            let _ = writer.write_all(b"\n{\"child-pid\":1}\n");
        });
        let mut reader = File::from(std::os::fd::OwnedFd::from(read_half));
        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let (status, report) =
            wait_with_status_drain(&mut child, &mut reader).map_err(|f| f.message().to_owned())?;
        assert!(status.success());
        assert!(bwrap::started(&report));
        Ok(())
    }
}
