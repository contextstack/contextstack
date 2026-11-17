//! Surgical edits to a client's spec: the Director's stamp (`intent:`, `stamp.rs`) and a reservation (`budget.usd`,
//! `budget.rs`) change only the lines they must. Every other byte stays as the client sent it: comments, key order,
//! quoting, and the budget's flow (`{turns: 30, usd: 0}`) or block style. The node's executor and runner read the
//! spec the client wrote, not a re-serialisation of it (2026-11-11: a block-style rewrite broke the executor).
//!
//! Each edit is checked by parsing: the result must parse to the original's values plus the edited one. A spec too
//! unusual to edit this way (a key written in another form, an anchor, alias or tag on an edited entry, a second
//! document) is refused with the reason, never rewritten.

use serde_yaml::{Mapping, Value};

/// The spec as text, split into lines that keep their line endings.
struct Lines<'a> {
    text: &'a str,
    /// Byte offset of each line's start; one past the last line is `text.len()`.
    starts: Vec<usize>,
    /// The spec's line ending, for the lines an edit writes.
    eol: &'static str,
}

impl<'a> Lines<'a> {
    fn new(spec: &'a [u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(spec).map_err(|_| "the spec is not UTF-8".to_string())?;
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        if starts.last() == Some(&text.len()) {
            starts.pop();
        }
        let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let lines = Self { text, starts, eol };
        lines.one_document()?;
        Ok(lines)
    }

    fn count(&self) -> usize {
        self.starts.len()
    }

    fn start(&self, i: usize) -> usize {
        self.starts.get(i).copied().unwrap_or(self.text.len())
    }

    /// Line `i` without its line ending.
    fn line(&self, i: usize) -> &'a str {
        self.text[self.start(i)..self.start(i + 1)].trim_end_matches(['\n', '\r'])
    }

    /// A document marker anywhere but the first line means more than one document (or an end marker an append
    /// would land after).
    fn one_document(&self) -> Result<(), String> {
        let marker =
            |l: &str| l == "---" || l == "..." || l.starts_with("--- ") || l.starts_with("... ");
        if (1..self.count()).any(|i| marker(self.line(i))) {
            return Err(refusal(
                "it has a YAML document marker after its first line",
            ));
        }
        Ok(())
    }

    /// The top-level key line `i` names, if it is a plain `key:` line at column 0.
    fn key(&self, i: usize) -> Option<&'a str> {
        let l = self.line(i);
        let name_end = l
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
            .unwrap_or(l.len());
        let (name, rest) = l.split_at(name_end);
        let rest = rest.trim_start_matches([' ', '\t']);
        let after = rest.strip_prefix(':')?;
        (!name.is_empty()
            && !name.starts_with(['-', '.'])
            && (after.is_empty() || after.starts_with([' ', '\t'])))
        .then_some(name)
    }

    /// The one entry `key` heads: the line index of `key:` and one past its value's last line. A key the parsed
    /// spec has but no single plain line writes (quoted, `? key`, inside a flow mapping) is refused.
    fn entry(&self, key: &str) -> Result<(usize, usize), String> {
        let heads: Vec<usize> = (0..self.count())
            .filter(|&i| self.key(i) == Some(key))
            .collect();
        let [head] = heads[..] else {
            return Err(refusal(&format!(
                "its {key} key is not written as one plain `{key}:` line"
            )));
        };
        let mut end = head + 1;
        for i in head + 1..self.count() {
            let l = self.line(i);
            let t = l.trim_start();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if l.starts_with([' ', '\t']) || (l.starts_with('-') && !l.starts_with("---")) {
                end = i + 1;
            } else {
                break;
            }
        }
        Ok((head, end))
    }

    /// The text with lines `[from, to)` replaced by `with`.
    fn splice(&self, from: usize, to: usize, with: &str) -> String {
        let (a, b) = (self.start(from), self.start(to));
        let mut out = String::with_capacity(self.text.len() + with.len());
        out.push_str(&self.text[..a]);
        out.push_str(with);
        out.push_str(&self.text[b..]);
        out
    }

    /// The text with `line` added as a new last line.
    fn append(&self, line: &str) -> String {
        let mut out = self.text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(self.eol);
        }
        out.push_str(line);
        out.push_str(self.eol);
        out
    }
}

fn refusal(why: &str) -> String {
    format!(
        "the Director cannot edit this spec in place: {why}; write it as plain `key: value` lines"
    )
}

