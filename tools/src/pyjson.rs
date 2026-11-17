//! `json.load(sys.stdin)` and `json.dumps(v, sort_keys=True)` with Python's default spacing,
//! `ensure_ascii` escapes and float rendering.

use crate::py::{self, Failure};
use serde_json::{Number, Value};
use std::fmt::Write as _;

/// Decode the tool's stdin as Python does, failing with the `JSONDecodeError` line.
pub fn load(input: &[u8]) -> Result<Value, Failure> {
    let Ok(text) = std::str::from_utf8(input) else {
        return Err(Failure::raise(
            "UnicodeDecodeError",
            "stdin is not valid UTF-8 (Python's UTF-8 mode would surrogate-escape it; not ported)",
        ));
    };
    if text.starts_with('\u{feff}') {
        return Err(decode_error(
            "Unexpected UTF-8 BOM (decode using utf-8-sig)",
            text,
            0,
        ));
    }
    let rest = text.trim_start_matches([' ', '\t', '\n', '\r']);
    if rest.is_empty() {
        return Err(decode_error("Expecting value", text, text.len()));
    }
    serde_json::from_str(text)
        .map_err(|e| Failure::raise("json.decoder.JSONDecodeError", &e.to_string()))
}

/// Python's `JSONDecodeError` text for a position known to sit after ASCII-only bytes.
fn decode_error(msg: &str, text: &str, pos: usize) -> Failure {
    let before = &text[..pos];
    let line = before.matches('\n').count() + 1;
    let column = match before.rfind('\n') {
        Some(nl) => pos - nl,
        None => pos + 1,
    };
    let detail = format!("{msg}: line {line} column {column} (char {pos})");
    Failure::raise("json.decoder.JSONDecodeError", &detail)
}

/// `json.dumps(v, sort_keys=True)`.
pub fn dumps_sorted(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(n, out),
        Value::String(s) => write_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str(key, out);
                out.push_str(": ");
                write_value(value, out);
            }
            out.push('}');
        }
    }
}

fn write_number(n: &Number, out: &mut String) {
    if !n.is_f64() {
        out.push_str(&n.to_string());
        return;
    }
    let f = n.as_f64().unwrap_or(f64::NAN);
    if f.is_nan() {
        out.push_str("NaN");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        out.push_str(&py::float_repr(f));
    }
}

/// A JSON string with `ensure_ascii=True`: everything outside space..tilde is escaped.
fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dumps_matches_python_spacing_and_escapes() {
        let v = json!({"b": 1, "a": [1.0, "é\u{7f}😀", null, true], "c": {}});
        assert_eq!(
            dumps_sorted(&v),
            "{\"a\": [1.0, \"\\u00e9\\u007f\\ud83d\\ude00\", null, true], \"b\": 1, \"c\": {}}"
        );
    }

    #[test]
    fn load_reports_python_positions() {
        assert_eq!(
            load(b""),
            Err(Failure(
                "json.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)"
                    .to_owned()
            ))
        );
        assert_eq!(
            load(b"  \n "),
            Err(Failure(
                "json.decoder.JSONDecodeError: Expecting value: line 2 column 2 (char 4)"
                    .to_owned()
            ))
        );
    }

    #[test]
    fn floats_and_ints_keep_their_kind() {
        let v = load(b"[1, 1.0, 1E5, -0.0, 12345678901234567890]");
        assert_eq!(
            v.map(|v| dumps_sorted(&v)),
            Ok("[1, 1.0, 100000.0, -0.0, 12345678901234567890]".to_owned())
        );
    }

    #[test]
    fn load_rejects_bom_and_non_utf8_stdin() {
        let err = load(b"\xef\xbb\xbf{}");
        assert!(matches!(err, Err(Failure(ref m)) if m.contains("UTF-8 BOM")));
        let err = load(b"\xff");
        assert!(matches!(err, Err(Failure(ref m)) if m.contains("UnicodeDecodeError")));
    }

    #[test]
    fn dumps_escapes_backspace_and_formfeed_in_strings() {
        let v = serde_json::json!({"x": "\u{8}\u{c}"});
        assert_eq!(dumps_sorted(&v), "{\"x\": \"\\b\\f\"}");
    }

    #[test]
    fn dumps_formats_false_bool_literal() {
        assert_eq!(dumps_sorted(&json!(false)), "false");
    }

}
