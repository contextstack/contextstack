//! The slice of Python 3 semantics the ported tools depend on: how a failure reaches stderr,
//! `int()`, truthiness, `str()`, `repr()` of a string, float `repr`, and `OSError` text.

use serde_json::Value;
use std::io;

/// Why a tool stopped with exit 1: the text Python writes to stderr, without the newline.
#[derive(Debug, PartialEq, Eq)]
pub struct Failure(pub String);

impl Failure {
    /// `sys.exit(msg)`: the message alone.
    pub fn exit(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    /// An uncaught exception: the last line of Python's traceback (the frames are not ported).
    pub fn raise(kind: &str, msg: &str) -> Self {
        Self(format!("{kind}: {msg}"))
    }
}

/// Python's type name for a JSON-decoded value.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `bool(v)`.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Magnitude cap for Python's unbounded ints: far beyond any line count, and sums stay in range.
const INT_CAP: i128 = 1 << 120;

/// `int(v)`, saturating at ±2^120 where Python's ints are unbounded.
pub fn int(v: &Value) -> Result<i128, Failure> {
    match v {
        Value::Bool(b) => Ok(i128::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Ok(i128::from(i));
            }
            if let Some(u) = n.as_u64() {
                return Ok(i128::from(u));
            }
            float_to_int(n.as_f64().unwrap_or(f64::NAN))
        }
        Value::String(s) => parse_int(s).ok_or_else(|| {
            let msg = format!("invalid literal for int() with base 10: {}", repr(s));
            Failure::raise("ValueError", &msg)
        }),
        other => {
            let msg = format!(
                "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                type_name(other)
            );
            Err(Failure::raise("TypeError", &msg))
        }
    }
}

#[allow(clippy::cast_possible_truncation)] // Truncation toward zero is what int(float) does.
fn float_to_int(f: f64) -> Result<i128, Failure> {
    if f.is_nan() {
        return Err(Failure::raise(
            "ValueError",
            "cannot convert float NaN to integer",
        ));
    }
    if f.is_infinite() {
        return Err(Failure::raise(
            "OverflowError",
            "cannot convert float infinity to integer",
        ));
    }
    Ok((f.trunc() as i128).clamp(-INT_CAP, INT_CAP))
}

/// Python's whitespace for `int()` stripping: Unicode `White_Space` plus the separators 0x1C-0x1F.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `int(str)` base 10: optional sign, ASCII digits with single underscores between them.
fn parse_int(s: &str) -> Option<i128> {
    let t = s.trim_matches(is_py_space);
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') {
        return None;
    }
    if digits.contains("__") {
        return None;
    }
    let mut value: i128 = 0;
    for c in digits.chars().filter(|c| *c != '_') {
        let d = c.to_digit(10)?;
        value = (value * 10 + i128::from(d)).min(INT_CAP);
    }
    Some(if negative { -value } else { value })
}

/// `str(v)` for a scalar; containers fall back to their JSON text (no path prints one).
pub fn str_of(v: &Value) -> String {
    match v {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(n) if n.is_f64() => float_repr(n.as_f64().unwrap_or(f64::NAN)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `repr(float)`: shortest round-trip digits, scientific below 1e-4 and from 1e16.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_owned();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let body = if (-4..16).contains(&exp) {
        fixed_notation(&digits, exp)
    } else {
        scientific_notation(&digits, exp)
    };
    format!("{sign}{body}")
}

fn fixed_notation(digits: &str, exp: i32) -> String {
    let Ok(int_len) = usize::try_from(exp + 1) else {
        let zeros = "0".repeat(usize::try_from(-exp - 1).unwrap_or(0));
        return format!("0.{zeros}{digits}");
    };
    if digits.len() <= int_len {
        let zeros = "0".repeat(int_len - digits.len());
        return format!("{digits}{zeros}.0");
    }
    format!("{}.{}", &digits[..int_len], &digits[int_len..])
}

fn scientific_notation(digits: &str, exp: i32) -> String {
    let (head, tail) = digits.split_at(1);
    let frac = if tail.is_empty() {
        String::new()
    } else {
        format!(".{tail}")
    };
    let exp_sign = if exp < 0 { '-' } else { '+' };
    format!("{head}{frac}e{exp_sign}{:02}", exp.unsigned_abs())
}

/// `repr(str)`: Python's quote choice and escapes. Printability is approximated: C0, DEL, C1,
/// the Unicode space and line separators, common format characters and private use are escaped.
pub fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_printable(c) => out.push(c),
            c => out.push_str(&escape_code_point(c)),
        }
    }
    out.push(quote);
    out
}

