//! Ruby's Time and Date as okf reaches them: `Time.iso8601` (time.rb's `xmlschema`, with
//! `apply_offset`, `zone_offset` and `force_zone!`), `Time.utc`/`Time.local`/`Time.new`
//! range checks and normalisation, Psych's `parse_time`, and their `to_s`/`inspect`/`iso8601`.

use super::rb::is_space;
use chrono::{DateTime, LocalResult, Offset as _, TimeZone as _};

/// A proleptic Gregorian civil date (Psych's `Date.strptime(s, '%F', Date::GREGORIAN)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    pub y: i64,
    pub m: i64,
    pub d: i64,
}

impl Date {
    pub fn to_s(self) -> String {
        format!("{}-{:02}-{:02}", year(self.y), self.m, self.d)
    }

    pub fn inspect(self) -> String {
        let jd = days_from_civil(self.y, self.m, self.d) + 2_440_588;
        format!("#<Date: {} (({jd}j,0s,0n),+0s,-Infj)>", self.to_s())
    }

    /// A valid calendar date, or `None` (Ruby's `Date::Error`).
    pub fn new(y: i64, m: i64, d: i64) -> Option<Self> {
        let ok = (1..=12).contains(&m) && d >= 1 && d <= month_len(y, m);
        ok.then_some(Self { y, m, d })
    }
}

/// How a Time displays its zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    Utc,
    /// A fixed or local offset in seconds east of UTC.
    Offset(i64),
}

/// An instant: whole seconds since the epoch plus the decimal fraction's digits (no
/// trailing zeros), and the zone it displays in.
#[derive(Clone, Debug)]
pub struct Time {
    secs: i64,
    frac: String,
    zone: Zone,
}

/// The broken-down wall-clock fields Ruby's constructors take.
#[derive(Clone, Copy)]
struct Fields {
    y: i64,
    mon: i64,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
}

impl Fields {
    /// `Time.utc`'s range checks (`ArgumentError` when they fail).
    fn valid(self) -> bool {
        let base = (1..=12).contains(&self.mon)
            && (1..=31).contains(&self.day)
            && (0..=24).contains(&self.hour)
            && (0..=59).contains(&self.min)
            && (0..=60).contains(&self.sec);
        base && (self.hour < 24 || (self.min == 0 && self.sec == 0))
    }

    /// Seconds since the epoch reading the fields as UTC; out-of-month days roll forward.
    fn naive_secs(self) -> i64 {
        let days = days_from_civil(self.y, self.mon, 1) + self.day - 1;
        days * 86_400 + self.hour * 3_600 + self.min * 60 + self.sec
    }
}

impl Time {
    pub fn same_instant(&self, other: &Self) -> bool {
        self.secs == other.secs && self.frac == other.frac
    }

    /// Ordering of instants.
    pub fn cmp_instant(&self, other: &Self) -> std::cmp::Ordering {
        let width = self.frac.len().max(other.frac.len());
        let a = format!("{:0<width$}", self.frac);
        let b = format!("{:0<width$}", other.frac);
        self.secs.cmp(&other.secs).then(a.cmp(&b))
    }

    fn offset(&self) -> i64 {
        match self.zone {
            Zone::Utc => 0,
            Zone::Offset(o) => o,
        }
    }

    /// Local wall-clock text `%Y-%m-%d %H:%M:%S`, with `sep` between date and time.
    fn wall(&self, sep: char) -> String {
        let local = self.secs + self.offset();
        let (y, m, d) = civil_from_days(local.div_euclid(86_400));
        let rem = local.rem_euclid(86_400);
        let (hh, mm, ss) = (rem / 3_600, rem % 3_600 / 60, rem % 60);
        format!("{}-{m:02}-{d:02}{sep}{hh:02}:{mm:02}:{ss:02}", year(y))
    }

    /// `%z`: `+hhmm`, or `+hhmmss` when the offset has seconds.
    fn zone_z(&self) -> String {
        let off = self.offset();
        let sign = if off < 0 { '-' } else { '+' };
        let a = off.abs();
        let (h, m, s) = (a / 3_600, a % 3_600 / 60, a % 60);
        if s == 0 {
            format!("{sign}{h:02}{m:02}")
        } else {
            format!("{sign}{h:02}{m:02}{s:02}")
        }
    }

    fn zone_label(&self) -> String {
        match self.zone {
            Zone::Utc => "UTC".to_string(),
            Zone::Offset(_) => self.zone_z(),
        }
    }

