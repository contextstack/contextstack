//! Psych's `ScalarScanner#tokenize` (psych 5.3, `strict_integer: false`) for plain scalars:
//! which Ruby object a plain YAML scalar becomes. Each regex is matched by hand as Ruby
//! would, `^`/`$` being line anchors.

use super::rb::{Raise, Val, inspect_str, is_space};
use super::rtime::{self, Date};

/// `Restricted#find` refusing a class.
pub fn disallowed(class: &str) -> Raise {
    let message = format!("Tried to load unspecified class: {class}");
    Raise::new("Psych::DisallowedClass", message)
}

/// The Ruby object for a plain scalar; `Err` for `DisallowedClass` (symbols, and dates and
/// times unless `date_time` permits them: the class is loaded before the text is checked)
/// or an uncaught exception (`Float()` on garbage, a time with no space to split on).
pub fn tokenize(s: &str, date_time: bool) -> Result<Val, Raise> {
    if s.is_empty() {
        return Ok(Val::Nil);
    }
    if s.contains('\n') || string_like(s) {
        return Ok(word(s));
    }
    if time_like(s) {
        if !date_time {
            return Err(disallowed("Time"));
        }
        return match rtime::psych_time(s) {
            Ok(Some(t)) => Ok(Val::Time(t)),
            Ok(None) => Ok(Val::Str(s.to_string())),
            Err(()) => Err(Raise::new(
                "NoMethodError",
                "undefined method 'match' for nil",
            )),
        };
    }
    if let Some((y, m, d)) = date_like(s) {
        if !date_time {
            return Err(disallowed("Date"));
        }
        return Ok(Date::new(y, m, d).map_or_else(|| Val::Str(s.to_string()), Val::Date));
    }
    let lower = s.to_ascii_lowercase();
    if lower == ".inf" || lower == "+.inf" {
        return Ok(Val::Float(f64::INFINITY));
    }
    if lower == "-.inf" {
        return Ok(Val::Float(f64::NEG_INFINITY));
    }
    if lower == ".nan" {
        return Ok(Val::Float(f64::NAN));
    }
    if s.starts_with(':') && s.chars().count() >= 2 {
        return Err(disallowed("Symbol"));
    }
    if let Some(parts) = sexagesimal(s, false) {
        return Ok(Val::Int(sexagesimal_int(&parts)));
    }
    if let Some(parts) = sexagesimal(s, true) {
        return Ok(Val::Float(sexagesimal_float(&parts)));
    }
    if float_like(s) {
        if matches!(s, "." | "+." | "-.") {
            return Ok(Val::Str(s.to_string()));
        }
        return ruby_float(&float_source(s)).map(Val::Float);
    }
    if integer_like(s) {
        return Ok(Val::Int(parse_int(&s.replace([',', '_'], ""))));
    }
    Ok(Val::Str(s.to_string()))
}

/// `[[:alpha:]_\s!@#$%\^&*(){}<>|/\\~;=]`.
fn word_char(c: char) -> bool {
    c.is_alphabetic() || c == '_' || is_space(c) || "!@#$%^&*(){}<>|/\\~;=".contains(c)
}

/// `^[^\d.:-]?[[:alpha:]_\s!@#$%\^&*(){}<>|/\\~;=]+` at the start of a newline-free string.
fn string_like(s: &str) -> bool {
    let mut chars = s.chars();
    let c0 = chars.next();
    let c1 = chars.next();
    let skip_first = c0.is_some_and(|c| !c.is_ascii_digit() && !".:-".contains(c));
    (skip_first && c1.is_some_and(word_char)) || c0.is_some_and(word_char)
}

/// The string branch: booleans and nulls in up to five characters, otherwise the string.
fn word(s: &str) -> Val {
    if s.chars().count() > 5 {
        return Val::Str(s.to_string());
    }
    let line_starts = std::iter::once(0).chain(s.match_indices('\n').map(|(i, _)| i + 1));
    let starts_other = line_starts
        .filter_map(|i| s[i..].chars().next())
        .any(|c| !"ytonfYTONF~".contains(c));
    if starts_other {
        return Val::Str(s.to_string());
    }
    let any_line = |words: &[&str]| {
        s.split('\n')
            .any(|line| words.iter().any(|w| line.eq_ignore_ascii_case(w)))
    };
    if s == "~" || any_line(&["null"]) {
        return Val::Nil;
    }
    if any_line(&["yes", "true", "on"]) {
        return Val::Bool(true);
    }
    if any_line(&["no", "false", "off"]) {
        return Val::Bool(false);
    }
    Val::Str(s.to_string())
}

