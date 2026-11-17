//! The Ruby values okf observes, and Ruby's text forms of them: `to_s`, `inspect`,
//! `JSON.generate` for strings, `Float#to_s`, `String#strip`, and `sub`'s replacement escapes.

use super::rtime::{Date, Time};
use std::fmt::Write as _;

/// A Ruby object as `YAML.safe_load` (or `JSON.parse`) hands it to okf.
#[derive(Clone, Debug)]
pub enum Val {
    Nil,
    Bool(bool),
    /// An Integer, in decimal (bignums included).
    Int(String),
    Float(f64),
    Str(String),
    /// An ASCII-8BIT string (`!!binary`).
    Bin(Vec<u8>),
    Date(Date),
    Time(Time),
    Array(Vec<Val>),
    /// A Hash in insertion order.
    Hash(Vec<(Val, Val)>),
    /// A reference back to an enclosing collection still being built (`&a [*a]`):
    /// true for a Hash, false for an Array.
    Recursive(bool),
}

/// An uncaught Ruby exception or a rescued Psych one: the class name and the message.
#[derive(Debug)]
pub struct Raise {
    pub class: String,
    pub message: String,
}

impl Raise {
    pub fn new(class: &str, message: impl Into<String>) -> Self {
        Self {
            class: class.to_string(),
            message: message.into(),
        }
    }

    /// Psych's exceptions are rescued by okf; everything else is uncaught.
    pub fn is_psych(&self) -> bool {
        self.class.starts_with("Psych::")
    }
}

impl Val {
    pub fn is_hash(&self) -> bool {
        matches!(self, Self::Hash(_) | Self::Recursive(true))
    }

    /// The bytes of a String (either encoding); `None` for anything else.
    pub fn string_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Str(s) => Some(s.as_bytes()),
            Self::Bin(b) => Some(b),
            _ => None,
        }
    }

    /// Ruby truthiness: only nil and false are falsy.
    pub fn truthy(&self) -> bool {
        !matches!(self, Self::Nil | Self::Bool(false))
    }

    /// `hash.fetch(key, nil)` for a String key; nil for a non-Hash.
    pub fn fetch(&self, key: &str) -> Option<&Val> {
        let Self::Hash(pairs) = self else {
            return None;
        };
        pairs
            .iter()
            .find(|(k, _)| k.string_bytes() == Some(key.as_bytes()))
            .map(|(_, v)| v)
    }
}

/// `Hash#[]=`: an `eql?` key keeps its place and takes the new value.
pub fn hash_set(pairs: &mut Vec<(Val, Val)>, key: Val, value: Val) {
    if let Some(slot) = pairs.iter_mut().find(|(k, _)| eql(k, &key)) {
        slot.1 = value;
        return;
    }
    pairs.push((key, value));
}

/// Ruby's `eql?` as Hash keys use it.
pub fn eql(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Nil, Val::Nil) => true,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Int(x), Val::Int(y)) => x == y,
        #[allow(clippy::float_cmp)] // Ruby's Float#eql? is exact equality.
        (Val::Float(x), Val::Float(y)) => x == y,
        (Val::Date(x), Val::Date(y)) => x == y,
        (Val::Time(x), Val::Time(y)) => x.same_instant(y),
        (Val::Array(x), Val::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| eql(p, q))
        }
        (Val::Hash(x), Val::Hash(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| eql(k, k2) && eql(v, v2)))
        }
        _ => match (a.string_bytes(), b.string_bytes()) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// `to_s`. Strings stay strings (a binary one stays binary); collections inspect.
pub fn to_s(v: &Val) -> Val {
    match v {
        Val::Nil => Val::Str(String::new()),
        Val::Str(_) | Val::Bin(_) => v.clone(),
        Val::Date(d) => Val::Str(d.to_s()),
        Val::Time(t) => Val::Str(t.to_s()),
        _ => Val::Str(inspect(v)),
    }
}