    pub fn to_s(&self) -> String {
        format!("{} {}", self.wall(' '), self.zone_label())
    }

    /// `Time#inspect`: the fraction appears, as a rational beyond nanoseconds.
    pub fn inspect(&self) -> String {
        let wall = self.wall(' ');
        let zone = self.zone_label();
        if self.frac.is_empty() {
            return format!("{wall} {zone}");
        }
        if self.frac.len() <= 9 {
            return format!("{wall}.{} {zone}", self.frac);
        }
        format!("{wall} {} {zone}", rational_text(&self.frac))
    }

    /// `Time#iso8601` (no fraction digits).
    pub fn iso8601(&self) -> String {
        let wall = self.wall('T');
        if self.zone == Zone::Utc {
            return format!("{wall}Z");
        }
        let off = self.offset();
        let sign = if off < 0 { '-' } else { '+' };
        let a = off.abs();
        format!("{wall}{sign}{:02}:{:02}", a / 3_600, a % 3_600 / 60)
    }

    /// The float seconds Ruby's `Time#-` returns, truncated by `to_i`.
    pub fn seconds_since(&self, earlier: &Self) -> i64 {
        let width = self.frac.len().max(earlier.frac.len());
        let a = frac_units(&self.frac, width);
        let b = frac_units(&earlier.frac, width);
        let whole = i128::from(self.secs) - i128::from(earlier.secs);
        if width <= 9 {
            let scale = 10_i128.pow(9 - u32::try_from(width).unwrap_or(9));
            let ns = whole * 1_000_000_000 + (a - b) * scale;
            return ns_to_seconds(ns);
        }
        let scale = 10_i128.checked_pow(u32::try_from(width).unwrap_or(u32::MAX));
        let units = scale.and_then(|s| whole.checked_mul(s)).map(|w| w + a - b);
        let text = units.map_or_else(|| whole.to_string(), |u| decimal_text(u, width));
        truncate_f64(text.parse::<f64>().unwrap_or(0.0))
    }

    /// Seconds as a Float (`Time#to_f`): Ruby's `rb_time_unmagnify_to_float`.
    pub fn to_f(&self) -> f64 {
        if self.frac.len() <= 9 {
            let ns = i128::from(self.secs) * 1_000_000_000 + frac_units(&self.frac, 9);
            return ns_to_f64(ns);
        }
        format!("{}.{}0", self.secs, self.frac)
            .parse()
            .unwrap_or(0.0)
    }
}

/// Ruby's `Float#to_i` on a finite value (saturating).
fn truncate_f64(f: f64) -> i64 {
    let t = f.trunc();
    #[allow(clippy::cast_possible_truncation)] // saturating cast after trunc is the intent
    let i = t as i64;
    i
}

/// `rb_time_unmagnify_to_float` for an integer count of nanoseconds on a 64-bit Ruby: the
/// Integer converted to double (rounded to nearest), then divided by 1e9.
fn ns_to_f64(ns: i128) -> f64 {
    #[allow(clippy::cast_precision_loss)] // the (double) conversion is what Ruby does
    let f = ns as f64;
    f / 1e9
}

/// `(a - b).to_i` for an integer nanosecond difference.
fn ns_to_seconds(ns: i128) -> i64 {
    truncate_f64(ns_to_f64(ns))
}

fn frac_units(frac: &str, width: usize) -> i128 {
    format!("{frac:0<width$}").parse().unwrap_or(0)
}

/// `units / 10^width` as decimal text.
fn decimal_text(units: i128, width: usize) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let digits = format!("{:0>w$}", units.unsigned_abs(), w = width + 1);
    let (int, frac) = digits.split_at(digits.len() - width);
    format!("{sign}{int}.{frac}")
}

/// `0.digits` as Ruby prints a sub-nanosecond Rational: `num/den` in lowest terms.
fn rational_text(frac: &str) -> String {
    let num: u128 = frac.parse().unwrap_or(0);
    let den = 10_u128.checked_pow(u32::try_from(frac.len()).unwrap_or(u32::MAX));
    let Some(den) = den else {
        return format!("0.{frac}");
    };
    let g = gcd(num, den);
    format!("{}/{}", num / g, den / g)
}

fn gcd(a: u128, b: u128) -> u128 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}

