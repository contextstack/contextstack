//! `YAML.safe_load` as Psych 5.3 runs it: okf's `permitted_classes: [Date, Time], aliases: true`
//! or the bare call's defaults (`yaml-json`): libyaml 0.2.5's events (through `libyaml-safer`, a
//! safe port of the same C) up to the end of the first document, then `Visitors::ToRuby` (or
//! `NoAliasRuby`) with the restricted class loader.

use super::rb::{Raise, Val, hash_set};
use super::scanner::{disallowed, ruby_float, tokenize};
use super::utf8::Gate;
use libyaml_safer::{Encoding, ErrorKind, EventData, Parser, ScalarStyle};
use std::cell::Cell;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};

/// Ruby's stack gives out somewhere; nesting this deep is reported as it would be.
const MAX_DEPTH: usize = 2_000;

/// `safe_load`'s keyword arguments that differ between its callers.
#[derive(Clone, Copy)]
pub struct Options {
    /// `permitted_classes: [Date, Time]`; otherwise a date or time is `DisallowedClass`.
    pub date_time: bool,
    /// `aliases: true`; otherwise any alias is `AliasesNotEnabled`.
    pub aliases: bool,
}

impl Options {
    /// okf's call.
    pub const OKF: Self = Self {
        date_time: true,
        aliases: true,
    };
    /// `YAML.safe_load(text)` with no keywords.
    pub const BARE: Self = Self {
        date_time: false,
        aliases: false,
    };
}

/// A Psych AST node.
enum Node {
    Scalar {
        anchor: Option<String>,
        tag: Option<String>,
        value: String,
        quoted: bool,
    },
    Seq {
        anchor: Option<String>,
        tag: Option<String>,
        items: Vec<Node>,
    },
    Map {
        anchor: Option<String>,
        tag: Option<String>,
        items: Vec<Node>,
    },
    Alias(String),
}

impl Node {
    fn tag(&self) -> Option<&str> {
        match self {
            Self::Scalar { tag, .. } | Self::Seq { tag, .. } | Self::Map { tag, .. } => {
                tag.as_deref()
            }
            Self::Alias(_) => None,
        }
    }
}

/// okf's `YAML.safe_load`: `Ok(None)` for an empty stream, `Err` for a Psych exception or
/// an uncaught one.
pub fn safe_load(raw: &str) -> Result<Option<Val>, Raise> {
    load(raw.as_bytes(), Options::OKF)
}

/// `YAML.safe_load` of a Ruby UTF-8 string's bytes (invalid UTF-8 is libyaml's to refuse).
pub fn load(raw: &[u8], opts: Options) -> Result<Option<Val>, Raise> {
    let Some(root) = first_document(raw)? else {
        return Ok(None);
    };
    let mut visitor = ToRuby {
        anchors: HashMap::new(),
        opts,
    };
    visitor.accept(&root, 0).map(Some)
}

/// `Psych::SyntaxError`'s message from a libyaml error: problem and context, and the context
/// mark (zero when libyaml left it unset). `shifts` are the (line, column) marks of spaces
/// the port inserted (see `first_document`); columns after them on their line move back.
fn syntax_error(e: &libyaml_safer::Error, shifts: &[(u64, u64)]) -> Raise {
    let mark = match (e.kind(), e.context_mark()) {
        (_, Some(m)) => Some(m),
        (ErrorKind::Scanner, None) => e.problem_mark(),
        _ => None,
    };
    let (line, column) = mark.map_or((1, 1), |m| {
        let before = shifts.iter().filter(|(l, c)| *l == m.line && *c < m.column);
        let shift = u64::try_from(before.count()).unwrap_or(0);
        (m.line + 1, m.column - shift + 1)
    });
    let err = match e.context() {
        Some(context) => format!("{} {context}", e.problem()),
        None => e.problem().to_string(),
    };
    let message = format!("(<unknown>): {err} at line {line} column {column}");
    Raise::new("Psych::SyntaxError", message)
}

