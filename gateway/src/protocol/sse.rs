//! The line reader every stream adapter shares: `data:` payloads from an upstream SSE body, under the
//! call's deadline. A read error and the deadline are errors; EOF is `Ok(None)` (after a final
//! unterminated line), and each adapter decides whether EOF came before its own terminal event
//! (WIRE clause 4).

use super::transport_after_response;
use crate::errors::CallError;
use tokio::time::Instant;

const MAX_LINE: usize = 1 << 20;

/// An open SSE body, read line by line.
pub struct Lines {
    resp: reqwest::Response,
    buf: Vec<u8>,
    deadline: Instant,
    /// Names the stream in read errors.
    label: &'static str,
    eof: bool,
}

impl Lines {
    /// Read `resp` until `deadline`.
    pub fn new(resp: reqwest::Response, deadline: Instant, label: &'static str) -> Self {
        Self {
            resp,
            buf: Vec::new(),
            deadline,
            label,
            eof: false,
        }
    }

    #[cfg(test)]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The next `data:` payload (`\r` and the one optional space after the colon removed); other lines
    /// (event names, comments, blanks) are skipped. `Ok(None)` at EOF.
    pub async fn data(&mut self) -> Result<Option<Vec<u8>>, CallError> {
        while let Some(line) = self.line().await? {
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            if let Some(rest) = line.strip_prefix(b"data:") {
                return Ok(Some(rest.strip_prefix(b" ").unwrap_or(rest).to_vec()));
            }
        }
        Ok(None)
    }

    async fn line(&mut self) -> Result<Option<Vec<u8>>, CallError> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                line.pop();
                return Ok(Some(line));
            }
            if self.eof {
                return Ok((!self.buf.is_empty()).then(|| std::mem::take(&mut self.buf)));
            }
            if self.buf.len() > MAX_LINE {
                return Err(CallError::transport_local("bufio.Scanner: token too long"));
            }
            let chunk = match tokio::time::timeout_at(self.deadline, self.resp.chunk()).await {
                Err(_) => {
                    return Err(CallError::deadline("context deadline exceeded"));
                }
                Ok(Ok(chunk)) => chunk,
                // A peer that closes TLS without close_notify has ended the body: the same EOF.
                Ok(Err(e)) if unexpected_eof(&e) => None,
                Ok(Err(e)) => return Err(transport_after_response("Read", self.label, &e)),
            };
            match chunk {
                Some(bytes) => self.buf.extend_from_slice(&bytes),
                None => self.eof = true,
            }
        }
    }
}

/// Whether a read error is only the peer closing without TLS `close_notify` (an EOF, as Go reads it).
fn unexpected_eof(e: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        if s.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
        {
            return true;
        }
        source = s.source();
    }
    false
}