/// The spec parsed: it must be a YAML mapping.
pub fn parse(spec: &[u8]) -> Result<Mapping, String> {
    match serde_yaml::from_slice(spec).map_err(|e| format!("the spec is not YAML: {e}"))? {
        Value::Mapping(m) => Ok(m),
        _ => Err("the spec is not a YAML mapping".into()),
    }
}

/// `edited` must parse to `expected`: the original's values plus the one the edit sets.
fn checked(edited: String, expected: &Mapping) -> Result<Vec<u8>, String> {
    match parse(edited.as_bytes()) {
        Ok(m) if &m == expected => Ok(edited.into_bytes()),
        _ => Err(refusal(
            "an edit in place would change more than the stamped value",
        )),
    }
}

/// Refuse an anchor (`&a`), alias (`*a`) or tag (`!t`) in a value the edit touches.
fn plain(value: &str, key: &str) -> Result<(), String> {
    if value.trim_start().starts_with(['&', '*', '!']) {
        return Err(refusal(&format!("its {key} uses an anchor, alias or tag")));
    }
    Ok(())
}

/// The spec with the top-level `key` set to the plain scalar `value`: its entry replaced by `key: value`, or that
/// line appended when it has none.
pub fn set_top(spec: &[u8], key: &str, value: &str) -> Result<Vec<u8>, String> {
    let lines = Lines::new(spec)?;
    let mut expected = parse(spec)?;
    let line = format!("{key}: {value}");
    let edited = if expected.contains_key(key) {
        let (head, end) = lines.entry(key)?;
        lines.splice(head, end, &format!("{line}{}", lines.eol))
    } else {
        lines.append(&line)
    };
    expected.insert(Value::from(key), Value::from(value));
    checked(edited, &expected)
}

/// Micro-dollars as a YAML float that both `serde_yaml` and the node's YAML 1.1 reader take as one: always a decimal
/// point, never an exponent (`1e3` is a string to YAML 1.1).
pub fn usd_text(micros: i64) -> String {
    let (whole, frac) = (micros / 1_000_000, (micros % 1_000_000).abs());
    let frac = format!("{frac:06}");
    let frac = frac.trim_end_matches('0');
    format!("{whole}.{}", if frac.is_empty() { "0" } else { frac })
}

/// The spec with `budget.usd` set to `micros`, in the budget's own style: inside a one-line flow mapping, as a line
/// of a block mapping, or a new `budget: {usd: X}` line when the spec has no budget mapping.
pub fn set_budget_usd(spec: &[u8], micros: i64) -> Result<Vec<u8>, String> {
    let lines = Lines::new(spec)?;
    let mut expected = parse(spec)?;
    let usd = usd_text(micros);
    let fresh = format!("budget: {{usd: {usd}}}");
    let edited = match expected.get("budget") {
        None => lines.append(&fresh),
        Some(Value::Mapping(_)) => {
            let (head, end) = lines.entry("budget")?;
            let rest = lines.line(head).split_once(':').map_or("", |(_, r)| r);
            plain(rest, "budget")?;
            let value = rest.trim();
            if value.starts_with('{') {
                if end != head + 1 {
                    return Err(refusal(
                        "its budget is a flow mapping over more than one line",
                    ));
                }
                let new = flow_with_usd(lines.line(head), &usd)?;
                lines.splice(head, end, &format!("{new}{}", lines.eol))
            } else if value.is_empty() || value.starts_with('#') {
                block_with_usd(&lines, head, end, &usd)?
            } else {
                return Err(refusal(
                    "its budget is written in a form the Director does not edit",
                ));
            }
        }
        // As before this edit was surgical: a budget that is not a mapping becomes one holding only `usd`.
        Some(_) => {
            let (head, end) = lines.entry("budget")?;
            lines.splice(head, end, &format!("{fresh}{}", lines.eol))
        }
    };
    if !expected.get("budget").is_some_and(Value::is_mapping) {
        expected.insert(Value::from("budget"), Value::Mapping(Mapping::new()));
    }
    let budget = expected
        .get_mut("budget")
        .and_then(Value::as_mapping_mut)
        .ok_or("the spec's budget is not a mapping")?;
    #[allow(clippy::cast_precision_loss, reason = "money is far below 2^53 micros")]
    budget.insert(Value::from("usd"), Value::from(micros as f64 / 1e6));
    checked(edited, &expected)
}