/// Parse up to the end of the first document, as `Psych.parse` returns on it.
///
/// libyaml-safer 0.3.0 panics in two places where C libyaml 0.2.5 does not, and both are
/// caught and worked around so the result is libyaml's:
/// - "unexpected end of input": a block scalar's last line runs to the end of the input
///   (okf's frontmatter never ends in a newline, so `note: |` last hits it). C's `READ_LINE`
///   is a no-op there. Retried with a newline appended; the one block scalar that reached
///   it then loses the newline that gained it.
/// - "TODO: What is expected here?": a tag followed directly by `,` in a flow collection
///   (`{by: !x, at: y}`), which C accepts. Retried with a space before that comma: the same
///   tokens and values; only error columns later on that line could differ.
fn first_document(raw: &[u8]) -> Result<Option<Node>, Raise> {
    let mut text = raw.to_vec();
    let mut inserted: Vec<usize> = Vec::new();
    let mut eof_newline = false;
    let hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let mut outcome = Err(unsupported("input libyaml-safer cannot parse"));
    for _ in 0..64 {
        let last_end = Cell::new(0);
        let shifts: Vec<(u64, u64)> = inserted.iter().map(|at| mark_of(&text, *at)).collect();
        let run = || parse_first(&text, eof_newline, &shifts, &last_end);
        let payload = match panic::catch_unwind(AssertUnwindSafe(run)) {
            Ok(result) => {
                outcome = result;
                break;
            }
            Err(payload) => payload,
        };
        let message = payload
            .downcast_ref::<&str>()
            .map(|m| (*m).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if message == "unexpected end of input" && !eof_newline {
            eof_newline = true;
            continue;
        }
        let Some(at) = flow_tag_comma(&text, last_end.get()) else {
            break;
        };
        for offset in &mut inserted {
            if *offset >= at {
                *offset += 1;
            }
        }
        inserted.push(at);
        text.insert(at, b' ');
    }
    panic::set_hook(hook);
    outcome
}

/// The first `!tag,` at or after byte `from` whose `!` starts a token: the comma's offset.
fn flow_tag_comma(b: &[u8], from: usize) -> Option<usize> {
    let tag_char = |c: u8| c.is_ascii_alphanumeric() || b"-_;/?:@&=+$.%!~*'()".contains(&c);
    (from..b.len()).find_map(|i| {
        let starts = b[i] == b'!' && (i == 0 || b" \t\r\n[{,:".contains(&b[i - 1]));
        if !starts {
            return None;
        }
        let end = i + 1 + b[i + 1..].iter().take_while(|c| tag_char(**c)).count();
        (b.get(end) == Some(&b',')).then_some(end)
    })
}

/// libyaml's (line, column) for a byte offset: columns in characters, lines broken by
/// CR LF, CR, LF, NEL, LS and PS.
fn mark_of(text: &[u8], offset: usize) -> (u64, u64) {
    let mut line = 0;
    let mut column = 0;
    let prefix = String::from_utf8_lossy(&text[..offset.min(text.len())]);
    let mut chars = prefix.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' && chars.peek() == Some(&'\n') {
            continue;
        }
        if matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            line += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn parse_first(
    text: &[u8],
    eof_newline: bool,
    shifts: &[(u64, u64)],
    last_end: &Cell<usize>,
) -> Result<Option<Node>, Raise> {
    let mut padded = text.to_vec();
    if eof_newline {
        padded.push(b'\n');
    }
    let problem = Cell::new(None);
    let mut parser = Parser::new();
    parser.set_encoding(Encoding::Utf8);
    parser.set_input(Gate::new(&padded, &problem));
    let limit = u64::try_from(text.len()).unwrap_or(u64::MAX);
    let problem = &problem;
    let mut next = move || {
        let mut event = parser.parse().map_err(|e| match problem.get() {
            // Psych reports a reader problem at the unset context mark.
            Some(problem) if e.kind() == ErrorKind::Io => Raise::new(
                "Psych::SyntaxError",
                format!("(<unknown>): {problem} at line 1 column 1"),
            ),
            _ => syntax_error(&e, shifts),
        })?;
        last_end.set(usize::try_from(event.end_mark.index).unwrap_or(usize::MAX));
        let reached_padding = eof_newline && event.end_mark.index > limit;
        if let EventData::Scalar {
            value,
            style: ScalarStyle::Literal | ScalarStyle::Folded,
            ..
        } = &mut event.data
            && reached_padding
            && value.ends_with('\n')
        {
            value.pop();
        }
        Ok(event.data)
    };
    loop {
        match next()? {
            EventData::StreamStart { .. } => {}
            EventData::DocumentStart { .. } => break,
            _ => return Ok(None),
        }
    }
    let first = next()?;
    let root = build(first, &mut next, 0)?;
    next()?;
    Ok(Some(root))
}

fn too_deep() -> Raise {
    Raise::new("SystemStackError", "stack level too deep")
}

/// One node from its first event, pulling the rest of a collection's events.
fn build(
    event: EventData,
    next: &mut impl FnMut() -> Result<EventData, Raise>,
    depth: usize,
) -> Result<Node, Raise> {
    if depth > MAX_DEPTH {
        return Err(too_deep());
    }
    match event {
        EventData::Scalar {
            anchor,
            tag,
            value,
            quoted_implicit,
            ..
        } => Ok(Node::Scalar {
            anchor,
            tag,
            value,
            quoted: quoted_implicit,
        }),
        EventData::Alias { anchor } => Ok(Node::Alias(anchor)),
        EventData::SequenceStart { anchor, tag, .. } => {
            let items = children(next, depth, |e| matches!(e, EventData::SequenceEnd))?;
            Ok(Node::Seq { anchor, tag, items })
        }
        EventData::MappingStart { anchor, tag, .. } => {
            let items = children(next, depth, |e| matches!(e, EventData::MappingEnd))?;
            Ok(Node::Map { anchor, tag, items })
        }
        _ => Err(Raise::new("Psych::SyntaxError", "unexpected event")),
    }
}

fn children(
    next: &mut impl FnMut() -> Result<EventData, Raise>,
    depth: usize,
    is_end: impl Fn(&EventData) -> bool,
) -> Result<Vec<Node>, Raise> {
    let mut items = Vec::new();
    loop {
        let event = next()?;
        if is_end(&event) {
            return Ok(items);
        }
        items.push(build(event, next, depth + 1)?);
    }
}

/// A tagged form this port does not model (only reachable through `!ruby/...` tags that
/// name a permitted class or Ruby internals); reported as an uncaught error.
fn unsupported(tag: &str) -> Raise {
    Raise::new(
        "NotImplementedError",
        format!("okf port: unsupported YAML tag {tag}"),
    )
}

/// The restricted class loader's `resolve_class`: nil for an empty name, otherwise
/// `DisallowedClass` (Date and Time, when permitted, are only reachable through forms the
/// port does not model).
fn resolve_class(name: &str, tag: &str, opts: Options) -> Result<(), Raise> {
    match name {
        "" => Ok(()),
        "Date" | "Time" if opts.date_time => Err(unsupported(tag)),
        _ => Err(disallowed(name)),
    }
}

/// `/^PREFIX(?::(.*))?$/` on a tag: `Some(None)` for the bare prefix, `Some(Some(name))` after `:`.
#[allow(clippy::option_option)] // the regex's match, then its optional capture
fn suffix_name<'a>(tag: &'a str, prefix: &str) -> Option<Option<&'a str>> {
    let rest = tag.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some(None);
    }
    rest.strip_prefix(':').map(Some)
}

