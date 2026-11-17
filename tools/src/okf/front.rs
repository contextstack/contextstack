//! okf's frontmatter text operations, each a hand match of the Ruby regex it ports
//! (`^` and `$` are line anchors in Ruby; `\s` is ASCII whitespace and crosses lines).

use super::rb::{is_space, json_str, rstrip, strip, sub_at};

/// `FRONTMATTER = /\A---\r?\n(.*?)\r?\n---\r?\n?/m`: the raw block and the body after it.
pub fn split(text: &str) -> (Option<&str>, &str) {
    let Some(after) = text.strip_prefix("---") else {
        return (None, text);
    };
    let start = if after.starts_with("\r\n") {
        5
    } else if after.starts_with('\n') {
        4
    } else {
        return (None, text);
    };
    let b = text.as_bytes();
    let close = (start..b.len()).find(|&p| b[p] == b'\n' && b[p + 1..].starts_with(b"---"));
    let Some(p) = close else {
        return (None, text);
    };
    let end = if p > start && b[p - 1] == b'\r' {
        p - 1
    } else {
        p
    };
    let mut k = p + 4;
    if b.get(k) == Some(&b'\r') {
        k += 1;
    }
    if b.get(k) == Some(&b'\n') {
        k += 1;
    }
    (Some(&text[start..end]), &text[k..])
}

/// `body.sub(/\A\r?\n+/, "")`.
pub fn trim_leading_newlines(body: &str) -> &str {
    let rest = body.strip_prefix('\r').unwrap_or(body);
    if rest.starts_with('\n') {
        rest.trim_start_matches('\n')
    } else {
        body
    }
}

/// Line-start offsets: 0 and after every `\n`.
fn line_starts(s: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(s.match_indices('\n').map(|(i, _)| i + 1))
}

/// The first match of `/^key\s*:/`: its start and the offset just past the colon.
fn key_colon(raw: &str, key: &str) -> Option<(usize, usize)> {
    line_starts(raw).find_map(|s| {
        let rest = raw[s..].strip_prefix(key)?;
        let blanks = rest.len() - rest.trim_start_matches(is_space).len();
        let colon = s + key.len() + blanks;
        (raw.as_bytes().get(colon) == Some(&b':')).then_some((s, colon + 1))
    })
}

/// `add_key`: prepend `key: rendered` unless a `key:` line is already there.
pub fn add_key(raw: Option<&str>, key: &str, rendered: &str) -> String {
    match raw {
        None | Some("") => format!("{key}: {rendered}"),
        Some(r) if key_colon(r, key).is_some() => r.to_string(),
        Some(r) => format!("{key}: {rendered}\n{r}"),
    }
}

/// `replace_key`: `raw.sub(/^key\s*:.*$/, "key: rendered")`, else `add_key`.
pub fn replace_key(raw: &str, key: &str, rendered: &str) -> String {
    let Some((start, after_colon)) = key_colon(raw, key) else {
        return add_key(Some(raw), key, rendered);
    };
    let end = raw[after_colon..]
        .find('\n')
        .map_or(raw.len(), |i| after_colon + i);
    sub_at(raw, start, end, &format!("{key}: {rendered}"))
}

/// `raw.sub(/^key\s*:/, "escape:")`.
pub fn rename_key(raw: &str, key: &str, escape: &str) -> String {
    match key_colon(raw, key) {
        Some((start, end)) => sub_at(raw, start, end, &format!("{escape}:")),
        None => raw.to_string(),
    }
}

/// `"---\n#{raw.rstrip}\n---\n\n#{body.sub(/\A\r?\n+/, "")}"`.
pub fn render(raw: &str, body: &str) -> String {
    format!(
        "---\n{}\n---\n\n{}",
        rstrip(raw),
        trim_leading_newlines(body)
    )
}

/// `String#lines`: pieces ending after each `\n`.
fn lines(s: &str) -> Vec<&str> {
    let mut out: Vec<&str> = s.split_inclusive('\n').collect();
    if s.is_empty() {
        out.clear();
    }
    out
}

/// `/\A(?:[>|][+-]?|[\[{].*[\]}])\z/`: a block scalar header or a flow collection.
fn block_or_flow(value: &str) -> bool {
    if matches!(value, ">" | "|" | ">+" | ">-" | "|+" | "|-") {
        return true;
    }
    let b = value.as_bytes();
    b.len() >= 2 && matches!(b[0], b'[' | b'{') && matches!(b[b.len() - 1], b']' | b'}')
}