fn digits(s: &[u8]) -> usize {
    s.iter().take_while(|b| b.is_ascii_digit()).count()
}

/// A run of one or two digits followed by `next` (or the end when `next` is `None`).
fn one_or_two(s: &[u8], i: usize) -> Option<usize> {
    let n = digits(&s[i..]);
    (1..=2).contains(&n).then_some(i + n)
}

/// Psych's TIME regex.
fn time_like(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut pos = usize::from(bytes.first() == Some(&b'-'));
    if digits(&bytes[pos..]) != 4 || bytes.get(pos + 4) != Some(&b'-') {
        return false;
    }
    pos += 5;
    let Some(end) = one_or_two(bytes, pos).filter(|&end| bytes.get(end) == Some(&b'-')) else {
        return false;
    };
    let Some(mut pos) = one_or_two(bytes, end + 1) else {
        return false;
    };
    match bytes.get(pos) {
        Some(b'T' | b't') => pos += 1,
        Some(c) if is_space(char::from(*c)) => {
            while bytes.get(pos).is_some_and(|c| is_space(char::from(*c))) {
                pos += 1;
            }
        }
        _ => return false,
    }
    let Some(end) = one_or_two(bytes, pos).filter(|&end| bytes.get(end) == Some(&b':')) else {
        return false;
    };
    let ok = digits(&bytes[end + 1..]) == 2
        && bytes.get(end + 3) == Some(&b':')
        && digits(&bytes[end + 4..]) == 2;
    if !ok {
        return false;
    }
    let mut pos = end + 6;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1 + digits(&bytes[pos + 1..]);
    }
    let rest = &bytes[pos..];
    let lead = rest
        .iter()
        .take_while(|c| is_space(char::from(**c)))
        .count();
    let tz = &rest[lead..];
    rest.is_empty() || tz == b"Z" || tz_like(tz)
}

/// `[-+]\d{1,2}:?(?:\d\d)?` filling `tz`.
fn tz_like(tz: &[u8]) -> bool {
    if !matches!(tz.first(), Some(b'+' | b'-')) {
        return false;
    }
    let body = &tz[1..];
    let n = digits(body);
    let rest = &body[n..];
    match rest {
        [] => (1..=4).contains(&n),
        [b':'] => (1..=2).contains(&n),
        [b':', a, b] => (1..=2).contains(&n) && a.is_ascii_digit() && b.is_ascii_digit(),
        _ => false,
    }
}

/// `^\d{4}-(?:1[012]|0\d|\d)-(?:[12]\d|3[01]|0\d|\d)$` with the captured numbers.
fn date_like(s: &str) -> Option<(i64, i64, i64)> {
    let b = s.as_bytes();
    if digits(b) != 4 || b.get(4) != Some(&b'-') {
        return None;
    }
    let m_len = digits(&b[5..]);
    let month = &b[5..5 + m_len];
    let month_ok = match month {
        [_] | [b'0', _] => true,
        [b'1', c] => matches!(c, b'0'..=b'2'),
        _ => false,
    };
    if !month_ok || b.get(5 + m_len) != Some(&b'-') {
        return None;
    }
    let day = &b[6 + m_len..];
    let day_ok = match day {
        [c] | [b'1' | b'2' | b'0', c] => c.is_ascii_digit(),
        [b'3', c] => matches!(c, b'0' | b'1'),
        _ => false,
    };
    if !day_ok {
        return None;
    }
    let n = |x: &[u8]| {
        std::str::from_utf8(x)
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0)
    };
    Some((n(&b[..4]), n(month), n(day)))
}

/// `^[-+]?[0-9][0-9_]*(:[0-5]?[0-9]){1,2}$`, or with `\.[0-9_]*` before the end when
/// `float`; the colon-separated parts.
fn sexagesimal(s: &str, float: bool) -> Option<Vec<&str>> {
    let parts: Vec<&str> = s.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let head = parts[0].strip_prefix(['-', '+']).unwrap_or(parts[0]);
    let head_ok = head.starts_with(|c: char| c.is_ascii_digit())
        && head.chars().all(|c| c.is_ascii_digit() || c == '_');
    if !head_ok {
        return None;
    }
    let last = parts.len() - 1;
    for (k, part) in parts.iter().enumerate().skip(1) {
        let (body, tail) = match (k == last && float, part.split_once('.')) {
            (true, Some((body, tail))) => (body, Some(tail)),
            (true, None) => return None,
            (false, _) => (*part, None),
        };
        let body_ok = match body.as_bytes() {
            [d] => d.is_ascii_digit(),
            [a, b] => matches!(a, b'0'..=b'5') && b.is_ascii_digit(),
            _ => false,
        };
        let tail_ok = tail.is_none_or(|t| t.chars().all(|c| c.is_ascii_digit() || c == '_'));
        if !body_ok || !tail_ok {
            return None;
        }
    }
    Some(parts)
}

