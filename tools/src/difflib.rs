//! A port of the parts of `CPython`'s `difflib` that `fs_edit` uses: `SequenceMatcher` with the
//! default `autojunk=True` and no junk predicate, `get_grouped_opcodes`, and `unified_diff` with
//! empty file dates. Also `str.splitlines(keepends=True)`, which feeds it.

use std::collections::HashMap;
use std::fmt::Write as _;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tag {
    Replace,
    Delete,
    Insert,
    Equal,
}

/// `(tag, i1, i2, j1, j2)`.
type Opcode = (Tag, usize, usize, usize, usize);

/// `(i, j, size)`.
type Block = (usize, usize, usize);

struct SequenceMatcher<'a> {
    a: &'a [&'a str],
    b: &'a [&'a str],
    b2j: HashMap<&'a str, Vec<usize>>,
}

impl<'a> SequenceMatcher<'a> {
    /// `__init__` + `__chain_b`: index `b`, then drop "popular" lines (autojunk).
    fn new(a: &'a [&'a str], b: &'a [&'a str]) -> Self {
        let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, line) in b.iter().enumerate() {
            b2j.entry(line).or_default().push(i);
        }
        let n = b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            b2j.retain(|_, indices| indices.len() <= ntest);
        }
        Self { a, b, b2j }
    }

    fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> Block {
        let (a, b) = (self.a, self.b);
        let (mut best_a, mut best_b, mut bestsize) = (alo, blo, 0);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, line) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(indices) = self.b2j.get(line) {
                for &j in indices {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let prev = if j == 0 {
                        0
                    } else {
                        j2len.get(&(j - 1)).copied().unwrap_or(0)
                    };
                    let k = prev + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        best_a = i + 1 - k;
                        best_b = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        // No junk predicate, so bjunk is empty: only the non-junk extensions can fire, and they
        // extend across popular lines, exactly as CPython does.
        while best_a > alo && best_b > blo && a[best_a - 1] == b[best_b - 1] {
            best_a -= 1;
            best_b -= 1;
            bestsize += 1;
        }
        while best_a + bestsize < ahi
            && best_b + bestsize < bhi
            && a[best_a + bestsize] == b[best_b + bestsize]
        {
            bestsize += 1;
        }
        (best_a, best_b, bestsize)
    }

    fn get_matching_blocks(&self) -> Vec<Block> {
        let (la, lb) = (self.a.len(), self.b.len());
        let mut queue = vec![(0, la, 0, lb)];
        let mut blocks = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                blocks.push((i, j, k));
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        blocks.sort_unstable();
        let (mut i1, mut j1, mut k1) = (0, 0, 0);
        let mut merged = Vec::new();
        for (i2, j2, k2) in blocks {
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    merged.push((i1, j1, k1));
                }
                (i1, j1, k1) = (i2, j2, k2);
            }
        }
        if k1 > 0 {
            merged.push((i1, j1, k1));
        }
        merged.push((la, lb, 0));
        merged
    }

    fn get_opcodes(&self) -> Vec<Opcode> {
        let (mut i, mut j) = (0, 0);
        let mut answer = Vec::new();
        for (ai, bj, size) in self.get_matching_blocks() {
            let tag = if i < ai && j < bj {
                Some(Tag::Replace)
            } else if i < ai {
                Some(Tag::Delete)
            } else if j < bj {
                Some(Tag::Insert)
            } else {
                None
            };
            if let Some(tag) = tag {
                answer.push((tag, i, ai, j, bj));
            }
            i = ai + size;
            j = bj + size;
            if size > 0 {
                answer.push((Tag::Equal, ai, i, bj, j));
            }
        }
        answer
    }

    fn get_grouped_opcodes(&self, n: usize) -> Vec<Vec<Opcode>> {
        let mut codes = self.get_opcodes();
        if codes.is_empty() {
            codes.push((Tag::Equal, 0, 1, 0, 1));
        }
        if let Some(first) = codes.first_mut()
            && first.0 == Tag::Equal
        {
            let (tag, i1, i2, j1, j2) = *first;
            *first = (
                tag,
                i1.max(i2.saturating_sub(n)),
                i2,
                j1.max(j2.saturating_sub(n)),
                j2,
            );
        }
        if let Some(last) = codes.last_mut()
            && last.0 == Tag::Equal
        {
            let (tag, i1, i2, j1, j2) = *last;
            *last = (tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n));
        }
        let nn = n + n;
        let mut groups = Vec::new();
        let mut group = Vec::new();
        for (tag, mut i1, i2, mut j1, j2) in codes {
            if tag == Tag::Equal && i2 - i1 > nn {
                group.push((tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n)));
                groups.push(std::mem::take(&mut group));
                i1 = i1.max(i2.saturating_sub(n));
                j1 = j1.max(j2.saturating_sub(n));
            }
            group.push((tag, i1, i2, j1, j2));
        }
        let only_equal = group.len() == 1 && group[0].0 == Tag::Equal;
        if !group.is_empty() && !only_equal {
            groups.push(group);
        }
        groups
    }
}