/// `budget: {…}` with its `usd` item's value replaced, or `usd` added as the last item; spacing kept.
fn flow_with_usd(line: &str, usd: &str) -> Result<String, String> {
    let (Some(open), Some(close)) = (line.find('{'), line.rfind('}')) else {
        return Err(refusal(
            "its budget flow mapping does not close on its line",
        ));
    };
    let inner = &line[open + 1..close];
    if close < open || inner.contains(['{', '[', '"', '\'', '#', '}']) {
        return Err(refusal("its budget flow mapping nests, quotes or comments"));
    }
    let items: Vec<&str> = inner.split(',').collect();
    let at = items
        .iter()
        .position(|it| it.split(':').next().map(str::trim) == Some("usd"));
    let inner = if let Some(at) = at {
        let item = items[at];
        let (k, v) = item
            .split_once(':')
            .ok_or_else(|| refusal("its budget usd item has no value"))?;
        plain(v, "budget.usd")?;
        let lead = &v[..v.len() - v.trim_start().len()];
        let trail = &v[v.trim_end().len()..];
        let lead = if lead.is_empty() { " " } else { lead };
        let mut items: Vec<String> = items.iter().map(|s| (*s).to_string()).collect();
        items[at] = format!("{k}:{lead}{usd}{trail}");
        items.join(",")
    } else if inner.trim().is_empty() {
        format!("usd: {usd}")
    } else {
        let body = inner.trim_end();
        format!("{body}, usd: {usd}{}", &inner[body.len()..])
    };
    Ok(format!(
        "{}{{{inner}}}{}",
        &line[..open],
        &line[close + 1..]
    ))
}