/// `to_s` as bytes, for comparisons.
pub fn to_s_bytes(v: &Val) -> Vec<u8> {
    match to_s(v) {
        Val::Str(s) => s.into_bytes(),
        Val::Bin(b) => b,
        _ => Vec::new(),
    }
}

/// `inspect`.
pub fn inspect(v: &Val) -> String {
    match v {
        Val::Nil => "nil".to_string(),
        Val::Bool(b) => b.to_string(),
        Val::Int(i) => i.clone(),
        Val::Float(f) => float_to_s(*f),
        Val::Str(s) => inspect_str(s),
        Val::Bin(b) => inspect_bin(b),
        Val::Date(d) => d.inspect(),
        Val::Time(t) => t.inspect(),
        Val::Array(items) => {
            let parts: Vec<String> = items.iter().map(inspect).collect();
            format!("[{}]", parts.join(", "))
        }
        Val::Hash(pairs) => {
            if pairs.is_empty() {
                return "{}".to_string();
            }
            let parts: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("{} => {}", inspect(k), inspect(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Val::Recursive(true) => "{...}".to_string(),
        Val::Recursive(false) => "[...]".to_string(),
    }
}

/// The escape Ruby's `inspect` uses for a special character, if any.
fn special_escape(c: char) -> Option<&'static str> {
    match c {
        '"' => Some("\\\""),
        '\\' => Some("\\\\"),
        '\n' => Some("\\n"),
        '\r' => Some("\\r"),
        '\t' => Some("\\t"),
        '\u{0C}' => Some("\\f"),
        '\u{0B}' => Some("\\v"),
        '\u{08}' => Some("\\b"),
        '\u{07}' => Some("\\a"),
        '\u{1B}' => Some("\\e"),
        _ => None,
    }
}

/// Ruby's printability for inspect, approximated without Unicode category tables:
/// controls, line/paragraph separators and noncharacters are escaped.
fn printable(c: char) -> bool {
    let cp = u32::from(c);
    if c.is_control() || cp == 0x2028 || cp == 0x2029 {
        return false;
    }
    let nonchar = (0xFDD0..=0xFDEF).contains(&cp) || (cp & 0xFFFE) == 0xFFFE;
    !nonchar
}

/// `String#inspect` for a UTF-8 string.
pub fn inspect_str(s: &str) -> String {
    let mut out = String::from("\"");
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' && matches!(chars.peek(), Some('{' | '$' | '@')) {
            out.push_str("\\#");
            continue;
        }
        if let Some(esc) = special_escape(c) {
            out.push_str(esc);
            continue;
        }
        if printable(c) {
            out.push(c);
            continue;
        }
        let cp = u32::from(c);
        if cp > 0xFFFF {
            let _ = write!(out, "\\u{{{cp:X}}}");
        } else {
            let _ = write!(out, "\\u{cp:04X}");
        }
    }
    out.push('"');
    out
}

/// `String#inspect` for an ASCII-8BIT string.
fn inspect_bin(b: &[u8]) -> String {
    let mut out = String::from("\"");
    for (i, &byte) in b.iter().enumerate() {
        let c = char::from(byte);
        let next = b.get(i + 1).copied();
        if byte == b'#' && matches!(next, Some(b'{' | b'$' | b'@')) {
            out.push_str("\\#");
            continue;
        }
        if let Some(esc) = special_escape(c) {
            out.push_str(esc);
            continue;
        }
        if (0x20..0x7F).contains(&byte) {
            out.push(c);
            continue;
        }
        let _ = write!(out, "\\x{byte:02X}");
    }
    out.push('"');
    out
}

/// `Float#to_s`: shortest round-trip digits, fixed notation for exponents -4..15.
pub fn float_to_s(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i64 = exp.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let decpt = exp + 1;
    if (-3..=15).contains(&decpt) {
        return format!("{sign}{}", fixed(&digits, decpt));
    }
    let (first, rest) = digits.split_at(1);
    let rest = if rest.is_empty() { "0" } else { rest };
    let esign = if exp < 0 { '-' } else { '+' };
    format!("{sign}{first}.{rest}e{esign}{:02}", exp.abs())
}

/// Digits placed around a decimal point `decpt` places from their start.
fn fixed(digits: &str, decpt: i64) -> String {
    let n = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    if decpt <= 0 {
        let zeros = "0".repeat(usize::try_from(-decpt).unwrap_or(0));
        return format!("0.{zeros}{digits}");
    }
    if decpt >= n {
        let zeros = "0".repeat(usize::try_from(decpt - n).unwrap_or(0));
        return format!("{digits}{zeros}.0");
    }
    let (int, frac) = digits.split_at(usize::try_from(decpt).unwrap_or(0));
    format!("{int}.{frac}")
}

/// `JSON.generate` of a String: `"` `\` and controls escaped, nothing else.
pub fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0C}' => out.push_str("\\f"),
            '\u{08}' => out.push_str("\\b"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON array of strings, as `JSON.generate` writes it.
pub fn json_str_array(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| json_str(s)).collect();
    format!("[{}]", parts.join(","))
}