fn is_printable(c: char) -> bool {
    let cp = u32::from(c);
    let control = cp < 0x20 || (0x7f..=0xa0).contains(&cp);
    let separator = matches!(cp, 0x1680 | 0x2000..=0x200f | 0x2028..=0x202f | 0x205f..=0x2064);
    let other = matches!(cp, 0xad | 0xfeff | 0xe000..=0xf8ff | 0x3000 | 0xfff9..=0xfffb);
    !(control || separator || other)
}

fn escape_code_point(c: char) -> String {
    let cp = u32::from(c);
    if cp < 0x100 {
        format!("\\x{cp:02x}")
    } else if cp < 0x10000 {
        format!("\\u{cp:04x}")
    } else {
        format!("\\U{cp:08x}")
    }
}

/// `str(OSError)` for a failed call on `filename`: `[Errno N] strerror: 'filename'`.
pub fn os_error_text(e: &io::Error, filename: &str) -> String {
    let Some(code) = e.raw_os_error() else {
        return e.to_string();
    };
    let full = e.to_string();
    let suffix = format!(" (os error {code})");
    let strerror = full.strip_suffix(&suffix).unwrap_or(&full);
    format!("[Errno {code}] {strerror}: {}", repr(filename))
}

/// The uncaught `OSError` subclass Python raises for a failed open, read or write.
pub fn os_error(e: &io::Error, filename: &str) -> Failure {
    let kind = match e.raw_os_error() {
        Some(1 | 13) => "PermissionError",
        Some(2) => "FileNotFoundError",
        Some(17) => "FileExistsError",
        Some(20) => "NotADirectoryError",
        Some(21) => "IsADirectoryError",
        _ => "OSError",
    };
    Failure::raise(kind, &os_error_text(e, filename))
}

/// `a[key]` on a decoded JSON value, with Python's errors for a missing key or a non-dict.
pub fn getitem<'a>(a: &'a Value, key: &str) -> Result<&'a Value, Failure> {
    match a {
        Value::Object(map) => map
            .get(key)
            .ok_or_else(|| Failure::raise("KeyError", &repr(key))),
        Value::Array(_) => Err(Failure::raise(
            "TypeError",
            "list indices must be integers or slices, not str",
        )),
        Value::String(_) => Err(Failure::raise(
            "TypeError",
            "string indices must be integers, not 'str'",
        )),
        other => {
            let msg = format!("'{}' object is not subscriptable", type_name(other));
            Err(Failure::raise("TypeError", &msg))
        }
    }
}

/// `a.get(key)`: `None` for a missing key; `AttributeError` on a non-dict.
pub fn get<'a>(a: &'a Value, key: &str) -> Result<Option<&'a Value>, Failure> {
    match a {
        Value::Object(map) => Ok(map.get(key)),
        other => {
            let msg = format!("'{}' object has no attribute 'get'", type_name(other));
            Err(Failure::raise("AttributeError", &msg))
        }
    }
}

