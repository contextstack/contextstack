//! C libyaml's reader in front of libyaml-safer's. libyaml-safer decodes all of its input at
//! once (its UTF-8 problem is "invalid UTF-8"); C reads a string 16 KiB at a time
//! (`YAML_RAW_BUFFER_SIZE`) and decodes a window only when the scanner runs out of characters,
//! so a syntax error in an earlier window, or a first document that ends before the bad window,
//! wins. `Gate` hands libyaml-safer the same windows and fails the read of a bad one with C's
//! problem.

use std::cell::Cell;
use std::io::{self, BufRead, Read};

/// `YAML_RAW_BUFFER_SIZE`.
const RAW_BUFFER: usize = 16_384;

/// Why C stops decoding at a character.
#[derive(Debug, PartialEq)]
enum Stop {
    /// The window ends inside the character: C keeps the bytes for the next read.
    Incomplete,
    Problem(&'static str),
}

/// C's printable set (`IS_PRINTABLE` as the reader applies it).
fn printable(value: u32) -> bool {
    matches!(value,
        0x09 | 0x0A | 0x0D | 0x20..=0x7E | 0x85 | 0xA0..=0xD7FF | 0xE000..=0xFFFD | 0x1_0000..=0x10_FFFF)
}

/// `yaml_parser_update_buffer` decoding the character at `b[0]`: its width, or why not.
fn decode(b: &[u8]) -> Result<usize, Stop> {
    let lead = b[0];
    let width = match lead {
        _ if lead & 0x80 == 0 => 1,
        _ if lead & 0xE0 == 0xC0 => 2,
        _ if lead & 0xF0 == 0xE0 => 3,
        _ if lead & 0xF8 == 0xF0 => 4,
        _ => return Err(Stop::Problem("invalid leading UTF-8 octet")),
    };
    if width > b.len() {
        return Err(Stop::Incomplete);
    }
    let mut value = u32::from(match width {
        1 => lead & 0x7F,
        2 => lead & 0x1F,
        3 => lead & 0x0F,
        _ => lead & 0x07,
    });
    for &octet in &b[1..width] {
        if octet & 0xC0 != 0x80 {
            return Err(Stop::Problem("invalid trailing UTF-8 octet"));
        }
        value = (value << 6) + u32::from(octet & 0x3F);
    }
    let shortest = match width {
        1 => true,
        2 => value >= 0x80,
        3 => value >= 0x800,
        _ => value >= 0x1_0000,
    };
    if !shortest {
        return Err(Stop::Problem("invalid length of a UTF-8 sequence"));
    }
    if (0xD800..=0xDFFF).contains(&value) || value > 0x10_FFFF {
        return Err(Stop::Problem("invalid Unicode character"));
    }
    if !printable(value) {
        return Err(Stop::Problem("control characters are not allowed"));
    }
    Ok(width)
}

/// The input as C libyaml reads a string; a refused window records its problem in `problem`.
pub struct Gate<'a> {
    data: &'a [u8],
    /// Where the next raw window starts (after the last whole character decoded).
    next: usize,
    /// The decoded window being handed out: `pos..end`.
    pos: usize,
    end: usize,
    problem: &'a Cell<Option<&'static str>>,
}

impl<'a> Gate<'a> {
    pub fn new(data: &'a [u8], problem: &'a Cell<Option<&'static str>>) -> Self {
        Self {
            data,
            next: 0,
            pos: 0,
            end: 0,
            problem,
        }
    }

    /// Decode the next raw window as C does: every whole character in it, or its problem.
    fn window(&mut self) -> io::Result<()> {
        let start = self.next;
        let last = (start + RAW_BUFFER).min(self.data.len());
        let mut at = start;
        while at < last {
            match decode(&self.data[at..last]) {
                Ok(width) => at += width,
                // C keeps a split character for the next read; at the end of input there is
                // none, and that read fails.
                Err(Stop::Incomplete) if at > start || last < self.data.len() => break,
                Err(Stop::Incomplete) => return Err(self.fail("incomplete UTF-8 octet sequence")),
                Err(Stop::Problem(problem)) => return Err(self.fail(problem)),
            }
        }
        (self.pos, self.end, self.next) = (start, at, at);
        Ok(())
    }

    fn fail(&self, problem: &'static str) -> io::Error {
        self.problem.set(Some(problem));
        io::Error::other(problem)
    }
}

impl Read for Gate<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Gate<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos == self.end && self.next < self.data.len() {
            self.window()?;
        }
        Ok(&self.data[self.pos..self.end])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read everything through the gate: the bytes delivered and the problem, if any.
    fn drain(data: &[u8]) -> (usize, Option<&'static str>) {
        let problem = Cell::new(None);
        let mut gate = Gate::new(data, &problem);
        let mut total = 0;
        loop {
            match gate.fill_buf() {
                Ok([]) => return (total, None),
                Ok(chunk) => {
                    let n = chunk.len();
                    total += n;
                    gate.consume(n);
                }
                Err(_) => return (total, problem.get()),
            }
        }
    }

    #[test]
    fn refusals_are_c_libyamls() {
        let cases: [(&[u8], Option<&str>); 9] = [
            (b"a: \xc3\xa9", None),
            (b"\xef\xbb\xbfa: 1\t\r\n", None),
            (b"a: \xff", Some("invalid leading UTF-8 octet")),
            (b"a\xe3\x41", Some("incomplete UTF-8 octet sequence")),
            (b"a\xe3\x41xyz", Some("invalid trailing UTF-8 octet")),
            (b"\xc0\x80", Some("invalid length of a UTF-8 sequence")),
            (b"\xed\xa0\x80", Some("invalid Unicode character")),
            (b"\xf5\x80\x80\x80", Some("invalid Unicode character")),
            (b"a: \x01", Some("control characters are not allowed")),
        ];
        for (text, want) in cases {
            assert_eq!(drain(text).1, want, "{text:?}");
        }
    }

    #[test]
    fn read_trait_delivers_bytes_from_gate() {
        use std::io::Read as _;
        let problem = Cell::new(None);
        let mut gate = Gate::new(b"hi", &problem);
        let mut buf = [0u8; 4];
        let n = gate.read(&mut buf).unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], b"hi");
    }

    #[test]
    fn windows_are_c_raw_buffers() {
        // A bad byte in the first window refuses it whole; in the second, the first is read.
        let mut text = vec![b'a'; RAW_BUFFER + 10];
        text[RAW_BUFFER + 5] = 0x01;
        assert_eq!(
            drain(&text),
            (RAW_BUFFER, Some("control characters are not allowed"))
        );
        text[100] = 0xff;
        assert_eq!(drain(&text), (0, Some("invalid leading UTF-8 octet")));
        // A character split by the window boundary is carried into the next window.
        let mut split = vec![b'a'; RAW_BUFFER - 1];
        split.extend_from_slice("é and more".as_bytes());
        assert_eq!(drain(&split), (split.len(), None));
    }
}