/// `String#to_i`: sign, then digits with single underscores between them.
fn ruby_to_i(s: &str) -> i128 {
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let text = underscored_digits(body);
    let v: i128 = text.parse().unwrap_or(0);
    if neg { -v } else { v }
}

/// The leading digits of `s`, dropping single underscores that sit between digits.
fn underscored_digits(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            out.push(char::from(c));
            i += 1;
            continue;
        }
        let between = c == b'_' && i > 0 && b.get(i + 1).is_some_and(u8::is_ascii_digit);
        if !between {
            break;
        }
        i += 1;
    }
    out
}

/// `String#to_f` for `[0-5]?[0-9]\.[0-9_]*`.
fn ruby_to_f(s: &str) -> f64 {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let frac = if int.is_empty() {
        String::new()
    } else {
        underscored_digits(frac)
    };
    format!("{}.{frac}0", underscored_digits(int))
        .parse()
        .unwrap_or(0.0)
}

/// `i += n.to_i * 60 ** (e - 2).abs` over the parts.
fn sexagesimal_int(parts: &[&str]) -> String {
    let mut total: i128 = 0;
    for (e, part) in parts.iter().enumerate() {
        let power = 60_i128.pow(u32::try_from(e.abs_diff(2)).unwrap_or(0));
        total = total.saturating_add(ruby_to_i(part).saturating_mul(power));
    }
    total.to_string()
}

/// `i += n.to_f * 60 ** (e - 2).abs` over the parts, in Float.
fn sexagesimal_float(parts: &[&str]) -> f64 {
    let mut total = 0.0;
    for (e, part) in parts.iter().enumerate() {
        let power = 60_f64.powi(i32::try_from(e.abs_diff(2)).unwrap_or(0));
        let n = if e == 0 {
            #[allow(clippy::cast_precision_loss)] // Integer#to_f, as Ruby converts
            let f = ruby_to_i(part) as f64;
            f
        } else {
            ruby_to_f(part)
        };
        total += n * power;
    }
    total
}

/// Psych's FLOAT: `^[-+]?([0-9][0-9_,]*)?\.[0-9]*([eE][-+][0-9]+)?$`.
fn float_like(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    if b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while b
            .get(i)
            .is_some_and(|c| c.is_ascii_digit() || *c == b'_' || *c == b',')
        {
            i += 1;
        }
    }
    if b.get(i) != Some(&b'.') {
        return false;
    }
    i += 1 + digits(&b[i + 1..]);
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let signed = matches!(b.get(i + 1), Some(b'+' | b'-'));
        let n = if signed { digits(&b[i + 2..]) } else { 0 };
        if n == 0 {
            return false;
        }
        i += 2 + n;
    }
    i == b.len()
}

/// `string.delete(',_').gsub(/\.([Ee]|$)/, '\1')`.
fn float_source(s: &str) -> String {
    let cleaned = s.replace([',', '_'], "");
    let mut out = String::new();
    let mut chars = cleaned.chars().peekable();
    while let Some(c) = chars.next() {
        let drop = c == '.' && matches!(chars.peek(), None | Some('e' | 'E'));
        if !drop {
            out.push(c);
        }
    }
    out
}