/// `/^!ruby\/data(-with-ivars)?(?::(.*))?$/`: whether with ivars, and the class name.
fn data_tag(tag: &str) -> Option<(bool, Option<&str>)> {
    suffix_name(tag, "!ruby/data-with-ivars")
        .map(|name| (true, name))
        .or_else(|| suffix_name(tag, "!ruby/data").map(|name| (false, name)))
}

/// `resolve_class("")` is nil, and nil has no `allocate`.
fn no_allocate() -> Raise {
    Raise::new("NoMethodError", "undefined method 'allocate' for nil")
}

/// `/^PREFIX(.*)$/` on a tag: the capture after the prefix.
fn after<'a>(tag: &'a str, prefix: &str) -> Option<&'a str> {
    tag.strip_prefix(prefix)
}

/// `/^!(?:str|ruby\/string)(?::(.*))?$/` or the core str tag: `Some(class name or "")`.
fn str_tag(tag: &str) -> Option<&str> {
    if tag == "tag:yaml.org,2002:str" {
        return Some("");
    }
    let rest = after(tag, "!str").or_else(|| after(tag, "!ruby/string"))?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix(':')
}

/// An anchor's state while visiting.
enum Anchor {
    Open(bool),
    Done(Val),
}

struct ToRuby {
    anchors: HashMap<String, Anchor>,
    opts: Options,
}

impl ToRuby {
    fn register(&mut self, anchor: Option<&String>, state: Anchor) {
        if let Some(name) = anchor {
            self.anchors.insert(name.clone(), state);
        }
    }

