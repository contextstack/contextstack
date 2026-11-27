//! Tokens shared by parameter binding and the scoped SQL guard.

use std::ops::Range;

pub fn tokens(mut rest: &str) -> impl Iterator<Item = (Range<usize>, &str)> {
    let length = rest.len();
    std::iter::from_fn(move || {
        loop {
            rest = rest.trim_start();
            let start = length - rest.len();
            let first = *rest.as_bytes().first()?;
            let width = match first {
                b'-' if rest.starts_with("--") => rest.find('\n').unwrap_or(rest.len()),
                b'/' if rest.starts_with("/*") => {
                    rest[2..].find("*/").map_or(rest.len(), |at| at + 4)
                }
                quote @ (b'\'' | b'"' | b'`' | b'[' | b'{') => quoted(rest, quote),
                b'?' => 1 + rest[1..].bytes().take_while(u8::is_ascii_digit).count(),
                b':' | b'@' | b'$' => 1 + identifier_len(&rest[1..]),
                byte if byte.is_ascii_alphanumeric() || byte == b'_' => identifier_len(rest),
                _ => rest.chars().next()?.len_utf8(),
            };
            let (token, remaining) = rest.split_at(width);
            rest = remaining;
            if !token.starts_with("--") && !token.starts_with("/*") {
                return Some((start..start + width, token));
            }
        }
    })
}

fn identifier_len(sql: &str) -> usize {
    sql.bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        .count()
}

fn quoted(sql: &str, opening: u8) -> usize {
    let closing = match opening {
        b'[' => b']',
        b'{' => b'}',
        other => other,
    };
    let bytes = sql.as_bytes();
    let mut at = 1;
    while at < bytes.len() {
        if bytes[at] == closing {
            if opening != closing || bytes.get(at + 1) != Some(&closing) {
                return at + 1;
            }
            at += 1;
        }
        at += 1;
    }
    bytes.len()
}