/// `Kernel#Float(string)`: surrounding whitespace, a sign, then decimal (`1_000.5e-3`, `.5`,
/// `1.`) or hexadecimal (`0x1.8p1`, `0x.8`, `0x7f.`) digits, underscores only between digits.
/// Anything else is Ruby's `ArgumentError`.
pub fn ruby_float(s: &str) -> Result<f64, Raise> {
    let invalid = || {
        let msg = format!("invalid value for Float(): {}", inspect_str(s));
        Raise::new("ArgumentError", msg)
    };
    let text = s.trim_matches(is_space);
    let (neg, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let hex = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X"));
    let value = match hex {
        Some(rest) => hex_float(rest),
        None => decimal_float(body),
    };
    let value = value.ok_or_else(invalid)?;
    Ok(if neg { -value } else { value })
}

/// A run of digits (per `ok`) with single underscores between them, from the start of `b`:
/// the digits and the bytes consumed; `None` when an underscore is misplaced.
fn digit_group(b: &[u8], ok: impl Fn(u8) -> bool) -> Option<(String, usize)> {
    let mut digits = String::new();
    let mut i = 0;
    while i < b.len() {
        if ok(b[i]) {
            digits.push(char::from(b[i]));
            i += 1;
            continue;
        }
        if b[i] != b'_' {
            break;
        }
        let between = i > 0 && ok(b[i - 1]) && b.get(i + 1).is_some_and(|c| ok(*c));
        if !between {
            return None;
        }
        i += 1;
    }
    Some((digits, i))
}

/// An exponent's digits after `e`/`p`: optional sign, at least one digit.
fn exponent(b: &[u8]) -> Option<i64> {
    let (neg, body) = match b.first() {
        Some(b'-') => (true, &b[1..]),
        Some(b'+') => (false, &b[1..]),
        _ => (false, b),
    };
    let (digits, used) = digit_group(body, |c| c.is_ascii_digit())?;
    if digits.is_empty() || used != body.len() {
        return None;
    }
    let v: i64 = digits.parse().unwrap_or(i64::MAX / 2);
    Some(if neg { -v } else { v })
}

fn decimal_float(body: &str) -> Option<f64> {
    let b = body.as_bytes();
    let (int, mut i) = digit_group(b, |c| c.is_ascii_digit())?;
    let mut frac = String::new();
    if b.get(i) == Some(&b'.') {
        let (digits, used) = digit_group(&b[i + 1..], |c| c.is_ascii_digit())?;
        frac = digits;
        i += 1 + used;
    }
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let mut exp = 0;
    if matches!(b.get(i), Some(b'e' | b'E')) {
        exp = exponent(&b[i + 1..])?;
    } else if i != b.len() {
        return None;
    }
    format!("{int}.{frac}0e{exp}").parse().ok()
}

fn hex_float(rest: &str) -> Option<f64> {
    let b = rest.as_bytes();
    let (int, mut i) = digit_group(b, |c| c.is_ascii_hexdigit())?;
    let mut frac = String::new();
    let dot = b.get(i) == Some(&b'.');
    if dot {
        let (digits, used) = digit_group(&b[i + 1..], |c| c.is_ascii_hexdigit())?;
        frac = digits;
        i += 1 + used;
    }
    if int.is_empty() && !dot {
        return None;
    }
    let mut exp = 0;
    if matches!(b.get(i), Some(b'p' | b'P')) {
        exp = exponent(&b[i + 1..])?;
    } else if i != b.len() {
        return None;
    }
    let mut mantissa = 0.0_f64;
    for c in int.chars().chain(frac.chars()) {
        mantissa = mantissa * 16.0 + f64::from(c.to_digit(16).unwrap_or(0));
    }
    let shift = exp - 4 * i64::try_from(frac.len()).unwrap_or(0);
    let shift = i32::try_from(shift.clamp(-2_000, 2_000)).unwrap_or(0);
    Some(mantissa * 2_f64.powi(shift))
}

/// `INTEGER_LEGACY`.
fn integer_like(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let b = body.as_bytes();
    let run = |b: &[u8], ok: &dyn Fn(u8) -> bool| -> bool {
        let lead = b.iter().take_while(|c| **c == b'_' || **c == b',').count();
        let rest = &b[lead..];
        rest.first().is_some_and(|c| ok(*c))
            && rest.iter().all(|c| ok(*c) || *c == b'_' || *c == b',')
    };
    if let Some(bin) = body.strip_prefix("0b") {
        return run(bin.as_bytes(), &|c| matches!(c, b'0' | b'1'));
    }
    if let Some(hex) = body.strip_prefix("0x") {
        return run(hex.as_bytes(), &|c| c.is_ascii_hexdigit());
    }
    if body == "0" {
        return true;
    }
    if let Some(oct) = body.strip_prefix('0') {
        return run(oct.as_bytes(), &|c| matches!(c, b'0'..=b'7'));
    }
    decimal_like(b)
}

/// `[1-9](?:[0-9]|,[0-9]|_[0-9])*`.
fn decimal_like(b: &[u8]) -> bool {
    if !b.first().is_some_and(|c| matches!(c, b'1'..=b'9')) {
        return false;
    }
    let mut i = 1;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let sep = matches!(b[i], b',' | b'_') && b.get(i + 1).is_some_and(u8::is_ascii_digit);
        if !sep {
            return false;
        }
        i += 2;
    }
    true
}

/// `Integer(s)` for a separator-free `INTEGER_LEGACY` match, in decimal.
fn parse_int(s: &str) -> String {
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (radix, digits) = if let Some(d) = body.strip_prefix("0b") {
        (2, d)
    } else if let Some(d) = body.strip_prefix("0x") {
        (16, d)
    } else if body.len() > 1 && body.starts_with('0') {
        (8, &body[1..])
    } else {
        (10, body)
    };
    let magnitude = to_decimal(digits, radix);
    if neg && magnitude != "0" {
        format!("-{magnitude}")
    } else {
        magnitude
    }
}