    fn done(&mut self, anchor: Option<&String>, v: &Val) {
        self.register(anchor, Anchor::Done(v.clone()));
    }

    fn accept(&mut self, node: &Node, depth: usize) -> Result<Val, Raise> {
        if depth > MAX_DEPTH {
            return Err(too_deep());
        }
        match node {
            Node::Scalar {
                anchor,
                tag,
                value,
                quoted,
            } => {
                let v = deserialize(tag.as_deref(), value, *quoted, self.opts)?;
                self.done(anchor.as_ref(), &v);
                Ok(v)
            }
            Node::Alias(_) if !self.opts.aliases => Err(Raise::new(
                "Psych::AliasesNotEnabled",
                "Alias parsing was not enabled. To enable it, pass `aliases: true` to `Psych::load` or `Psych::safe_load`.",
            )),
            Node::Alias(name) => match self.anchors.get(name) {
                Some(Anchor::Done(v)) => Ok(v.clone()),
                Some(Anchor::Open(hash)) => Ok(Val::Recursive(*hash)),
                None => {
                    let message = format!("An alias referenced an unknown anchor: {name}");
                    Err(Raise::new("Psych::AnchorNotDefined", message))
                }
            },
            Node::Seq { anchor, tag, items } => {
                self.sequence(anchor.as_ref(), tag.as_deref(), items, depth)
            }
            Node::Map { anchor, tag, items } => {
                self.mapping(anchor.as_ref(), tag.as_deref(), items, depth)
            }
        }
    }

    fn sequence(
        &mut self,
        anchor: Option<&String>,
        tag: Option<&str>,
        items: &[Node],
        depth: usize,
    ) -> Result<Val, Raise> {
        let tag_text = tag.unwrap_or("");
        if matches!(tag, Some("!omap" | "tag:yaml.org,2002:omap")) {
            self.register(anchor, Anchor::Open(true));
            let mut pairs = Vec::new();
            for item in items {
                let (Node::Map { items: kv, .. } | Node::Seq { items: kv, .. }) = item else {
                    return Err(Raise::new(
                        "NoMethodError",
                        "undefined method 'first' for nil",
                    ));
                };
                let (Some(first), Some(last)) = (kv.first(), kv.last()) else {
                    return Err(Raise::new(
                        "NoMethodError",
                        "undefined method 'tag' for nil",
                    ));
                };
                let k = self.accept(first, depth + 1)?;
                let v = self.accept(last, depth + 1)?;
                hash_set(&mut pairs, k, v);
            }
            let v = Val::Hash(pairs);
            self.done(anchor, &v);
            return Ok(v);
        }
        let named = after(tag_text, "!seq:").or_else(|| after(tag_text, "!ruby/array:"));
        if let Some(name) = named {
            resolve_class(name, tag_text, self.opts)?;
            return Err(Raise::new(
                "NoMethodError",
                "undefined method 'allocate' for nil",
            ));
        }
        self.register(anchor, Anchor::Open(false));
        let mut list = Vec::new();
        for item in items {
            list.push(self.accept(item, depth + 1)?);
        }
        let v = Val::Array(list);
        self.done(anchor, &v);
        Ok(v)
    }

