//! The gateway's log line and the time and duration spellings its contract shares with Go.
//!
//! Lines go to stderr as `2006/01/02 15:04:05 [csgateway] message`, Go's `log` with `Lmsgprefix`, so a
//! reader of either gateway implementation's log (the contract suite) parses one shape. Structured rows are one
//! JSON object at the end of a line.

use chrono::{DateTime, Local, SecondsFormat, Utc};
use std::time::Duration;

/// Write one log line.
pub fn line(message: &str) {
    eprintln!(
        "{} [csgateway] {message}",
        Local::now().format("%Y/%m/%d %H:%M:%S")
    );
}

/// Log a formatted line.
#[macro_export]
macro_rules! logf {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}

/// Log one structured row (a JSON object) on its own line.
pub fn row(value: &serde_json::Value) {
    line(&value.to_string());
}

/// RFC 3339 in UTC with `Z`, fraction only when non-zero (Go's `time.Time` JSON).
pub fn rfc3339(t: DateTime<Utc>) -> String {
    let s = t.to_rfc3339_opts(SecondsFormat::Nanos, true);
    match s.split_once('.') {
        Some((head, frac)) => {
            let digits = frac.trim_end_matches('Z').trim_end_matches('0');
            if digits.is_empty() {
                format!("{head}Z")
            } else {
                format!("{head}.{digits}Z")
            }
        }
        None => s,
    }
}

/// RFC 3339 in local time with offset, whole seconds (Go's `Format(time.RFC3339)`).
pub fn rfc3339_local(t: DateTime<Local>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(data)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Parse a Go duration (`300ms`, `1.5s`, `2m`, `1h30m`). `None` when malformed.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let (neg, mut rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if rest == "0" {
        return Some(Duration::ZERO);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total = 0f64;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..num_len].parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let nanos = match &rest[..unit_len] {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total += number * nanos;
    }
    if neg {
        return Some(Duration::ZERO);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(Duration::from_nanos(total as u64))
}

/// Go's `Duration.String`: `1m0s`, `30s`, `1.5s`, `250ms`, `0s`.
pub fn format_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".into();
    }
    if nanos < 1_000_000_000 {
        let (value, unit) = if nanos < 1_000 {
            return format!("{nanos}ns");
        } else if nanos < 1_000_000 {
            (nanos, "µs")
        } else {
            (nanos, "ms")
        };
        let scale = if unit == "µs" { 1_000 } else { 1_000_000 };
        return format!("{}{unit}", fraction(value, scale));
    }
    let secs = nanos / 1_000_000_000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let sub = nanos % 1_000_000_000;
    let seconds = fraction(
        u128::from(u64::try_from(s).unwrap_or(0)) * 1_000_000_000 + sub,
        1_000_000_000,
    );
    match (h, m) {
        (0, 0) => format!("{seconds}s"),
        (0, m) => format!("{m}m{seconds}s"),
        (h, m) => format!("{h}h{m}m{seconds}s"),
    }
}

fn fraction(value: u128, scale: u128) -> String {
    let whole = value / scale;
    let rem = value % scale;
    if rem == 0 {
        return whole.to_string();
    }
    let width = scale.to_string().len() - 1;
    let frac = format!("{rem:0width$}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_round_trip_go_spellings() {
        assert_eq!(parse_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_duration("60m"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_mins(90)));
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("5"), None);
        assert_eq!(format_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(format_duration(Duration::from_secs(30)), "30s");
        assert_eq!(format_duration(Duration::from_mins(15)), "15m0s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_duration(Duration::from_millis(12)), "12ms");
        assert_eq!(format_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn rfc3339_trims_like_go() {
        let t = DateTime::parse_from_rfc3339("2026-01-01T01:00:00Z")
            .map(|t| t.with_timezone(&Utc))
            .ok();
        assert_eq!(t.map(rfc3339).as_deref(), Some("2026-01-01T01:00:00Z"));
    }
}