/// `_format_range_unified`.
fn format_range(start: usize, stop: usize) -> String {
    let mut beginning = start + 1;
    let length = stop - start;
    if length == 1 {
        return beginning.to_string();
    }
    if length == 0 {
        beginning -= 1;
    }
    format!("{beginning},{length}")
}

/// `"".join(difflib.unified_diff(a, b, fromfile, tofile, n=n))` with `lineterm="\n"`.
pub fn unified_diff(a: &[&str], b: &[&str], fromfile: &str, tofile: &str, n: usize) -> String {
    let matcher = SequenceMatcher::new(a, b);
    let mut out = String::new();
    for (index, group) in matcher.get_grouped_opcodes(n).iter().enumerate() {
        if index == 0 {
            let _ = write!(out, "--- {fromfile}\n+++ {tofile}\n");
        }
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let file1 = format_range(first.1, last.2);
        let file2 = format_range(first.3, last.4);
        let _ = writeln!(out, "@@ -{file1} +{file2} @@");
        for &(tag, i1, i2, j1, j2) in group {
            if tag == Tag::Equal {
                for line in &a[i1..i2] {
                    out.push(' ');
                    out.push_str(line);
                }
                continue;
            }
            if matches!(tag, Tag::Replace | Tag::Delete) {
                for line in &a[i1..i2] {
                    out.push('-');
                    out.push_str(line);
                }
            }
            if matches!(tag, Tag::Replace | Tag::Insert) {
                for line in &b[j1..j2] {
                    out.push('+');
                    out.push_str(line);
                }
            }
        }
    }
    out
}

/// `s.splitlines(keepends=True)`: Python's line boundaries, `\r\n` counted as one.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = match c {
            '\r' => {
                if chars.peek().is_some_and(|&(_, next)| next == '\n') {
                    chars.next();
                    i + 2
                } else {
                    i + 1
                }
            }
            '\n' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => i + c.len_utf8(),
            _ => continue,
        };
        lines.push(&s[start..end]);
        start = end;
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(a: &str, b: &str) -> String {
        unified_diff(&splitlines(a), &splitlines(b), "f", "f", 3)
    }
    #[test]
fn fixture_replace_diff() {
        assert_eq!(
            diff("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n"),
            "--- f\n+++ f\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"
        );
    }
    #[test]
fn distant_changes_split_into_hunks() {
        let a: String = (0..20)
            .map(|i| "l".to_owned() + &i.to_string() + "\n")
            .collect();
        let b = a.replace("l2\n", "X\n").replace("l17\n", "Y\n");
        assert_eq!(
            diff(&a, &b),
            "--- f\n+++ f\n@@ -1,6 +1,6 @@\n l0\n l1\n-l2\n+X\n l3\n l4\n l5\n\
             @@ -15,6 +15,6 @@\n l14\n l15\n l16\n-l17\n+Y\n l18\n l19\n"
        );
    }
    #[test]
fn missing_final_newline_and_empty_ranges() {
        assert_eq!(diff("a", "a\nb"), "--- f\n+++ f\n@@ -1 +1,2 @@\n-a+a\n+b");
        assert_eq!(diff("", "x\n"), "--- f\n+++ f\n@@ -0,0 +1 @@\n+x\n");
        assert_eq!(diff("solo\n", ""), "--- f\n+++ f\n@@ -1 +0,0 @@\n-solo\n");
    }
        #[test]
    fn autojunk_ignores_overfrequent_lines_when_matching() {
        let common = "repeat\n";
        let a: String = common.repeat(250);
        let mut b = a.clone();
        b = b.replacen("repeat", "changed", 1);
        let a_lines = splitlines(&a);
        let b_lines = splitlines(&b);
        let out = unified_diff(&a_lines, &b_lines, "a", "b", 3);
        assert!(out.contains("changed"));
        assert!(out.contains("-repeat"));
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(
            splitlines("a\r\nb\rc\u{b}d\u{2028}e"),
            vec!["a\r\n", "b\r", "c\u{b}", "d\u{2028}", "e"]
        );
        assert_eq!(splitlines(""), Vec::<&str>::new());
    }
}