    fn mapping(
        &mut self,
        anchor: Option<&String>,
        tag: Option<&str>,
        items: &[Node],
        depth: usize,
    ) -> Result<Val, Raise> {
        let Some(tag) = tag else {
            return self.hash(anchor, items, depth);
        };
        if let Some(rest) = after(tag, "!ruby/struct") {
            let name = rest.strip_prefix(':').unwrap_or(rest);
            resolve_class(name, tag, self.opts)?;
            return Err(disallowed("Struct"));
        }
        if let Some(rest) = after(tag, "!ruby/object") {
            let name = rest.strip_prefix(':').unwrap_or(rest);
            return match name {
                "Complex" | "Rational" => Err(disallowed(name)),
                "Hash" => self.hash(anchor, items, depth),
                _ => {
                    resolve_class(name, tag, self.opts)?;
                    Err(disallowed("Object"))
                }
            };
        }
        if let Some(name) = str_tag(tag) {
            return self.string_map(anchor, items, depth, name, tag);
        }
        if let Some(name) = after(tag, "!ruby/array:")
            .or_else(|| after(tag, "!map:"))
            .or_else(|| after(tag, "!ruby/hash:"))
            .or_else(|| after(tag, "!ruby/marshalable:"))
        {
            resolve_class(name, tag, self.opts)?;
            return Err(Raise::new(
                "NoMethodError",
                "undefined method 'allocate' for nil",
            ));
        }
        match tag {
            "!ruby/range" => return Err(disallowed("Range")),
            "!set" | "tag:yaml.org,2002:set" => return Err(disallowed("Psych::Set")),
            "!omap" | "tag:yaml.org,2002:omap" => return Err(disallowed("Psych::Omap")),
            _ => {}
        }
        if let Some(rest) = after(tag, "!ruby/exception") {
            for item in items {
                self.accept(item, depth + 1)?;
            }
            let name = rest.strip_prefix(':').unwrap_or(rest);
            resolve_class(name, tag, self.opts)?;
            return Err(disallowed("Exception"));
        }
        if let Some((ivars, name)) = data_tag(tag) {
            return self.data(items, depth, ivars, name, tag);
        }
        if let Some(name) = suffix_name(tag, "!ruby/hash-with-ivars") {
            if let Some(name) = name {
                resolve_class(name, tag, self.opts)?;
                return Err(no_allocate());
            }
            return self.hash_with_ivars(anchor, items, depth, tag);
        }
        self.hash(anchor, items, depth)
    }

    /// `!ruby/data(-with-ivars)?(:Name)?`: a named class is resolved and refused; an anonymous
    /// Data symbolizes its first member's name (refused) or, with none, loads `Data` (refused).
    fn data(
        &mut self,
        items: &[Node],
        depth: usize,
        ivars: bool,
        name: Option<&str>,
        tag: &str,
    ) -> Result<Val, Raise> {
        if let Some(name) = name {
            resolve_class(name, tag, self.opts)?;
            return Err(no_allocate());
        }
        if ivars {
            return Err(unsupported(tag));
        }
        if let Some(pair) = items.chunks(2).next() {
            for node in pair {
                self.accept(node, depth + 1)?;
            }
            return Err(disallowed("Symbol"));
        }
        Err(disallowed("Data"))
    }

    /// An anonymous `!ruby/hash-with-ivars`: the Hash its `elements` mapping revives. Setting
    /// instance variables (a non-empty `ivars`) and malformed shapes are not modelled.
    fn hash_with_ivars(
        &mut self,
        anchor: Option<&String>,
        items: &[Node],
        depth: usize,
        tag: &str,
    ) -> Result<Val, Raise> {
        self.register(anchor, Anchor::Open(true));
        let mut pairs = Vec::new();
        for pair in items.chunks(2) {
            let (Node::Scalar { value: key, .. }, Some(value)) = (&pair[0], pair.get(1)) else {
                return Err(unsupported(tag));
            };
            match (key.as_str(), value) {
                ("elements", Node::Map { items, .. }) => self.revive(&mut pairs, items, depth)?,
                ("ivars", Node::Map { items, .. }) if items.is_empty() => {}
                ("elements" | "ivars", _) => return Err(unsupported(tag)),
                _ => {}
            }
        }
        let v = Val::Hash(pairs);
        self.done(anchor, &v);
        Ok(v)
    }

    /// A `!str` mapping: the `str` key's value, or nil; other keys become ivars.
    fn string_map(
        &mut self,
        anchor: Option<&String>,
        items: &[Node],
        depth: usize,
        name: &str,
        tag: &str,
    ) -> Result<Val, Raise> {
        resolve_class(name, tag, self.opts)?;
        let mut string = Val::Nil;
        let mut members = false;
        for pair in items.chunks(2) {
            let key = self.accept(&pair[0], depth + 1)?;
            let value = match pair.get(1) {
                Some(v) => self.accept(v, depth + 1)?,
                None => Val::Nil,
            };
            if key.string_bytes() == Some(b"str") {
                string = value;
                self.done(anchor, &string);
            } else {
                members = true;
            }
        }
        if members && !string.truthy() {
            return Err(Raise::new(
                "FrozenError",
                "can't modify frozen NilClass: nil",
            ));
        }
        Ok(string)
    }

    /// `revive_hash`, merge keys included.
    fn hash(
        &mut self,
        anchor: Option<&String>,
        items: &[Node],
        depth: usize,
    ) -> Result<Val, Raise> {
        self.register(anchor, Anchor::Open(true));
        let mut pairs: Vec<(Val, Val)> = Vec::new();
        self.revive(&mut pairs, items, depth)?;
        let v = Val::Hash(pairs);
        self.done(anchor, &v);
        Ok(v)
    }

