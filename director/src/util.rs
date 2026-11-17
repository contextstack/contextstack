//! Small shared pieces: the log line, hashes, random tokens and ids, UTC timestamps, plain paths.

use chrono::{Local, Utc};
use sha2::{Digest, Sha256};
use std::fmt::Write;

/// Write one log line to stderr: `2006/01/02 15:04:05 [director] message`, the gateway's shape.
pub fn log(message: &str) {
    eprintln!(
        "{} [director] {message}",
        Local::now().format("%Y/%m/%d %H:%M:%S")
    );
}

/// Log a formatted line.
#[macro_export]
macro_rules! logf {
    ($($arg:tt)*) => { $crate::util::log(&format!($($arg)*)) };
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// 64 lower-case hex characters.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `n` bytes from the operating system's generator.
pub fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| format!("random: {e}"))?;
    Ok(buf)
}

/// A bearer secret: `prefix` and 256 random bits in hex. Only its hash is ever stored.
pub fn secret(prefix: &str) -> Result<String, String> {
    Ok(format!("{prefix}{}", hex(&random_bytes(32)?)))
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Crockford base32 of `bits` (most significant first), `chars` characters long.
fn base32(bits: u128, chars: u32) -> String {
    (0..chars)
        .rev()
        .map(|i| char::from(CROCKFORD[usize::try_from((bits >> (5 * i)) & 31).unwrap_or(0)]))
        .collect()
}

/// A ULID: 48 bits of milliseconds, 80 random bits, 26 Crockford characters (Laravel's `Str::ulid`).
pub fn ulid() -> Result<String, String> {
    let millis = u128::try_from(Utc::now().timestamp_millis().max(0)).unwrap_or(0);
    let random = random_bytes(10)?
        .iter()
        .fold(0u128, |acc, b| (acc << 8) | u128::from(*b));
    Ok(base32((millis << 80) | random, 26))
}

/// A pairing code: 50 random bits as two groups of five Crockford characters (`7QK2M-XC4HD`).
pub fn pairing_code() -> Result<String, String> {
    let bits = random_bytes(7)?
        .iter()
        .fold(0u128, |acc, b| (acc << 8) | u128::from(*b))
        & ((1 << 50) - 1);
    let code = base32(bits, 10);
    Ok(format!("{}-{}", &code[..5], &code[5..]))
}

/// A typed pairing code in its canonical form: upper case, no separators, Crockford's look-alikes folded.
pub fn canonical_code(typed: &str) -> String {
    typed
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

/// Now, UTC, whole seconds: `YYYY-MM-DDTHH:MM:SSZ` (sorts as text).
pub fn now_utc() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `seconds` from now in the same form.
pub fn utc_in(seconds: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(seconds))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Seconds since a timestamp written by [`now_utc`]; `None` when it does not parse.
pub fn seconds_since(stamp: &str) -> Option<i64> {
    let then = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M:%SZ").ok()?;
    Some((Utc::now().naive_utc() - then).num_seconds())
}

/// A plain relative path: no leading `/`, no `\` or NUL, no empty, `.` or `..` segment.
pub fn plain_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && path.split('/').all(|p| !matches!(p, "" | "." | ".."))
}

/// `[A-Za-z0-9][A-Za-z0-9._-]{0,max-1}`: node names, run ids, installation ids.
pub fn plain_name(s: &str, max: usize) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.len() <= max
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A percent-encoded URL path segment decoded as UTF-8; `None` when malformed.
pub fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = char::from(bytes.next()?).to_digit(16)?;
            let lo = char::from(bytes.next()?).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_their_shapes() {
        let id = ulid().unwrap_or_default();
        assert_eq!(id.len(), 26);
        assert!(id.bytes().all(|b| CROCKFORD.contains(&b)));
        let code = pairing_code().unwrap_or_default();
        assert_eq!(code.len(), 11);
        assert_eq!(canonical_code(&code.to_lowercase()).len(), 10);
        assert_eq!(canonical_code("o1l-i"), "0111");
        assert!(secret("csn_").unwrap_or_default().starts_with("csn_"));
    }

    #[test]
    fn hashes_and_names() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(is_sha256(&sha256_hex(b"x")));
        assert!(!is_sha256(&sha256_hex(b"x").to_uppercase()));
        assert!(plain_path("tasks/t/goal.md"));
        for bad in ["", "/etc", "a/../b", "a//b", "./a", "a\\b"] {
            assert!(!plain_path(bad), "{bad}");
        }
        assert!(plain_name("node-a.node-1", 100));
        assert!(!plain_name("-x", 100));
        assert!(!plain_name("a/b", 100));
        assert_eq!(percent_decode("a%20b/c%2Fd").as_deref(), Some("a b/c/d"));
        assert_eq!(percent_decode("bad%2"), None);
    }

    #[test]
    fn timestamps_round_trip() {
        assert!(seconds_since(&now_utc()).is_some_and(|s| (0..5).contains(&s)));
        assert!(utc_in(60) > now_utc());
    }
}