/// Digits in `radix` to decimal text, any length (limbs of 10^9).
fn to_decimal(digits: &str, radix: u32) -> String {
    let mut limbs: Vec<u64> = vec![0];
    for c in digits.chars() {
        let mut carry = u64::from(c.to_digit(radix).unwrap_or(0));
        for limb in &mut limbs {
            let v = *limb * u64::from(radix) + carry;
            *limb = v % 1_000_000_000;
            carry = v / 1_000_000_000;
        }
        if carry > 0 {
            limbs.push(carry);
        }
    }
    let mut parts = limbs.iter().rev();
    let head = parts.next().map(u64::to_string).unwrap_or_default();
    let tail: Vec<String> = parts.map(|limb| format!("{limb:09}")).collect();
    format!("{head}{}", tail.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::okf::rb::inspect;

    fn show(s: &str) -> String {
        match tokenize(s, true) {
            Ok(v) => inspect(&v),
            Err(e) => format!("{}: {}", e.class, e.message),
        }
    }


    #[test]
    fn tokenize_handles_non_finite_float_literals_and_refuses_dates_without_flag() {
        assert!(matches!(tokenize("+.inf", true), Ok(Val::Float(f)) if f.is_infinite() && f > 0.0));
        assert!(matches!(tokenize("-.inf", true), Ok(Val::Float(f)) if f.is_infinite() && f < 0.0));
        assert!(matches!(tokenize(".nan", true), Ok(Val::Float(f)) if f.is_nan()));
        assert!(matches!(tokenize(".", true), Ok(Val::Str(s)) if s == "."));
        let err = tokenize("2026-01-01 00:00:00Z", false).err().unwrap_or_else(|| panic!("err"));
        assert_eq!(err.class, "Psych::DisallowedClass");
    }

    #[test]
    fn tokenize_parses_sexagesimal_float_literals() {
        assert_eq!(show("2:30.25"), "9015.0");
    }
    #[test]
    fn scalars_resolve_as_psych_does() {
        assert_eq!(show("0.2"), "0.2");
        assert_eq!(show("0.10"), "0.1");
        assert_eq!(show("1:30"), "5400");
        assert_eq!(show("-1:30:15"), "-1785");
        assert_eq!(show("1:30.5"), "5430.0");
        assert_eq!(show("0b1_01"), "5");
        assert_eq!(show("017"), "15");
        assert_eq!(show("-0x1F"), "-31");
        assert_eq!(show("1,000"), "1000");
        assert_eq!(show("08"), "\"08\"");
        assert_eq!(show("1."), "1.0");
        assert_eq!(
            show(".e+5"),
            "ArgumentError: invalid value for Float(): \"e+5\""
        );
        assert_eq!(show("1e5"), "\"1e5\"");
        for (text, want) in [
            ("0x7f6c.", 32620.0),
            ("0x1.8p1", 3.0),
            ("0x.", 0.0),
            ("1.5e3_0", 1.5e30),
            (" 1.5 ", 1.5),
            ("1.", 1.0),
            ("-0x.8", -0.5),
        ] {
            assert_eq!(ruby_float(text).ok(), Some(want), "{text}");
        }
        for text in [
            "1__0", "0x", "0x.p", "1._5", "1e_5", ".", "inf", "0b101", "1 2", "0x1p",
        ] {
            assert!(ruby_float(text).is_err(), "{text}");
        }
        assert_eq!(show("yEs"), "true");
        assert_eq!(show("y"), "\"y\"");
        assert_eq!(show("~"), "nil");
        assert_eq!(
            show(":x"),
            "Psych::DisallowedClass: Tried to load unspecified class: Symbol"
        );
        assert_eq!(
            show("2026-1-5"),
            "#<Date: 2026-01-05 ((2461046j,0s,0n),+0s,-Infj)>"
        );
        assert_eq!(show("2026-02-30"), "\"2026-02-30\"");
        assert_eq!(
            show("123456789012345678901234567890"),
            "123456789012345678901234567890"
        );
        assert_eq!(
            show("2026-01-01 10:00:00 +123"),
            "2026-01-01 10:00:00 +1203"
        );
        assert_eq!(show("2026-01-01T10:00:00Z"), "2026-01-01 10:00:00 UTC");
        assert_eq!(show("020"), "16");
        assert_eq!(show("---"), "\"---\"");
        assert_eq!(show("..."), "\"...\"");

    }
}