/// The block mapping under `budget:` (lines `head+1..end`) with its `usd:` line's value replaced, trailing comment
/// kept, or a `usd:` line added after its last line at its indent.
fn block_with_usd(lines: &Lines, head: usize, end: usize, usd: &str) -> Result<String, String> {
    let children = || {
        (head + 1..end).filter(|&i| {
            let t = lines.line(i).trim_start();
            !t.is_empty() && !t.starts_with('#')
        })
    };
    let first = children()
        .next()
        .ok_or_else(|| refusal("its budget has no lines"))?;
    let l = lines.line(first);
    let indent = &l[..l.len() - l.trim_start().len()];
    if indent.is_empty() || indent.contains('\t') {
        return Err(refusal("its budget is not indented with spaces"));
    }
    let at_indent = |i: usize| {
        let l = lines.line(i);
        l.strip_prefix(indent)
            .filter(|r| !r.starts_with([' ', '\t']))
    };
    let usd_lines: Vec<usize> = children()
        .filter(|&i| {
            at_indent(i).is_some_and(|r| {
                r.strip_prefix("usd")
                    .map(|r| r.trim_start_matches([' ', '\t']))
                    .and_then(|r| r.strip_prefix(':'))
                    .is_some_and(|a| a.is_empty() || a.starts_with([' ', '\t']))
            })
        })
        .collect();
    match usd_lines[..] {
        [] => {
            let last = children().next_back().unwrap_or(first);
            Ok(lines.splice(
                last + 1,
                last + 1,
                &format!("{indent}usd: {usd}{}", lines.eol),
            ))
        }
        [i] => {
            let l = lines.line(i);
            let (k, v) = l
                .split_once(':')
                .ok_or_else(|| refusal("its budget usd line has no value"))?;
            plain(v, "budget.usd")?;
            let comment = v.find(" #").map_or("", |c| &v[v[..c].trim_end().len()..]);
            Ok(lines.splice(i, i + 1, &format!("{k}: {usd}{comment}{}", lines.eol)))
        }
        _ => Err(refusal("its budget has more than one usd line")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(b: &[u8]) -> Value {
        serde_yaml::from_slice(b).unwrap_or_default()
    }

    fn text(b: Result<Vec<u8>, String>) -> Result<String, String> {
        String::from_utf8(b?).map_err(|e| e.to_string())
    }

    const CLIENT: &str = "# a coding job\nid: web-1  # client id\nmodel: claude-code-sonnet\nbudget: {turns: 30, tool_calls: 60, usd: 0, seconds: 900}\ntasks: [tasks/many]\nhypothesis: 'Sonnet edits \"x\": fine'\n";

    #[test]
    fn inline_budget_kept_inline() -> Result<(), String> {
        let out = text(set_budget_usd(CLIENT.as_bytes(), 1_250_000))?;
        assert_eq!(out, CLIENT.replace("usd: 0,", "usd: 1.25,"));
        let added = text(set_budget_usd(b"budget: {turns: 3}\n", 2_000_000))?;
        assert_eq!(added, "budget: {turns: 3, usd: 2.0}\n");
        assert_eq!(
            text(set_budget_usd(b"budget: {}\n", 1))?,
            "budget: {usd: 0.000001}\n"
        );
        Ok(())
    }

    #[test]
    fn block_budget_kept_block() -> Result<(), String> {
        let spec = "id: x\nbudget:   # limits\n  turns: 30\n  usd: 0   # filled in\n  seconds: 900\ntasks:\n- tasks/many\n";
        let out = text(set_budget_usd(spec.as_bytes(), 500_000))?;
        assert_eq!(out, spec.replace("usd: 0   #", "usd: 0.5   #"));
        let none = "budget:\n  turns: 30\n\n  # trailing\ntrials: 2\n";
        let out = text(set_budget_usd(none.as_bytes(), 500_000))?;
        assert_eq!(
            out,
            "budget:\n  turns: 30\n  usd: 0.5\n\n  # trailing\ntrials: 2\n"
        );
        Ok(())
    }

    #[test]
    fn no_budget_gets_one_line() -> Result<(), String> {
        assert_eq!(
            text(set_budget_usd(b"id: y\n", 500_000))?,
            "id: y\nbudget: {usd: 0.5}\n"
        );
        assert_eq!(
            text(set_budget_usd(b"id: y", 500_000))?,
            "id: y\nbudget: {usd: 0.5}\n"
        );
        assert_eq!(
            text(set_budget_usd(b"id: y\nbudget:\n", 500_000))?,
            "id: y\nbudget: {usd: 0.5}\n"
        );
        Ok(())
    }

    #[test]
    fn top_key_appended_or_replaced() -> Result<(), String> {
        let out = text(set_top(CLIENT.as_bytes(), "intent", "01M74"))?;
        assert_eq!(out, format!("{CLIENT}intent: 01M74\n"));
        let old = "id: x\nintent: OLD\nmodel: m\n";
        assert_eq!(
            text(set_top(old.as_bytes(), "intent", "NEW"))?,
            "id: x\nintent: NEW\nmodel: m\n"
        );
        let crlf = "id: x\r\nmodel: m\r\n";
        assert_eq!(
            text(set_top(crlf.as_bytes(), "intent", "N"))?,
            "id: x\r\nmodel: m\r\nintent: N\r\n"
        );
        Ok(())
    }

    #[test]
    fn edits_parse_to_the_same_values_plus_the_stamp() -> Result<(), String> {
        let out = set_top(CLIENT.as_bytes(), "intent", "01M74")?;
        let out = set_budget_usd(&out, 3_000_000)?;
        let mut want = doc(CLIENT.as_bytes());
        want["intent"] = Value::from("01M74");
        want["budget"]["usd"] = Value::from(3.0);
        assert_eq!(doc(&out), want);
        Ok(())
    }

    #[test]
    fn unusual_specs_are_refused_with_the_reason() {
        for (spec, why) in [
            ("id: x\n---\nid: y\n", "document marker"),
            ("\"budget\": {usd: 1}\n", "budget key"),
            ("budget: &b {usd: 1}\n", "anchor"),
            ("budget: {turns: 3,\n  usd: 1}\n", "more than one line"),
            ("budget: {nested: {a: 1}}\n", "nests"),
            ("budget:\n  usd: *u\nother: &u 2\n", "anchor"),
        ] {
            match set_budget_usd(spec.as_bytes(), 1_000_000) {
                Err(e) => assert!(e.contains(why), "{spec:?}: {e}"),
                Ok(o) => panic!("{spec:?} edited: {}", String::from_utf8_lossy(&o)),
            }
        }
        assert!(set_top(b"\"intent\": x\n", "intent", "y").is_err());
    }

    #[test]
    fn usd_text_is_a_plain_decimal() {
        assert_eq!(usd_text(1), "0.000001");
        assert_eq!(usd_text(2_000_000), "2.0");
        assert_eq!(usd_text(1_250_000), "1.25");
        assert_eq!(usd_text(12_345_678_901), "12345.678901");
    }
}