/// Ruby's whitespace for `strip`: NUL, \t, \n, \v, \f, \r and space.
fn strip_char(c: char) -> bool {
    matches!(c, '\0' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ')
}

/// `String#strip`.
pub fn strip(s: &str) -> &str {
    s.trim_matches(strip_char)
}

/// `String#rstrip`.
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(strip_char)
}

/// Regexp `\s` in Ruby: ASCII whitespace only.
pub fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r')
}

/// `str.sub(re, replacement)` once the match `[start, end)` is known: the replacement's
/// backslash escapes (`\\`, `\0`, `\&`, `` \` ``, `\'`, `\1`..) as Ruby expands them for a
/// pattern without groups.
pub fn sub_at(s: &str, start: usize, end: usize, replacement: &str) -> String {
    let whole = &s[start..end];
    let mut expanded = String::new();
    let mut chars = replacement.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            expanded.push(c);
            continue;
        }
        match chars.next() {
            Some('0' | '&') => expanded.push_str(whole),
            Some('`') => expanded.push_str(&s[..start]),
            Some('\'') => expanded.push_str(&s[end..]),
            Some('\\') | None => expanded.push('\\'),
            Some(d) if d.is_ascii_digit() => {}
            Some(other) => {
                expanded.push('\\');
                expanded.push(other);
            }
        }
    }
    format!("{}{expanded}{}", &s[..start], &s[end..])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn floats_print_as_ruby_does() {
        assert_eq!(float_to_s(1e16), "1.0e+16");
        assert_eq!(float_to_s(1e15), "1.0e+15");
        assert_eq!(float_to_s(1e14), "100000000000000.0");
        assert_eq!(float_to_s(0.0001), "0.0001");
        assert_eq!(float_to_s(0.00001), "1.0e-05");
        assert_eq!(float_to_s(-0.0), "-0.0");
        assert_eq!(float_to_s(0.2), "0.2");
        assert_eq!(
            float_to_s(1.234_567_890_123_456e15),
            "1.234567890123456e+15"
        );
        assert_eq!(float_to_s(1e100), "1.0e+100");
        assert_eq!(float_to_s(f64::NEG_INFINITY), "-Infinity");
    }
    #[test]
    fn inspect_escapes_as_ruby_does() {
        assert_eq!(
            inspect_str("a\"b#{x}#$y\u{1}\u{7f}é"),
            "\"a\\\"b\\#{x}\\#$y\\u0001\\u007Fé\""
        );
        assert_eq!(
            inspect_str("\u{2028}\u{10FFFF}😀"),
            "\"\\u2028\\u{10FFFF}😀\""
        );
        assert_eq!(inspect_bin(b"a\x01\xe2\n"), "\"a\\x01\\xE2\\n\"");
        let hash = Val::Hash(vec![(
            Val::Str("a".into()),
            Val::Array(vec![Val::Float(1.5), Val::Nil]),
        )]);
        assert_eq!(inspect(&hash), "{\"a\" => [1.5, nil]}");
    }
    #[test]
    fn json_and_strip_match_ruby() {
        assert_eq!(
            json_str("a/\u{1f}\u{8}\u{7f}\u{2028}"),
            "\"a/\\u001f\\b\u{7f}\u{2028}\""
        );
        assert_eq!(strip(" \0\t x \0\u{b}"), "x");
        assert_eq!(rstrip("x\0 \t"), "x");
    }
    #[test]
    fn sub_expands_backslashes() {
        assert_eq!(sub_at("k: 1\nx", 0, 4, "k: \"a\\\\b\""), "k: \"a\\b\"\nx");
        assert_eq!(sub_at("k: 1", 0, 4, "\\&!\\q"), "k: 1!\\q");
    }
    #[test]
    fn hash_equality_and_fetch_follow_ruby_semantics() {
        let mut pairs = vec![(Val::Str("k".into()), Val::Int("1".into()))];
        hash_set(&mut pairs, Val::Str("k".into()), Val::Int("2".into()));
        assert!(eql(&Val::Int("1".into()), &Val::Int("1".into())));
        assert!(
            Val::Hash(pairs.clone())
                .fetch("k")
                .is_some_and(|v| matches!(v, Val::Int(s) if s == "2"))
        );
        let psych = Raise::new("Psych::SyntaxError", "bad");
        assert!(psych.is_psych());
        assert!(!Raise::new("TypeError", "x").is_psych());
    }
    #[test]
    fn inspect_covers_date_time_and_hash_fetch_miss() {
        use crate::okf::rtime::{self, Date};
        let d = Val::Date(Date::new(2026, 1, 2).unwrap_or_else(|| panic!("valid date")));
        assert!(inspect(&d).contains("Date"));
        let Some(inst) = rtime::iso8601("2026-01-01T00:00:00Z") else {
            panic!("timestamp");
        };
        assert!(inspect(&Val::Time(inst)).contains("UTC"));
        let h = Val::Hash(vec![(Val::Str("k".into()), Val::Int("1".into()))]);
        assert!(h.fetch("missing").is_none());
        assert!(inspect(&Val::Bin(vec![0xff])).contains("\\xFF"));
    }
    #[test]
    fn inspect_renders_collections_and_binary() {
        let arr = Val::Array(vec![Val::Recursive(false), Val::Bool(true)]);
        assert_eq!(inspect(&arr), "[[...], true]");
        let empty = Val::Hash(Vec::new());
        assert_eq!(inspect(&empty), "{}");
    }

    #[test]
    fn hash_set_replaces_existing_equal_key() {
        let mut pairs = vec![(Val::Str("k".into()), Val::Int("1".into()))];
        hash_set(&mut pairs, Val::Str("k".into()), Val::Int("9".into()));
        assert!(matches!(pairs[0].1, Val::Int(ref s) if s == "9"));
    }

    #[test]
    fn eql_returns_false_for_mismatched_values() {
        assert!(!eql(&Val::Int("1".into()), &Val::Int("2".into())));
        let h1 = Val::Hash(vec![(Val::Str("k".into()), Val::Int("1".into()))]);
        let h2 = Val::Hash(vec![(Val::Str("k".into()), Val::Int("2".into()))]);
        assert!(!eql(&h1, &h2));
    }

    #[test]
    fn eql_compares_arrays_dates_and_string_bytes() {
        use crate::okf::rtime::Date;
        let d1 = Val::Date(Date::new(2026, 1, 1).unwrap_or_else(|| panic!("date")));
        let d2 = Val::Date(Date::new(2026, 1, 1).unwrap_or_else(|| panic!("date")));
        assert!(eql(&d1, &d2));
        let a1 = Val::Array(vec![Val::Int("1".into())]);
        let a2 = Val::Array(vec![Val::Int("1".into())]);
        assert!(eql(&a1, &a2));
        assert_eq!(
            Val::Bin(b"x".to_vec()).string_bytes(),
            Some(b"x".as_slice())
        );
        assert!(Val::Int("1".into()).fetch("k").is_none());
        assert!(!Val::Nil.truthy());
        assert!(Val::Int("0".into()).truthy());
    }
}