    /// `revive_hash`'s loop: each pair into `pairs`, `<<` merging.
    fn revive(
        &mut self,
        pairs: &mut Vec<(Val, Val)>,
        items: &[Node],
        depth: usize,
    ) -> Result<(), Raise> {
        for pair in items.chunks(2) {
            let (k_node, v_node) = (&pair[0], pair.get(1));
            let key = self.accept(k_node, depth + 1)?;
            let val = match v_node {
                Some(v) => self.accept(v, depth + 1)?,
                None => Val::Nil,
            };
            let merge =
                key.string_bytes() == Some(b"<<") && k_node.tag() != Some("tag:yaml.org,2002:str");
            if !merge {
                hash_set(pairs, key, val);
                continue;
            }
            merge_into(pairs, key, val, v_node);
        }
        Ok(())
    }
}

/// `hash.merge!(val)` for `<<`, falling back to an ordinary `<<` key on `TypeError`.
fn merge_into(pairs: &mut Vec<(Val, Val)>, key: Val, val: Val, v_node: Option<&Node>) {
    match (v_node, &val) {
        (Some(Node::Alias(_) | Node::Map { .. }), Val::Hash(other)) => {
            for (k, v) in other.clone() {
                hash_set(pairs, k, v);
            }
        }
        (Some(Node::Alias(_) | Node::Map { .. }), Val::Recursive(true)) => {}
        (Some(Node::Seq { .. }), Val::Array(list)) if list.iter().all(Val::is_hash) => {
            let mut merged = Vec::new();
            for item in list.iter().rev() {
                if let Val::Hash(other) = item {
                    for (k, v) in other.clone() {
                        hash_set(&mut merged, k, v);
                    }
                }
            }
            for (k, v) in merged {
                hash_set(pairs, k, v);
            }
        }
        _ => hash_set(pairs, key, val),
    }
}

/// Lenient base64 (`unpack('m')`): skip non-alphabet bytes, stop at padding.
fn unbase64(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xFF).unwrap_or(0));
        }
    }
    out
}

/// `Float(x)` on what `tokenize` returned (the `!!float` tag).
fn float_of(v: Val) -> Result<Val, Raise> {
    let cant = |what: &str| Raise::new("TypeError", format!("can't convert {what} into Float"));
    match v {
        Val::Float(_) => Ok(v),
        Val::Int(i) => Ok(Val::Float(i.parse().unwrap_or(0.0))),
        Val::Str(s) => ruby_float(&s).map(Val::Float),
        Val::Time(t) => Ok(Val::Float(t.to_f())),
        Val::Nil => Err(cant("nil")),
        Val::Bool(b) => Err(cant(&b.to_string())),
        _ => Err(cant("Date")),
    }
}