/// `legacy_scalar`: unquote a single- or double-quoted value, as Ruby's JSON would.
fn legacy_scalar(value: &str) -> String {
    let text = strip(value);
    let n = text.len();
    if n >= 2 && text.starts_with('\'') && text.ends_with('\'') {
        return text[1..n - 1].replace("''", "'");
    }
    if n >= 2 && text.starts_with('"') && text.ends_with('"') {
        return serde_json::from_str::<String>(text).unwrap_or_else(|_| text[1..n - 1].to_string());
    }
    text.to_string()
}

/// `repair_legacy_yaml`: quote each top-level `key: scalar` line as a JSON string.
pub fn repair_legacy(raw: &str) -> String {
    let mut out = String::new();
    for line in lines(raw) {
        out.push_str(&repair_line(line));
    }
    out
}

/// `/^([A-Za-z0-9_-]+):[ \t]*(.*?)(\r?\n)?$/` on one line, and its rewrite.
fn repair_line(line: &str) -> String {
    let key_len = line
        .bytes()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
        .count();
    if key_len == 0 || line.as_bytes().get(key_len) != Some(&b':') {
        return line.to_string();
    }
    let rest = line[key_len + 1..].trim_start_matches([' ', '\t']);
    let newline = if rest.ends_with("\r\n") {
        "\r\n"
    } else if rest.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let value = &rest[..rest.len() - newline.len()];
    if value.is_empty() || block_or_flow(value) {
        return line.to_string();
    }
    let key = &line[..key_len];
    format!("{key}: {}{newline}", json_str(&legacy_scalar(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_splits_as_the_regex_does() {
        assert_eq!(split("---\na: 1\n---\nbody"), (Some("a: 1"), "body"));
        assert_eq!(
            split("---\r\na: 1\r\n---\r\n\r\nbody"),
            (Some("a: 1"), "\r\nbody")
        );
        assert_eq!(split("---\n---\n"), (None, "---\n---\n"));
        assert_eq!(split("---\n\n---\nx"), (Some(""), "x"));
        assert_eq!(split("---\n---\nfoo\n----x"), (Some("---\nfoo"), "-x"));
        assert_eq!(split("x\n---\n"), (None, "x\n---\n"));
        assert_eq!(split("---not-frontmatter"), (None, "---not-frontmatter"));
    }

    #[test]
    fn keys_are_added_replaced_and_renamed() {
        assert_eq!(add_key(None, "type", "\"T\""), "type: \"T\"");
        assert_eq!(
            add_key(Some("title: x"), "type", "\"T\""),
            "type: \"T\"\ntitle: x"
        );
        assert_eq!(add_key(Some("type : x"), "type", "\"T\""), "type : x");
        assert_eq!(
            replace_key(
                "a: 1\nschema_version: \"0.1\"\r\nb",
                "schema_version",
                "\"0.2\""
            ),
            "a: 1\nschema_version: \"0.2\"\nb"
        );
        assert_eq!(
            rename_key("status: weird", "status", "legacy_status"),
            "legacy_status: weird"
        );
        assert_eq!(
            replace_key("title: only", "schema_version", "\"0.2\""),
            "schema_version: \"0.2\"\ntitle: only"
        );
        assert_eq!(
            rename_key("title: only", "missing", "legacy_status"),
            "title: only"
        );
        assert_eq!(trim_leading_newlines("\r\n\n\nx"), "x");
        assert_eq!(trim_leading_newlines("\r\r\nx"), "\r\r\nx");
    }

    #[test]
    fn lines_returns_empty_for_empty_string() {
        assert_eq!(lines(""), Vec::<&str>::new());
        assert!(block_or_flow("[a]"));
        assert!(block_or_flow("|"));
        assert!(block_or_flow(">-"));
    }

    #[test]
    fn legacy_lines_are_quoted() {
        assert_eq!(
            repair_legacy("title: It's: odd\nlist: [a]\nq: 'a''b'\nd: \"\\q\"\r\n  nested: x\n"),
            "title: \"It's: odd\"\nlist: [a]\nq: \"a'b\"\nd: \"\\\\q\"\r\n  nested: x\n"
        );
    }
}