/// `%Y`: at least four digits, sign before the padding.
fn year(y: i64) -> String {
    if y < 0 {
        format!("-{:04}", y.unsigned_abs())
    } else {
        format!("{y:04}")
    }
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn month_len(y: i64, m: i64) -> i64 {
    match m {
        2 if leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// time.rb's `month_days`, Ruby array indexing included (month 0 reads December's 31).
fn month_days(y: i64, m: i64) -> Option<i64> {
    let idx = m - 1;
    let idx = if (-12..0).contains(&idx) {
        idx + 12
    } else {
        idx
    };
    if !(0..12).contains(&idx) {
        return None;
    }
    let len = if idx == 1 {
        month_len(y, 2)
    } else {
        month_len(y, idx + 1)
    };
    Some(len)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The local zone's offset at an instant.
fn local_offset_at(secs: i64) -> i64 {
    match DateTime::from_timestamp(secs, 0) {
        Some(utc) => {
            let off = chrono::Local.offset_from_utc_datetime(&utc.naive_utc());
            i64::from(off.fix().local_minus_utc())
        }
        None => 0,
    }
}

/// The local zone's offset for a wall-clock time given as naive seconds.
fn local_offset_for_wall(naive: i64) -> i64 {
    let Some(dt) = DateTime::from_timestamp(naive, 0) else {
        return 0;
    };
    match chrono::Local.offset_from_local_datetime(&dt.naive_utc()) {
        LocalResult::Single(off) | LocalResult::Ambiguous(off, _) => {
            i64::from(off.fix().local_minus_utc())
        }
        LocalResult::None => local_offset_at(naive),
    }
}

fn utc(f: Fields, frac: &str) -> Option<Time> {
    f.valid().then(|| Time {
        secs: f.naive_secs(),
        frac: frac.to_string(),
        zone: Zone::Utc,
    })
}

fn local(f: Fields, frac: &str) -> Option<Time> {
    if !f.valid() {
        return None;
    }
    let naive = f.naive_secs();
    let off = local_offset_for_wall(naive);
    Some(Time {
        secs: naive - off,
        frac: frac.to_string(),
        zone: Zone::Offset(off),
    })
}

/// `Time.new(y, m, d, h, mi, s, offset)`.
fn fixed(f: Fields, frac: &str, off: i64) -> Option<Time> {
    if !f.valid() || off.abs() >= 86_400 {
        return None;
    }
    Some(Time {
        secs: f.naive_secs() - off,
        frac: frac.to_string(),
        zone: Zone::Offset(off),
    })
}

/// `Time.at(secs, usec)` in the local zone.
fn at_local(secs: i64, frac: &str) -> Time {
    Time {
        secs,
        frac: frac.to_string(),
        zone: Zone::Offset(local_offset_at(secs)),
    }
}

fn trim_frac(digits: &str) -> String {
    digits.trim_end_matches('0').to_string()
}

/// A run of ASCII digits at the start of `s`.
fn digit_run(s: &[u8]) -> usize {
    s.iter().take_while(|b| b.is_ascii_digit()).count()
}

fn num(s: &[u8]) -> i64 {
    std::str::from_utf8(s)
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(i64::MAX)
}

/// time.rb's `zone_offset` for the zones `xmlschema` admits.
fn zone_offset(zone: &str) -> i64 {
    let z = zone.to_ascii_uppercase();
    let b = z.as_bytes();
    if z == "Z" {
        return 0;
    }
    let sign = if b[0] == b'-' { -1 } else { 1 };
    let hh = num(&b[1..3]);
    let rest = &b[3..];
    let mm = match rest.len() {
        0 => 0,
        2 => num(rest),
        _ => num(&rest[1..]),
    };
    sign * (hh * 60 + mm) * 60
}

/// time.rb's `apply_offset`, step for step.
fn apply_offset(mut f: Fields, off: i64) -> Option<Fields> {
    if off < 0 {
        let mut off = -off;
        let mut o = off.rem_euclid(60);
        off = off.div_euclid(60);
        if o != 0 {
            f.sec += o;
            o = f.sec.div_euclid(60);
            f.sec = f.sec.rem_euclid(60);
            off += o;
        }
        o = off.rem_euclid(60);
        off = off.div_euclid(60);
        if o != 0 {
            f.min += o;
            o = f.min.div_euclid(60);
            f.min = f.min.rem_euclid(60);
            off += o;
        }
        o = off.rem_euclid(24);
        off = off.div_euclid(24);
        if o != 0 {
            f.hour += o;
            o = f.hour.div_euclid(24);
            f.hour = f.hour.rem_euclid(24);
            off += o;
        }
        if off != 0 {
            f.day += off;
            if month_days(f.y, f.mon).is_some_and(|days| days < f.day) {
                f.mon += 1;
                if f.mon > 12 {
                    f.mon = 1;
                    f.y += 1;
                }
                f.day = 1;
            }
        }
    } else if off > 0 {
        let mut off = off;
        let mut o = off.rem_euclid(60);
        off = off.div_euclid(60);
        if o != 0 {
            f.sec -= o;
            o = f.sec.div_euclid(60);
            f.sec = f.sec.rem_euclid(60);
            off -= o;
        }
        o = off.rem_euclid(60);
        off = off.div_euclid(60);
        if o != 0 {
            f.min -= o;
            o = f.min.div_euclid(60);
            f.min = f.min.rem_euclid(60);
            off -= o;
        }
        o = off.rem_euclid(24);
        off = off.div_euclid(24);
        if o != 0 {
            f.hour -= o;
            o = f.hour.div_euclid(24);
            f.hour = f.hour.rem_euclid(24);
            off -= o;
        }
        if off != 0 {
            f.day -= off;
            if f.day < 1 {
                f.mon -= 1;
                if f.mon < 1 {
                    f.y -= 1;
                    f.mon = 12;
                }
                f.day = month_days(f.y, f.mon)?;
            }
        }
    }
    Some(f)
}

/// The zone alternatives `(Z|[+-]\d\d(?::?\d\d)?)` can take at the start of `s`, longest
/// first, as byte lengths.
fn zone_lengths(s: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if matches!(s.first(), Some(b'Z' | b'z')) {
        out.push(1);
        return out;
    }
    if !matches!(s.first(), Some(b'+' | b'-')) || digit_run(&s[1..]) < 2 {
        return out;
    }
    if s.len() >= 6 && s[3] == b':' && digit_run(&s[4..]) >= 2 {
        out.push(6);
    }
    if digit_run(&s[1..]) >= 4 {
        out.push(5);
    }
    out.push(3);
    out
}

fn all_space(s: &[u8]) -> bool {
    s.iter().all(|b| is_space(char::from(*b)))
}

/// `Time.iso8601(text)`; `None` is the `ArgumentError`.
pub fn iso8601(text: &str) -> Option<Time> {
    let bytes = text.as_bytes();
    let mut pos = bytes
        .iter()
        .take_while(|c| is_space(char::from(**c)))
        .count();
    let neg = bytes.get(pos) == Some(&b'-');
    if neg {
        pos += 1;
    }
    let len = digit_run(&bytes[pos..]);
    if len == 0 {
        return None;
    }
    let year_n = num(&bytes[pos..pos + len]);
    let year_n = if neg { -year_n } else { year_n };
    pos += len;
    let shape = b"-dd-ddTdd:dd:dd";
    let body = bytes.get(pos..pos + shape.len())?;
    let fits = shape.iter().zip(body).all(|(p, c)| match p {
        b'd' => c.is_ascii_digit(),
        b'T' => matches!(c, b'T' | b't'),
        _ => p == c,
    });
    if !fits {
        return None;
    }
    let f = Fields {
        y: year_n,
        mon: num(&body[1..3]),
        day: num(&body[4..6]),
        hour: num(&body[7..9]),
        min: num(&body[10..12]),
        sec: num(&body[13..15]),
    };
    pos += shape.len();
    let mut frac = String::new();
    if bytes.get(pos) == Some(&b'.') && digit_run(&bytes[pos + 1..]) > 0 {
        let len = digit_run(&bytes[pos + 1..]);
        frac = trim_frac(std::str::from_utf8(&bytes[pos + 1..=pos + len]).unwrap_or(""));
        pos += 1 + len;
    }
    let rest = &bytes[pos..];
    for len in zone_lengths(rest) {
        if all_space(&rest[len..]) {
            let zone = std::str::from_utf8(&rest[..len]).unwrap_or("Z");
            return zoned(f, &frac, zone);
        }
    }
    if all_space(rest) {
        return local(f, &frac);
    }
    None
}

/// The zoned branch of `xmlschema`: `apply_offset`, `Time.utc`, `force_zone!`.
fn zoned(f: Fields, frac: &str, zone: &str) -> Option<Time> {
    let off = zone_offset(zone);
    let shifted = apply_offset(f, off)?;
    let mut t = utc(shifted, frac)?;
    let utc_zone = ["-00:00", "-0000", "-00", "Z"]
        .iter()
        .any(|z| z.eq_ignore_ascii_case(zone));
    if !utc_zone {
        t.zone = Zone::Offset(off);
    }
    Some(t)
}

/// What Psych's `parse_time` produces: a Time, `None` for the rescued `ArgumentError`,
/// or `Err` for the uncaught `NoMethodError` (no space or T to split on).
pub fn psych_time(string: &str) -> Result<Option<Time>, ()> {
    let Some(split) = string.find([' ', 't', 'T']) else {
        return Err(());
    };
    let (date, time) = (&string.as_bytes()[..split], &string.as_bytes()[split + 1..]);
    let neg = date.first() == Some(&b'-');
    let ymd = if neg { &date[1..] } else { date };
    let year_n = num(&ymd[..4]);
    let year_n = if neg { -year_n } else { year_n };
    let mon_len = digit_run(&ymd[5..]);
    let mon = num(&ymd[5..5 + mon_len]);
    let day = num(&ymd[6 + mon_len..6 + mon_len + digit_run(&ymd[6 + mon_len..])]);
    let Some((start, parts, len)) = hms_search(time) else {
        return Err(());
    };
    let hms = &time[start..];
    let mut pos = len;
    let mut frac = String::new();
    if hms.get(pos) == Some(&b'.') {
        let n = digit_run(&hms[pos + 1..]);
        frac = trim_frac(std::str::from_utf8(&hms[pos + 1..=pos + n]).unwrap_or(""));
        pos += 1 + n;
    }
    while hms.get(pos).is_some_and(|c| is_space(char::from(*c))) {
        pos += 1;
    }
    let fields = Fields {
        y: year_n,
        mon,
        day,
        hour: parts[0],
        min: parts[1],
        sec: parts[2],
    };
    let Some(base) = utc(fields, &frac) else {
        return Ok(None);
    };
    let tz = &hms[pos..];
    if tz.first() == Some(&b'Z') {
        return Ok(Some(base));
    }
    if !matches!(tz.first(), Some(b'+' | b'-')) || digit_run(&tz[1..]) == 0 {
        return Ok(Some(at_local(base.secs, &frac)));
    }
    Ok(fixed(fields, &frac, psych_offset(tz)))
}

/// The leftmost `\d+:\d+:\d+` in `s`: its start, the three numbers and its length.
fn hms_search(s: &[u8]) -> Option<(usize, [i64; 3], usize)> {
    (0..s.len()).find_map(|start| {
        let mut parts = [0; 3];
        let mut j = start;
        for (k, part) in parts.iter_mut().enumerate() {
            let n = digit_run(&s[j..]);
            if n == 0 {
                return None;
            }
            *part = num(&s[j..j + n]);
            j += n;
            if k < 2 {
                if s.get(j) != Some(&b':') {
                    return None;
                }
                j += 1;
            }
        }
        Some((start, parts, j - start))
    })
}

/// `md[3].match(/^([+\-]?\d{1,2})\:?(\d{1,2})?$/)` then hours and minutes to seconds;
/// `md[3]` is `[-+]\d+(:\d\d)?` and TIME bounds it to sign, two digits, colon, two digits.
fn psych_offset(tz: &[u8]) -> i64 {
    let n = digit_run(&tz[1..]);
    let digits = &tz[1..=n];
    let colon = tz.get(1 + n) == Some(&b':');
    let (h, m) = if colon {
        (num(digits), num(&tz[2 + n..4 + n]))
    } else if n <= 2 {
        (num(digits), 0)
    } else {
        (num(&digits[..2]), num(&digits[2..]))
    };
    let h = if tz[0] == b'-' { -h } else { h };
    if h < 0 {
        h * 3_600 - m * 60
    } else {
        h * 3_600 + m * 60
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
fn iso8601_round_trips_offsets() {
        let t = iso8601("2026-01-01T00:00:00Z").map(|t| t.iso8601());
        assert_eq!(t.as_deref(), Some("2026-01-01T00:00:00Z"));
        let t = iso8601("2026-03-01T00:00:00+00:00").map(|t| t.iso8601());
        assert_eq!(t.as_deref(), Some("2026-03-01T00:00:00+00:00"));
        let t = iso8601(" 2026-01-01t05:30:00-00:00 ").map(|t| t.iso8601());
        assert_eq!(t.as_deref(), Some("2026-01-01T05:30:00Z"));
        assert!(iso8601("yesterday").is_none());
        assert!(iso8601("2026-13-01T00:00:00Z").is_none());
    }
    #[test]
fn apply_offset_rolls_month_zero_back() {
        let t = iso8601("2026-00-01T00:00:00+01:00").map(|t| t.iso8601());
        assert_eq!(t.as_deref(), Some("2026-01-01T00:00:00+01:00"));
        assert!(iso8601("2026-00-01T00:00:00Z").is_none());
    }
    #[test]
fn dates_and_differences() {
        assert_eq!(
            Date::new(2026, 1, 5).map(Date::inspect).as_deref(),
            Some("#<Date: 2026-01-05 ((2461046j,0s,0n),+0s,-Infj)>")
        );
        assert!(Date::new(2026, 2, 30).is_none());
        let a = iso8601("2026-01-02T00:00:00Z");
        let b = iso8601("2026-01-01T00:00:00Z");
        let d = a.zip(b).map(|(a, b)| a.seconds_since(&b));
        assert_eq!(d, Some(86_400));
    }
    #[test]
fn timespec_formats_subnanosecond_fractions_and_offset_seconds() {
        let Some(t) = iso8601("2020-01-01T00:00:00.123456789012Z") else {
            panic!("parse subnanosecond timestamp");
        };
        assert!(t.inspect().contains('/'));
        let Some(zoned) = iso8601("2020-01-01T12:00:00+05:30") else {
            panic!("parse offset timestamp");
        };
        assert_eq!(zoned.iso8601(), "2020-01-01T12:00:00+05:30");
        assert!(zoned.seconds_since(&t) > 0);
    }
    #[test]
fn psych_time_parses_zoned_iso8601_timestamps() {
        let Ok(Some(_)) = psych_time("2026-01-01T10:00:00Z") else {
            panic!("psych should parse zoned timestamp");
        };
    }
        #[test]
    fn iso8601_accepts_compact_and_colon_offsets() {
        assert!(iso8601("2026-06-15T12:00:00+0530").is_some());
        assert!(iso8601("2026-06-15T12:00:00-07:00").is_some());
        assert!(iso8601("2026-06-15T12:00:00+0000").is_some());
    }

    #[test]
    fn zone_offset_parses_colon_and_compact_forms() {
        assert_eq!(zone_offset("+0530"), 5 * 3600 + 30 * 60);
        assert_eq!(zone_offset("-0700"), -(7 * 3600));
        assert_eq!(zone_offset("Z"), 0);
    }
    #[test]
fn apply_offset_negative_rolls_calendar_when_needed() {
        let f = Fields { y: 2026, mon: 3, day: 1, hour: 0, min: 0, sec: 30 };
        let rolled = apply_offset(f, -90);
        assert!(rolled.is_some());
        let rolled = rolled.unwrap_or_else(|| panic!("offset apply"));
        assert_eq!((rolled.y, rolled.mon, rolled.day, rolled.sec), (2026, 3, 1, 0));
    }
    #[test]
fn apply_offset_positive_rolls_back_across_month_boundary() {
        let f = Fields { y: 2026, mon: 3, day: 1, hour: 0, min: 0, sec: 0 };
        let rolled = apply_offset(f, 86_400).unwrap_or_else(|| panic!("roll back one day"));
        assert_eq!((rolled.y, rolled.mon, rolled.day), (2026, 2, 28));
    }
    #[test]
fn to_f_preserves_long_subsecond_fractions() {
        let Some(t) = iso8601("2000-01-01T00:00:00.123456789012Z") else {
            panic!("parse timestamp with long fraction");
        };
        assert!(t.to_f() > 0.0);
    }

    #[test]
    fn zone_label_includes_seconds_when_offset_has_them() {
        let Ok(Some(t)) = psych_time("2026-01-01 10:00:00 +123") else {
            panic!("psych offset");
        };
        assert!(t.to_s().contains("+1203"));
    }

    #[test]
    fn cmp_instant_orders_subsecond_fraction_widths() {
        let Some(a) = iso8601("2020-01-01T00:00:00.5Z") else {
            panic!("a");
        };
        let Some(b) = iso8601("2020-01-01T00:00:00.50Z") else {
            panic!("b");
        };
        assert!(a.same_instant(&b));
        assert_eq!(a.cmp_instant(&b), std::cmp::Ordering::Equal);
        let Some(c) = iso8601("2020-01-01T00:00:00.51Z") else {
            panic!("c");
        };
        assert_eq!(c.cmp_instant(&b), std::cmp::Ordering::Greater);
    }

    #[test]
    fn inspect_omits_fraction_when_subsecond_field_empty() {
        let Some(t) = iso8601("2020-01-01T12:00:00Z") else {
            panic!("ts");
        };
        let text = t.inspect();
        assert!(!text.contains(". "));
        assert!(text.contains("UTC"));
    }


}