/// `ToRuby#deserialize` for a scalar.
fn deserialize(tag: Option<&str>, value: &str, quoted: bool, opts: Options) -> Result<Val, Raise> {
    if quoted {
        return Ok(Val::Str(value.to_string()));
    }
    let Some(tag) = tag else {
        return tokenize(value, opts.date_time);
    };
    if matches!(tag, "!binary" | "tag:yaml.org,2002:binary") {
        return Ok(Val::Bin(unbase64(value)));
    }
    if let Some(name) = str_tag(tag) {
        resolve_class(name, tag, opts)?;
        return Ok(Val::Str(value.to_string()));
    }
    match tag {
        "!ruby/object:BigDecimal" => Err(disallowed("BigDecimal")),
        "!ruby/object:DateTime" => Err(disallowed("DateTime")),
        "!ruby/object:Complex" => Err(disallowed("Complex")),
        "!ruby/object:Rational" => Err(disallowed("Rational")),
        "!ruby/encoding" => Err(unsupported(tag)),
        "!ruby/class" | "!ruby/module" => {
            resolve_class(value, tag, opts)?;
            Ok(Val::Nil)
        }
        "tag:yaml.org,2002:float" | "!float" => float_of(tokenize(value, opts.date_time)?),
        "!ruby/regexp" => Err(disallowed("Regexp")),
        "!ruby/range" => Err(disallowed("Range")),
        _ if tag.starts_with("!ruby/sym") => Err(disallowed("Symbol")),
        _ => tokenize(value, opts.date_time),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::okf::rb::inspect;

    fn load(s: &str) -> String {
        match safe_load(s) {
            Ok(Some(v)) => inspect(&v),
            Ok(None) => "nil".to_string(),
            Err(e) => format!("{}: {}", e.class, e.message),
        }
    }
    #[test]
fn documents_load_as_psych_loads_them() {
        assert_eq!(load(""), "nil");
        assert_eq!(load("a: &x 1\nb: *x"), "{\"a\" => 1, \"b\" => 1}");
        assert_eq!(load("? [a]\n: 1"), "{[\"a\"] => 1}");
        assert_eq!(load("a: 1\n...\n@"), "{\"a\" => 1}");
        assert_eq!(
            load("b: &m {x: 1}\nc:\n  <<: *m\n  y: 2"),
            "{\"b\" => {\"x\" => 1}, \"c\" => {\"x\" => 1, \"y\" => 2}}"
        );
        assert_eq!(
            load("a: !!str 0.2\nb: ! \"12\""),
            "{\"a\" => \"0.2\", \"b\" => 12}"
        );
    }
    #[test]
fn libyaml_safer_panics_are_worked_around() {
        assert_eq!(load("a: |\n  x"), "{\"a\" => \"x\"}");
        assert_eq!(load("a: >\n  x\n  y"), "{\"a\" => \"x y\"}");
        assert_eq!(load("a: |+\n  x"), "{\"a\" => \"x\"}");
        assert_eq!(load("a: |\n  x\n  "), "{\"a\" => \"x\\n\"}");
        assert_eq!(
            load("n: |\n  k\nt: >\n  - {d: e}"),
            "{\"n\" => \"k\\n\", \"t\" => \"- {d: e}\"}"
        );
        assert_eq!(
            load("g: {by: !+_7TN, at: ''}"),
            "{\"g\" => {\"by\" => nil, \"at\" => \"\"}}"
        );
        assert_eq!(
            load("g: {by: !x, at:, 'z'}"),
            "Psych::SyntaxError: (<unknown>): found unexpected ':' while scanning a plain scalar at line 1 column 13"
        );
    }
    #[test]
fn errors_read_as_psych_writes_them() {
        assert_eq!(
            load("a: b: c"),
            "Psych::SyntaxError: (<unknown>): mapping values are not allowed in this context at line 1 column 5"
        );
        assert_eq!(
            load("a: ["),
            "Psych::SyntaxError: (<unknown>): did not find expected node content while parsing a flow node at line 2 column 1"
        );
        assert_eq!(
            load("a: \"x"),
            "Psych::SyntaxError: (<unknown>): found unexpected end of stream while scanning a quoted scalar at line 1 column 4"
        );
        assert_eq!(
            load("%TAG ! a\n%TAG ! b\n---\nx"),
            "Psych::SyntaxError: (<unknown>): found duplicate %TAG directive at line 1 column 1"
        );
        assert_eq!(
            load("a: \u{1}"),
            "Psych::SyntaxError: (<unknown>): control characters are not allowed at line 1 column 1"
        );
        assert_eq!(
            load("a: *x"),
            "Psych::AnchorNotDefined: An alias referenced an unknown anchor: x"
        );
        assert_eq!(
            load("a: !ruby/object:Foo {}"),
            "Psych::DisallowedClass: Tried to load unspecified class: Foo"
        );
        assert_eq!(
            load("!ruby/data {a: 1}"),
            "Psych::DisallowedClass: Tried to load unspecified class: Symbol"
        );
        assert_eq!(
            load("!ruby/data {}"),
            "Psych::DisallowedClass: Tried to load unspecified class: Data"
        );
        assert_eq!(
            load("!ruby/data: {}"),
            "NoMethodError: undefined method 'allocate' for nil"
        );
        assert_eq!(
            load("!ruby/hash-with-ivars {elements: {a: 1}, ivars: {}}"),
            "{\"a\" => 1}"
        );
    }
        #[test]
    fn additional_mapping_error_messages_match_psych() {
        assert!(load("*\n").contains("SyntaxError"));
        assert!(load("a: b: c").contains("mapping values"));
        assert!(load("{\n").contains("SyntaxError"));
    }
    #[test]
fn merge_keys_copy_anchors_like_psych() {
        assert_eq!(
            load("defaults: &def\n  x: 1\nitem:\n  <<: *def\n  y: 2"),
            "{\"defaults\" => {\"x\" => 1}, \"item\" => {\"x\" => 1, \"y\" => 2}}"
        );
    }

    #[test]
    fn multi_document_stream_returns_first_mapping_only() {
        assert_eq!(load("---\na: 1\n---\nb: 2\n"), "{\"a\" => 1}");
    }
    #[test]
fn flow_mapping_scalar_tag_comma_workaround_parses() {
        assert_eq!(
            load("g: {by: !tag, at: 1}"),
            "{\"g\" => {\"by\" => nil, \"at\" => 1}}"
        );
    }
    
    #[test]
    fn bare_load_accepts_flow_mapping_and_sequence() {
        use crate::okf::rb::inspect;
        let Some(v) = super::load(b"{a: 1, b: [2, 3]}", Options::BARE).unwrap_or_else(|e| panic!("{}", e.message)) else {
            panic!("expected bare flow mapping");
        };
        assert!(inspect(&v).contains("\"a\" => 1"));
        assert!(inspect(&v).contains("[2, 3]"));
    }

    #[test]
    fn bare_load_preserves_explicit_string_and_float_tags() {
        use crate::okf::rb::inspect;
        let Some(doc) = super::load(b"v: !!float 1.5\ns: !!str yes\n", Options::BARE).unwrap_or_else(|e| panic!("{}", e.message)) else {
            panic!("expected bare mapping");
        };
        let text = inspect(&doc);
        assert!(text.contains("1.5"));
        assert!(text.contains("\"yes\""));
    }

    #[test]
    fn bare_load_parses_documents_without_safe_load_limits() {
        let bytes = b"a: [1, 2.5]
";
        let parsed =
            super::load(bytes, Options::BARE).unwrap_or_else(|e| panic!("load: {}", e.message));
        assert!(parsed.is_some());
    }

    #[test]
    fn disallowed_object_tags_report_psych_class_errors() {
        for yaml in [
            "x: !ruby/object:BigDecimal 1",
            "x: !ruby/object:DateTime 1",
            "x: !ruby/object:Complex 1",
            "x: !ruby/object:Rational 1",
            "x: !ruby/regexp /x/",
            "x: !ruby/range 1..2",
            "x: !ruby/sym x",
        ] {
            let out = load(yaml);
            assert!(
                out.contains("DisallowedClass"),
                "wanted DisallowedClass for {yaml:?}, got {out}"
            );
        }
    }

    #[test]
    fn tagged_scalars_cover_binary_float_and_ruby_class_forms() {
        assert_eq!(load("payload: !!binary YQ=="), "{\"payload\" => \"a\"}");
        assert_eq!(load("n: !!float 2.5"), "{\"n\" => 2.5}");

        assert!(load("enc: !ruby/encoding utf-8").contains("NotImplementedError"));
    }


    #[test]
    fn omap_struct_and_hash_object_tags_follow_psych() {
        assert_eq!(
            load("seq: !omap\n  - a: 1\n  - b: 2"),
            "{\"seq\" => {\"a\" => 1, \"b\" => 2}}"
        );
        assert!(load("x: !ruby/struct:Foo\n  a: 1").contains("DisallowedClass"));
        assert_eq!(
            load("h: !ruby/object:Hash\n  a: 1"),
            "{\"h\" => {\"a\" => 1}}"
        );
        assert!(load("s: !set {}").contains("Psych::Set"));
        assert!(load("r: !ruby/range\n  =: 1\n  :!: 2").contains("DisallowedClass"));
    }



    #[test]
    fn recursive_alias_yields_psych_recursive_placeholder() {
        assert_eq!(load("a: &x [1, *x]"), "{\"a\" => [1, [...]]}");
    }

    #[test]
    fn ruby_class_tag_is_disallowed_under_safe_load() {
        assert!(load("v: !ruby/class String").contains("DisallowedClass"));
    }


    #[test]
    fn disallowed_openstruct_reports_class_name() {
        assert!(load("x: !ruby/object:OpenStruct {}").contains("OpenStruct"));
    }

    #[test]
    fn float_coercion_refuses_nil_and_boolean_scalars() {
        assert!(load("n: !!float ~").contains("TypeError"));
        assert!(load("n: !!float true").contains("TypeError"));
    }


}