/// `os.fspath(p)` as `os.path.isabs` applies it: a str passes, anything else is a `TypeError`.
pub fn fspath(p: &Value) -> Result<&str, Failure> {
    match p {
        Value::String(s) => Ok(s),
        other => {
            let msg = format!(
                "expected str, bytes or os.PathLike object, not {}",
                type_name(other)
            );
            Err(Failure::raise("TypeError", &msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (1e16, "1e+16"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (123_456_789_012_345_678.0, "1.2345678901234568e+17"),
            (-0.0, "-0.0"),
            (100.0, "100.0"),
            (2.5, "2.5"),
            (1e15, "1000000000000000.0"),
            (1.5e300, "1.5e+300"),
        ];
        for (f, want) in cases {
            assert_eq!(float_repr(f), want, "{f}");
        }
    }
    #[test]
    fn int_follows_python() {
        assert_eq!(int(&json!(3)), Ok(3));
        assert_eq!(int(&json!(2.9)), Ok(2));
        assert_eq!(int(&json!(-2.9)), Ok(-2));
        assert_eq!(int(&json!(true)), Ok(1));
        assert_eq!(int(&json!(" +1_000 ")), Ok(1000));
        assert_eq!(
            int(&json!("1__0")),
            Err(Failure(
                "ValueError: invalid literal for int() with base 10: '1__0'".to_owned()
            ))
        );
        assert_eq!(
            int(&json!(null)),
            Err(Failure("TypeError: int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".to_owned()))
        );
    }
    #[test]
    fn repr_picks_quotes_like_python() {
        assert_eq!(repr("a'b"), "\"a'b\"");
        assert_eq!(repr("a'b\""), "'a\\'b\"'");
        assert_eq!(repr("x\ty\u{7}"), "'x\\ty\\x07'");
        assert_eq!(repr("é"), "'é'");
    }
    #[test]
    fn truthiness_and_str() {
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!(0.0)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!([0])));
        assert_eq!(str_of(&json!(5.0)), "5.0");
        assert_eq!(str_of(&json!(true)), "True");
    }
    #[test]
    fn getitem_errors_name_the_type() {
        assert_eq!(
            getitem(&json!([1]), "path"),
            Err(Failure(
                "TypeError: list indices must be integers or slices, not str".to_owned()
            ))
        );
        assert_eq!(
            getitem(&json!({}), "path"),
            Err(Failure("KeyError: 'path'".to_owned()))
        );
        assert_eq!(
            fspath(&json!(true)),
            Err(Failure(
                "TypeError: expected str, bytes or os.PathLike object, not bool".to_owned()
            ))
        );
    }
    #[test]
    fn os_error_maps_errno_subclasses() {
        use std::io;
        let e = io::Error::from_raw_os_error(2);
        assert_eq!(
            os_error(&e, "missing.txt"),
            Failure::raise("FileNotFoundError", &os_error_text(&e, "missing.txt"))
        );
        let e = io::Error::from_raw_os_error(21);
        assert_eq!(
            os_error(&e, "dir").0,
            "IsADirectoryError: [Errno 21] Is a directory: 'dir'"
        );
    }
    #[test]
    fn os_error_text_without_raw_errno_uses_display() {
        let e = std::io::Error::other("boom");
        assert_eq!(os_error_text(&e, "file"), "boom");
    }
    #[test]
    fn float_repr_handles_non_finite_values() {
        assert_eq!(float_repr(f64::NAN), "nan");
        assert_eq!(float_repr(f64::INFINITY), "inf");
        assert_eq!(float_repr(f64::NEG_INFINITY), "-inf");
    }
    #[test]
    fn getitem_and_get_reject_non_mapping_roots() {
        let text = json!("text");
        let list = json!([]);
        let err = getitem(&text, "k");
        assert!(matches!(err, Err(Failure(ref m)) if m.contains("string indices")));
        let err = get(&list, "k");
        assert!(matches!(err, Err(Failure(ref m)) if m.contains("has no attribute")));
        assert_eq!(type_name(&json!(1.0)), "float");
        assert_eq!(type_name(&json!([])), "list");
        assert_eq!(type_name(&json!({})), "dict");
        assert_eq!(type_name(&json!(null)), "NoneType");
    }
    #[test]
    fn int_accepts_large_unsigned_json_and_rejects_empty_string() {
        assert_eq!(
            int(&json!(18_446_744_073_709_551_615_u64)),
            Ok(18_446_744_073_709_551_615)
        );
        assert!(matches!(int(&json!("")), Err(Failure(ref m)) if m.starts_with("ValueError:")));
    }
    #[test]
    fn str_of_uses_json_for_containers_and_getitem_on_null() {
        assert_eq!(str_of(&json!([1, 2])), "[1,2]");
        assert_eq!(
            getitem(&json!(null), "k"),
            Err(Failure::raise(
                "TypeError",
                "'NoneType' object is not subscriptable"
            ))
        );
    }
    #[test]
    fn os_error_maps_permission_and_exists_like_python() {
        let e = io::Error::from_raw_os_error(13);
        assert!(os_error(&e, "x").0.starts_with("PermissionError:"));
        let e = io::Error::from_raw_os_error(17);
        assert!(os_error(&e, "x").0.starts_with("FileExistsError:"));
        let e = io::Error::from_raw_os_error(20);
        assert!(os_error(&e, "x").0.starts_with("NotADirectoryError:"));
    }
    #[test]
    fn truthy_treats_nonzero_int_and_nonempty_map_as_true() {
        assert!(truthy(&json!(-1)));
        assert!(truthy(&json!({"a": 1})));
        assert!(!truthy(&json!(null)));
    }

    #[test]
    fn repr_uses_lowercase_x_for_bmp_escapes() {
        assert!(repr("\x07").contains("\\x07"));
    }

    #[test]
    fn escape_code_point_uses_uppercase_u_for_supplementary_planes() {
        assert_eq!(escape_code_point('\u{10000}'), "\\U00010000");
    }

    #[test]
    fn repr_escapes_high_unicode_with_uppercase_u() {
        assert!(repr("\u{10FFFF}").contains("\u{10FFFF}"));
    }
}
