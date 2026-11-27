//! Lint runtime SQL, including SQL fragments used to assemble queries.

use super::{Store, sql};
use proc_macro2::{LineColumn, TokenStream, TokenTree};
use std::collections::HashSet;
use std::path::Path;
use syn::visit::Visit;

#[test]
fn scoped_sql_uses_explicit_identity() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let store = Store::open(&temp.path().join("data"))?;
    let db = store.db();
    let tables = db
        .raw()
        .prepare(
            "SELECT m.name FROM sqlite_schema m, pragma_table_info(m.name) c
         WHERE m.type = 'table' AND c.name = 'org_id'",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    assert!(tables.contains("jobs") && tables.contains("org_settings"));
    assert!(tables.contains("land_requests"));

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for path in sources(&src)? {
        let relative = path.strip_prefix(&src)?;
        if exempt(relative) {
            continue;
        }
        let source = std::fs::read_to_string(&path)?;
        for (position, literal) in rust_strings(&source)? {
            if relative == Path::new("store.rs") && legacy_migration_sql(&literal) {
                continue;
            }
            if let Some(reason) = unscoped(&literal, &tables) {
                violations.push(format!(
                    "{}:{}: {reason}\n  {literal}",
                    relative.display(),
                    position.line
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Scoped SQL violations:\n{}",
        violations.join("\n")
    );
    Ok(())
}

fn sources(dir: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            paths.extend(sources(&path)?);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn legacy_migration_sql(sql: &str) -> bool {
    sql.starts_with("SELECT c.conversation_id, cf.sha256 FROM conversations c")
        || sql == "UPDATE conversations SET lead = 1 WHERE conversation_id = ?1"
        || sql.starts_with("CREATE TABLE lands_next (")
}

// The migration uses pre-scope SQL. The remaining files are test-only modules.
fn exempt(relative: &Path) -> bool {
    [
        "store/teams.rs",
        "store/land_requests.rs",
        "store/teams_tests.rs",
        "store/scope_guard.rs",
        "store/plan_regressions.rs",
        "review_regressions.rs",
    ]
    .iter()
    .any(|path| relative == Path::new(path))
}

#[derive(Default)]
struct TestModules(Vec<(LineColumn, LineColumn)>);

impl<'ast> Visit<'ast> for TestModules {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let test = module.attrs.iter().any(|attr| {
            attr.path().is_ident("cfg")
                && attr
                    .parse_args::<syn::Ident>()
                    .is_ok_and(|arg| arg == "test")
        });
        if test {
            if let Some((brace, _)) = &module.content {
                self.0
                    .push((brace.span.open().start(), brace.span.close().end()));
            }
        } else {
            syn::visit::visit_item_mod(self, module);
        }
    }
}

/// Read cooked and raw Rust strings, including literals inside macro arguments.
pub(super) fn rust_strings(
    source: &str,
) -> Result<Vec<(LineColumn, String)>, Box<dyn std::error::Error>> {
    fn collect(stream: TokenStream, tests: &TestModules, strings: &mut Vec<(LineColumn, String)>) {
        for token in stream {
            let start = token.span().start();
            if tests
                .0
                .iter()
                .any(|&(open, close)| open <= start && start < close)
            {
                continue;
            }
            match token {
                TokenTree::Group(group) => collect(group.stream(), tests, strings),
                TokenTree::Literal(literal) => {
                    if let Ok(string) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                        strings.push((start, string.value()));
                    }
                }
                _ => {}
            }
        }
    }

    let mut tests = TestModules::default();
    tests.visit_file(&syn::parse_file(source)?);
    let mut strings = Vec::new();
    collect(source.parse()?, &tests, &mut strings);
    Ok(strings)
}

struct Tokens {
    words: Vec<String>,
    depth: Vec<usize>,
}

impl Tokens {
    fn new(sql: &str) -> Self {
        let mut level = 0_usize;
        let mut words = Vec::new();
        let mut depth = Vec::new();
        for (_, word) in sql::tokens(sql) {
            if word == ")" {
                level = level.saturating_sub(1);
            }
            words.push(if word.starts_with(':') {
                word.to_owned()
            } else {
                word.to_ascii_lowercase()
            });
            depth.push(level);
            if word == "(" {
                level += 1;
            }
        }
        Self { words, depth }
    }

    fn at(&self, index: usize) -> &str {
        self.words.get(index).map_or("", String::as_str)
    }

    fn name(&self, index: usize) -> (&str, usize) {
        let index = if self.at(index + 1) == "." {
            index + 2
        } else {
            index
        };
        (
            self.at(index).trim_matches(['"', '\x60', '[', ']']),
            index + 1,
        )
    }

    fn alias(&self, at: usize, table: &'_ str) -> usize {
        if self.at(at) == "as" {
            at + 2
        } else if self.at(at + 1) == "where"
            && self
                .at(at)
                .starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '_')
            && self.at(at) != table
        {
            at + 1
        } else {
            at
        }
    }

    fn same_level(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        let depth = self.depth[index];
        (index + 1..self.words.len())
            .take_while(move |&i| self.depth[i] >= depth && self.at(i) != ";")
            .filter(move |&i| self.depth[i] == depth)
    }

    fn scope_filter(&self, at: usize, table: &str) -> bool {
        if self.at(at) != "where" {
            return false;
        }
        let mut column = at + 1;
        if self.at(column) == "+" {
            column += 1;
        }
        if self.at(column) == table && self.at(column + 1) == "." {
            column += 2;
        }
        self.at(column) == "org_id"
            && self.at(column + 1) == "="
            && self.at(column + 2) == ":org_id"
            && (boundary(self.at(column + 3)) || self.at(column + 3) == "and")
            && self
                .same_level(at)
                .take_while(|&i| {
                    (self.at(i) == ")" || !boundary(self.at(i)))
                        && !(self.at(i) == "on" && self.at(i + 1) == "conflict")
                })
                .all(|i| self.at(i) != "or")
    }

    fn insert_has_scope(&self, open: usize, table: &str) -> bool {
        // The first explicit target column and the first value in every row must correspond.
        if self.at(open) != "(" || self.at(open + 1) != "org_id" {
            return false;
        }
        let Some(close) = self.same_level(open).find(|&i| self.at(i) == ")") else {
            return false;
        };
        let mut at = close + 1;
        match self.at(at) {
            "select" if self.at(at + 1) == ":org_id" && self.at(at + 2) == "," => {
                if self
                    .same_level(at)
                    .any(|i| matches!(self.at(i), "union" | "intersect" | "except"))
                {
                    return false;
                }
            }
            "values" => loop {
                if self.at(at + 1) != "(" || self.at(at + 2) != ":org_id" || self.at(at + 3) != ","
                {
                    return false;
                }
                let Some(end) = self.same_level(at + 1).find(|&i| self.at(i) == ")") else {
                    return false;
                };
                if self.at(end + 1) != "," {
                    at = end;
                    break;
                }
                at = end + 1;
            },
            _ => return false,
        }
        self.same_level(at)
            .filter(|&i| self.at(i) == "do" && self.at(i + 1) == "update")
            .all(|i| {
                self.same_level(i)
                    .take_while(|&j| {
                        !(self.at(j) == "on" && self.at(j + 1) == "conflict")
                            && self.at(j) != "returning"
                    })
                    .find(|&j| self.at(j) == "where")
                    .is_some_and(|j| self.scope_filter(j, table))
            })
    }
}

fn boundary(word: &str) -> bool {
    matches!(
        word,
        "" | ")"
            | ";"
            | "group"
            | "having"
            | "window"
            | "order"
            | "limit"
            | "union"
            | "intersect"
            | "except"
            | "returning"
    )
}

fn unscoped(sql: &str, tables: &HashSet<String>) -> Option<String> {
    let tokens = Tokens::new(sql);
    let scoped = |table: &str| tables.contains(table) || table.starts_with('{');
    for (i, word) in tokens.words.iter().enumerate() {
        let word = word.as_str();
        if matches!(word, "default_org_id" | "bootstrap_member_id") && tokens.at(i + 1) == "(" {
            return Some(format!("implicit SQL identity function {word}"));
        }
        let target = i
            + 1
            + if word == "update" && tokens.at(i + 1) == "or" {
                2
            } else {
                0
            };
        let (table, after) = tokens.name(target);
        match word {
            "into" if scoped(table) => {
                if i > 0 && tokens.at(i - 1) == "replace" {
                    return Some(format!("REPLACE into {table} can overwrite another org"));
                }
                if !tokens.insert_has_scope(after, table) {
                    return Some(format!("INSERT into {table} needs org_id bound to :org_id"));
                }
            }
            "update" if scoped(table) => {
                if tokens.at(i + 1) == "or" && tokens.at(i + 2) == "replace" {
                    return Some(format!(
                        "UPDATE OR REPLACE of {table} can overwrite another org"
                    ));
                }
                if !tokens
                    .same_level(i)
                    .find(|&j| tokens.at(j) == "where")
                    .is_some_and(|at| tokens.scope_filter(at, table))
                {
                    return Some(format!("UPDATE of {table} needs an org_id filter"));
                }
            }
            "from" | "join" => {
                if scoped(table) && !tokens.scope_filter(tokens.alias(after, table), table) {
                    return Some(format!("{word} {table} needs an org_id filter"));
                }
                if (i == 0
                    || tokens.words[..i]
                        .iter()
                        .any(|word| matches!(word.as_str(), "select" | "delete" | "update")))
                    && tokens
                        .same_level(i)
                        .take_while(|&j| {
                            tokens.at(j) == ")"
                                || (!boundary(tokens.at(j)) && tokens.at(j) != "where")
                        })
                        .any(|j| tokens.at(j) == "," && scoped(tokens.name(j + 1).0))
                {
                    return Some("comma join needs a scoped subquery".to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

#[test]
fn guard_rejects_missing_or_misplaced_scope() {
    let tables = ["jobs".to_owned(), "nodes".to_owned()]
        .into_iter()
        .collect();
    for sql in [
        "SELECT job_id FROM jobs",
        "SELECT job_id FROM (SELECT job_id FROM jobs) WHERE org_id = :org_id",
        "SELECT job_id FROM jobs WHERE org_id = :org_id OR job_id = ?1",
        "SELECT job_id FROM jobs WHERE org_id = :org_id AND (job_id = ?1) OR job_id = ?2",
        "SELECT job_id FROM (SELECT * FROM jobs WHERE org_id = :org_id) j JOIN nodes n ON j.node_id = n.id",
        "SELECT job_id FROM (SELECT * FROM jobs WHERE org_id = :org_id) j, nodes n",
        "UPDATE jobs SET state = ?1 WHERE job_id = ?2",
        "UPDATE OR IGNORE jobs AS j SET state = ?1 WHERE job_id = ?2",
        "DELETE FROM main.jobs WHERE job_id = ?1",
        "INSERT INTO jobs (job_id) VALUES (:org_id)",
        "INSERT INTO jobs (org_id, job_id) VALUES (:org_id, ?1), ('foreign', ?2)",
        "INSERT INTO jobs (org_id, job_id) VALUES (:org_id, ?1) ON CONFLICT(job_id) DO UPDATE SET state = ?2",
        "INSERT INTO jobs (org_id, job_id) SELECT :org_id, ?1 ON CONFLICT(job_id) DO UPDATE SET state = ?2",
        "INSERT INTO jobs (org_id, job_id) SELECT :org_id, ?1 UNION ALL SELECT 'foreign', ?2",
        "INSERT INTO jobs (org_id, job_id) SELECT :org_id, ?1 ON CONFLICT(job_id) DO UPDATE SET state = ?2 ON CONFLICT DO UPDATE SET state = ?2 WHERE +org_id = :org_id",
        "REPLACE INTO jobs (org_id, job_id) VALUES (:org_id, ?1)",
        "INSERT OR REPLACE INTO jobs (org_id, job_id) SELECT :org_id, ?1",
        "UPDATE OR REPLACE jobs SET job_id = ?1 WHERE +org_id = :org_id AND job_id = ?2",
    ] {
        assert!(unscoped(sql, &tables).is_some(), "{sql}");
    }
    for sql in [
        "SELECT job_id FROM jobs WHERE org_id = :org_id AND (job_id = ?1 OR job_id = ?2)",
        "UPDATE jobs SET state = ?1 WHERE org_id = :org_id AND job_id = ?2",
        "DELETE FROM jobs WHERE org_id = :org_id AND job_id = ?1",
        "INSERT INTO jobs (org_id, job_id) VALUES (:org_id, ?1), (:org_id, ?2)",
        "INSERT INTO jobs (org_id, job_id) VALUES (:org_id, ?1) ON CONFLICT(job_id) DO UPDATE SET state = ?2 WHERE jobs.org_id = :org_id",
        "INSERT INTO jobs (org_id, job_id) SELECT :org_id, ?1 ON CONFLICT(job_id) DO UPDATE SET state = ?2 WHERE +jobs.org_id = :org_id",
        "SELECT job_id FROM jobs WHERE +org_id = :org_id AND job_id = ?1",
    ] {
        assert_eq!(unscoped(sql, &tables), None, "{sql}");
    }
}

#[test]
fn guard_checks_runtime_strings_after_test_modules() -> Result<(), Box<dyn std::error::Error>> {
    let tables = HashSet::from(["jobs".to_owned()]);
    let source = r#"
        #[cfg(test)]
        pub(crate) mod tests {
            const FIXTURE: &str = "SELECT skipped FROM jobs";
        }
        // #[cfg(test)]
        mod tests_like_runtime {
            const NEWLINE: &str = "SELECT x\nFROM jobs WHERE job_id = ?1";
            const TAB: &str = "SELECT x\tFROM jobs WHERE job_id = ?1";
            const CONTINUATION: &str = "SELECT x \
                FROM jobs WHERE job_id = ?1";
        }
    "#;
    let strings = rust_strings(source)?;
    assert_eq!(strings.len(), 3);
    for (_, sql) in strings {
        assert!(unscoped(&sql, &tables).is_some(), "{sql:?}");
    }
    assert!(exempt(Path::new("store/teams.rs")));
    for path in [
        "teams.rs",
        "future/teams_tests.rs",
        "future/review_regressions.rs",
    ] {
        assert!(!exempt(Path::new(path)), "{path}");
    }
    Ok(())
}

#[test]
fn control_scope_survives_appended_or() -> Result<(), Box<dyn std::error::Error>> {
    let tables = HashSet::from(["controls".to_owned()]);
    let suffix = " WHERE node_id = ?1 OR control_id = ?2";
    let unsafe_query = "SELECT control_id FROM controls WHERE +org_id = :org_id AND node_id = ?1 OR control_id = ?2";
    assert!(unscoped(unsafe_query, &tables).is_some());
    let strings = rust_strings(include_str!("../control.rs"))?;
    let (_, base) = strings
        .iter()
        .find(|(_, sql)| sql.starts_with("SELECT control_id, kind, state,"))
        .ok_or("missing control base SELECT")?;
    assert_eq!(unscoped(&format!("{base}{suffix}"), &tables), None);
    Ok(())
}
