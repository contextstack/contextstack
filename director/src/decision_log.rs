//! The installation owns its projects. Paired devices act for that owner;
//! hosted leads can use only the log of their conversation's project.

use crate::access::{Client, Kind};
use crate::records::{self, View};
use crate::server::{Call, Reply, State, json};
use crate::store::ScopedConnection;
use crate::store::{Record, touch, write_tx};
use crate::util::{now_utc, percent_decode, ulid};
use crate::work::{self, No};
use rusqlite::params;
use serde_json::{Map, Value, json};
use std::fmt::Write as _;

pub const LESSONS_PATH: &str = "conversation/lessons.md";
pub const TURN_PATH: &str = "conversation/decision-log.md";
const TURN_LIMIT: usize = 12_000;
const HEADING: &str = "# Decision log\n";

const KIND: Record = Record::DecisionLog;
const SELECT: &str = "SELECT entry_id, project_id, number, body, status, superseded_by,
    created_by, created_at, updated_by, updated_at, revision FROM (SELECT * FROM decision_log WHERE +org_id = :org_id) AS decision_log";

pub(crate) const VIEW: View = View {
    select: SELECT,
    key: "entry_id",
    order: "number DESC",
    shape: |_, row| shape(row),
};

fn shape(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let raw: String = row.get(3)?;
    let mut entry: Value = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    entry["entry_id"] = json!(row.get::<_, String>(0)?);
    entry["project_id"] = json!(row.get::<_, String>(1)?);
    entry["number"] = json!(format!("D{}", row.get::<_, i64>(2)?));
    entry["status"] = json!(row.get::<_, String>(4)?);
    entry["superseded_by"] = json!(row.get::<_, Option<String>>(5)?);
    for (column, name) in ["created_by", "created_at", "updated_by", "updated_at"]
        .into_iter()
        .enumerate()
    {
        entry[name] = json!(row.get::<_, String>(column + 6)?);
    }
    entry["revision"] = json!(row.get::<_, i64>(10)?);
    Ok(entry)
}

pub fn project_visible(db: &ScopedConnection, client: &Client, id: &str) -> rusqlite::Result<bool> {
    if client.kind != Kind::Lead {
        return Ok(true);
    }
    db.prepare("SELECT 1 FROM conversations WHERE +org_id = :org_id AND conversation_id = ?1 AND project_id = ?2")?
        .exists(params![client.conversation_id, id])
}

fn missing() -> No {
    No(
        404,
        "not_found",
        "no such project or decision log entry".into(),
        Value::Null,
    )
}

fn conflict(detail: &str) -> No {
    No(409, "conflict", detail.into(), Value::Null)
}

fn check_project(db: &ScopedConnection, client: &Client, project: &str) -> Result<(), No> {
    if !project_visible(db, client, project).map_err(|e| No::internal(&e))? {
        return Err(missing());
    }
    work::project_exists(db, Some(project)).map_err(|e| if e.0 == 422 { missing() } else { e })
}

fn list(
    db: &ScopedConnection,
    project: &str,
    status: Option<&str>,
    search: Option<&str>,
) -> Result<Vec<Value>, No> {
    let mut stmt = db
        .prepare(&format!(
            "{SELECT} WHERE project_id = ?1
        AND (?2 IS NULL OR status = ?2) ORDER BY number"
        ))
        .map_err(|e| No::internal(&e))?;
    let search = search.map(str::to_lowercase);
    let mut entries = Vec::new();
    for row in stmt
        .query_map(params![project, status], shape)
        .map_err(|e| No::internal(&e))?
    {
        let entry = row.map_err(|e| No::internal(&e))?;
        if search.as_ref().is_none_or(|q| matches_search(&entry, q)) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn matches_search(entry: &Value, query: &str) -> bool {
    fn contains(value: &Value, query: &str) -> bool {
        match value {
            Value::String(text) => text.to_lowercase().contains(query),
            Value::Array(items) => items.iter().any(|item| contains(item, query)),
            Value::Object(fields) => fields.values().any(|item| contains(item, query)),
            _ => false,
        }
    }
    FIELDS
        .into_iter()
        .chain(["number"])
        .any(|field| contains(&entry[field], query))
}

pub fn attachment(db: &ScopedConnection, client: &Client, project: &str) -> Result<String, No> {
    check_project(db, client, project)?;
    Ok(render(&list(db, project, Some("active"), None)?))
}

pub struct TurnLog {
    pub markdown: String,
    pub lessons: String,
    pub revision: i64,
    pub entry_count: usize,
}

pub fn for_turn(db: &ScopedConnection, project: &str) -> Result<Option<TurnLog>, No> {
    let entries = list(db, project, Some("active"), None)?;
    if entries.is_empty() {
        return Ok(None);
    }
    // A retirement or supersession also changes the active snapshot.
    let revision = db
        .query_row(
            "SELECT MAX(revision) FROM decision_log WHERE +org_id = :org_id AND project_id = ?1",
            [project],
            |row| row.get(0),
        )
        .map_err(|e| No::internal(&e))?;
    let (lessons, decisions): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|entry| entry["kind"] == "lesson");
    let lessons = render_lessons(lessons);
    let (markdown, entry_count) = render_turn(decisions);
    Ok(Some(TurnLog {
        markdown,
        lessons,
        revision,
        entry_count,
    }))
}

fn render_lessons(mut entries: Vec<Value>) -> String {
    fn one_line(value: &Value, limit: usize) -> String {
        value
            .as_str()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(limit)
            .collect()
    }
    entries.sort_by_key(|entry| {
        std::cmp::Reverse((
            entry["sources"].as_array().map_or(0, Vec::len),
            entry["revision"].as_i64(),
        ))
    });
    let mut out = String::from("# Lessons\n");
    for entry in entries.iter().take(38) {
        let _ = writeln!(
            out,
            "- {} {} — {}",
            entry["number"].as_str().unwrap_or_default(),
            one_line(&entry["decision"], 140),
            one_line(&entry["why"], 180)
        );
    }
    if entries.len() > 38 {
        let _ = writeln!(
            out,
            "- {} more lessons in the decision log.",
            entries.len() - 38
        );
    }
    out
}

fn render_turn(mut entries: Vec<Value>) -> (String, usize) {
    let full = render(&entries);
    if full.chars().count() <= TURN_LIMIT {
        return (full, entries.len());
    }

    // Revisions are allocated in write order, including updates within the same clock tick.
    entries.sort_by_key(|entry| std::cmp::Reverse(entry["revision"].as_i64()));
    let mut out = HEADING.to_owned();
    let mut remaining = TURN_LIMIT - HEADING.len();
    let mut included = 0;
    let note = |omitted| {
        format!(
            "\nOmitted entries from the active decision log: {omitted}. Use `decision_log` to fetch them all.\n"
        )
    };
    for entry in &entries {
        let mut section = String::new();
        render_entry(&mut section, entry);
        let size = section.chars().count();
        if size + note(entries.len() - included - 1).len() > remaining {
            break;
        }
        out.push_str(&section);
        remaining -= size;
        included += 1;
    }
    out.push_str(&note(entries.len() - included));
    (out, included)
}

fn render(entries: &[Value]) -> String {
    let mut out = HEADING.to_owned();
    for entry in entries {
        render_entry(&mut out, entry);
    }
    out
}

fn render_entry(out: &mut String, entry: &Value) {
    let text = |v: &Value| v.as_str().unwrap_or("").to_owned();
    let _ = writeln!(
        out,
        "\n## {} — {}\n\nStatus: {}",
        text(&entry["number"]),
        text(&entry["decision"]),
        text(&entry["status"])
    );
    if let Some(id) = entry["superseded_by"].as_str() {
        let _ = writeln!(out, "Superseded by: {id}");
    }
    let _ = writeln!(out, "\n**Why**\n\n{}", text(&entry["why"]));
    if let Some(alternatives) = entry["alternatives"].as_array().filter(|a| !a.is_empty()) {
        out.push_str("\n**Alternatives considered**\n\n");
        for item in alternatives {
            let _ = writeln!(
                out,
                "- {} — {}",
                text(&item["option"]),
                text(&item["why_not"])
            );
        }
    }
    for (key, label) in [("reopen_when", "Reopen when"), ("sources", "Sources")] {
        if let Some(items) = entry[key].as_array().filter(|a| !a.is_empty()) {
            let _ = writeln!(out, "\n**{label}**\n");
            for item in items {
                let _ = writeln!(out, "- {}", text(item));
            }
        }
    }
}

fn text_field(value: &Value, name: &str) -> Result<(), No> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|_| ())
        .ok_or_else(|| No::invalid(&format!("{name} must be non-empty text")))
}

const FIELDS: [&str; 6] = [
    "decision",
    "why",
    "alternatives",
    "reopen_when",
    "sources",
    "kind",
];

fn fields(input: &Map<String, Value>, previous: Option<&Value>) -> Result<Value, No> {
    let mut content = json!({});
    for name in FIELDS.into_iter().filter(|name| *name != "kind") {
        content[name] = input
            .get(name)
            .or_else(|| previous.and_then(|v| v.get(name)))
            .cloned()
            .unwrap_or_else(|| {
                if matches!(name, "decision" | "why") {
                    Value::Null
                } else {
                    json!([])
                }
            });
    }
    if let Some(kind) = input
        .get("kind")
        .or_else(|| previous.and_then(|v| v.get("kind")))
    {
        if kind != "lesson" && kind != "decision" {
            return Err(No::invalid("kind must be lesson or decision"));
        }
        content["kind"] = kind.clone();
    }
    text_field(&content["decision"], "decision")?;
    if content["decision"]
        .as_str()
        .is_some_and(|s| s.contains(['\r', '\n']))
    {
        return Err(No::invalid("decision must be one line"));
    }
    text_field(&content["why"], "why")?;
    for name in ["reopen_when", "sources", "alternatives"] {
        for item in content[name]
            .as_array()
            .ok_or_else(|| No::invalid(&format!("{name} must be an array")))?
        {
            if name == "alternatives" {
                if item.as_object().is_none_or(|m| m.len() != 2) {
                    return Err(No::invalid("alternatives must contain {option, why_not}"));
                }
                text_field(&item["option"], "option")?;
                text_field(&item["why_not"], "why_not")?;
            } else {
                text_field(item, name)?;
            }
        }
    }
    Ok(content)
}

fn entry(db: &ScopedConnection, project: &str, id: &str) -> Result<Value, No> {
    records::one(db, KIND, id)
        .map_err(|e| No::internal(&e))?
        .filter(|v| v["project_id"] == project)
        .ok_or_else(missing)
}

fn insert(
    db: &ScopedConnection,
    client: &Client,
    project: &str,
    number: i64,
    body: &Value,
) -> Result<String, No> {
    let id = format!(
        "dle_{}",
        ulid().map_err(|e| No::internal(&e))?.to_lowercase()
    );
    let added = db.execute(
        "INSERT INTO decision_log (org_id, entry_id, project_id, number, body, status, created_by, created_at, updated_by, updated_at)
         VALUES (:org_id, ?1, ?2, ?3, ?4, 'active', ?5, ?6, ?5, ?6) ON CONFLICT(project_id, number) DO NOTHING",
        params![id, project, number, body.to_string(), client.id, now_utc()],
    ).map_err(|e| No::internal(&e))?;
    if added == 0 {
        return Err(conflict(&format!("D{number} already exists")));
    }
    touch(db, KIND, &id).map_err(|e| No::internal(&e))?;
    Ok(id)
}

fn update(
    db: &ScopedConnection,
    client: &Client,
    project: &str,
    id: &str,
    op: &str,
    input: &Map<String, Value>,
) -> Result<(), No> {
    let old = entry(db, project, id)?;
    if input.get("revision").and_then(Value::as_i64) != old["revision"].as_i64() {
        return Err(conflict(
            "refresh the entry and supply its current revision",
        ));
    }
    if old["status"] != "active" {
        return Err(conflict("only active entries can be changed"));
    }
    let replacement = if op == "supersede" {
        let target = input
            .get("superseded_by")
            .and_then(Value::as_str)
            .ok_or_else(|| No::invalid("superseded_by must name an active replacement"))?;
        if target == id || entry(db, project, target)?["status"] != "active" {
            return Err(No::invalid(
                "superseded_by must name a different active entry",
            ));
        }
        Some(target)
    } else {
        None
    };
    let status = match op {
        "supersede" => "superseded",
        "retire" => "retired",
        _ => "active",
    };
    db.execute(
        "INSERT INTO decision_log_revisions (org_id, entry_id, revision, snapshot) VALUES (:org_id, ?1, ?2, ?3)",
        params![id, old["revision"].as_i64(), old.to_string()],
    )
    .map_err(|e| No::internal(&e))?;
    db.execute("UPDATE decision_log SET body = ?2, status = ?3, superseded_by = ?4, updated_by = ?5, updated_at = ?6
                  WHERE +org_id = :org_id AND (entry_id = ?1)",
        params![id, fields(input, Some(&old))?.to_string(), status, replacement, client.id, now_utc()])
        .map_err(|e| No::internal(&e))?;
    touch(db, KIND, id).map_err(|e| No::internal(&e))?;
    Ok(())
}

fn entry_answer(db: &ScopedConnection, project: &str, id: &str) -> Result<Value, No> {
    let entry = entry(db, project, id)?;
    let mut stmt = db
        .prepare(
            "SELECT snapshot FROM decision_log_revisions WHERE +org_id = :org_id AND entry_id = ?1 ORDER BY revision",
        )
        .map_err(|e| No::internal(&e))?;
    let history = stmt
        .query_map([id], |r| r.get::<_, String>(0))
        .map_err(|e| No::internal(&e))?
        .map(|row| {
            serde_json::from_str::<Value>(&row.map_err(|e| No::internal(&e))?)
                .map_err(|e| No::internal(&e))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"entry": entry, "revisions": history, "markdown": render(&[entry])}))
}

fn parse_table(markdown: &str) -> Result<Vec<(i64, Value)>, No> {
    let cells = |line: &str| {
        let line = line.trim();
        let line = line.strip_prefix('|').unwrap_or(line);
        let stripped = line
            .strip_suffix('|')
            .filter(|s| !s.ends_with('\\'))
            .unwrap_or(line);
        let mut cells = vec![String::new()];
        let mut chars = stripped.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '|' {
                cells.push(String::new());
            } else if let Some(cell) = cells.last_mut() {
                if ch == '\\' && chars.peek() == Some(&'|') {
                    chars.next();
                    cell.push('|');
                } else {
                    cell.push(ch);
                }
            }
        }
        cells
            .into_iter()
            .map(|cell| cell.trim().to_owned())
            .collect::<Vec<_>>()
    };
    let mut lines = markdown.lines().filter(|line| !line.trim().is_empty());
    let header = cells(lines.next().unwrap_or(""));
    let valid_header = matches!(header.as_slice(), [num, decision, why, reopen, source]
        if num == "#" && decision == "Decision" && why == "Why" && reopen.starts_with("Reopen when") && source == "Source");
    let separator = cells(lines.next().unwrap_or(""));
    if !valid_header
        || separator.len() != 5
        || !separator.iter().all(|s| {
            let s = s.trim_matches(':');
            !s.is_empty() && s.bytes().all(|b| b == b'-')
        })
    {
        return Err(No::invalid(
            "expected a # | Decision | Why | Reopen when | Source markdown table",
        ));
    }
    let values = |s: &str| {
        if matches!(s, "" | "—" | "-") {
            json!([])
        } else {
            json!([s])
        }
    };
    let rows = lines
        .map(|line| {
            let row = cells(line);
            let [number, decision, why, reopen, source] = row.as_slice() else {
                return Err(No::invalid("expected five cells in each table row"));
            };
            let number = number
                .strip_prefix('D')
                .and_then(|n| n.parse::<i64>().ok())
                .filter(|n| *n > 0 && format!("D{n}") == *number)
                .ok_or_else(|| No::invalid("expected a positive decision number such as D1"))?;
            let input = Map::from_iter([
                ("decision".into(), json!(decision)),
                ("why".into(), json!(why)),
                ("reopen_when".into(), values(reopen)),
                (
                    "sources".into(),
                    if matches!(source.as_str(), "" | "—" | "-") {
                        json!([])
                    } else {
                        json!(source.split("; ").collect::<Vec<_>>())
                    },
                ),
            ]);
            Ok((number, fields(&input, None)?))
        })
        .collect::<Result<Vec<_>, No>>()?;
    if rows.is_empty() {
        return Err(No::invalid("the table must contain at least one entry"));
    }
    Ok(rows)
}

fn import_rows(
    db: &ScopedConnection,
    client: &Client,
    project: &str,
    rows: &[(i64, Value)],
    replayed: bool,
) -> Result<Vec<Value>, No> {
    let mut entries = Vec::with_capacity(rows.len());
    for (number, body) in rows {
        let id = if replayed {
            db.query_row(
                "SELECT entry_id FROM decision_log WHERE +org_id = :org_id AND project_id = ?1 AND number = ?2",
                params![project, number],
                |r| r.get(0),
            )
            .map_err(|e| No::internal(&e))?
        } else {
            insert(db, client, project, *number, body)?
        };
        entries.push(entry(db, project, &id)?);
    }
    Ok(entries)
}

fn write(
    state: &State,
    call: &Call,
    client: &Client,
    project: &str,
    id: Option<&str>,
    op: &str,
) -> Result<Reply, No> {
    let input = work::body(call)?;
    let key = work::key(call)?;
    let sha = work::content(
        &format!("decision_log_{op}"),
        &format!("{project}/{}", id.unwrap_or("")),
        &json!(input),
    );
    let mut db = state.store.db();
    let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
    check_project(&tx, client, project)?;
    let valid_key = |name: &str| match op {
        "import" => name == "markdown",
        "supersede" => matches!(name, "revision" | "superseded_by"),
        "retire" => name == "revision",
        "revise" => name == "revision" || FIELDS.contains(&name),
        _ => FIELDS.contains(&name),
    };
    if input.keys().any(|name| !valid_key(name)) {
        return Err(No::invalid(
            "unexpected field for this decision log operation",
        ));
    }
    let prior = work::scoped_prior(&tx, &key, &sha, client)?;
    let replayed = prior.is_some();
    let (id, answer) = if op == "import" {
        let rows = parse_table(
            input
                .get("markdown")
                .and_then(Value::as_str)
                .ok_or_else(|| No::invalid("markdown must be a table"))?,
        )?;
        let entries = import_rows(&tx, client, project, &rows, replayed)?;
        let id = entries[0]["entry_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        (
            id,
            json!({"entries": entries, "markdown": render(&entries)}),
        )
    } else {
        let id = if let Some((_, id)) = prior {
            id
        } else if let Some(id) = id {
            update(&tx, client, project, id, op, &input)?;
            id.to_owned()
        } else {
            let number: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(number), 0) FROM decision_log WHERE +org_id = :org_id AND project_id = ?1",
                    [project],
                    |r| r.get(0),
                )
                .map_err(|e| No::internal(&e))?;
            let number = number
                .checked_add(1)
                .ok_or_else(|| conflict("decision numbers exhausted"))?;
            insert(&tx, client, project, number, &fields(&input, None)?)?
        };
        let answer = entry_answer(&tx, project, &id)?;
        (id, answer)
    };
    if !replayed {
        work::remember(&tx, &key, &sha, KIND, &id, client).map_err(|e| No::internal(&e))?;
    }
    tx.commit().map_err(|e| No::internal(&e))?;
    let mut reply = json(
        if replayed || !matches!(op, "create" | "import") {
            200
        } else {
            201
        },
        &answer,
    );
    if replayed {
        reply.headers_mut().insert(
            "idempotent-replayed",
            hyper::header::HeaderValue::from_static("true"),
        );
    }
    Ok(reply)
}

pub fn handle(state: &State, call: &Call, client: &Client, project: &str, rest: &[&str]) -> Reply {
    let read = || {
        let db = state.store.db();
        check_project(&db, client, project)?;
        match rest {
            [] => {
                let status = call.query("status");
                if status.is_some_and(|s| !matches!(s, "active" | "superseded" | "retired")) {
                    return Err(No::invalid("status must be active, superseded or retired"));
                }
                let search = call
                    .query("q")
                    .map(|q| {
                        percent_decode(&q.replace('+', " "))
                            .ok_or_else(|| No::invalid("q must be URL encoded text"))
                    })
                    .transpose()?;
                let entries = list(&db, project, status, search.as_deref())?;
                Ok(json!({"entries": entries, "markdown": render(&entries)}))
            }
            [id] => entry_answer(&db, project, id),
            _ => Err(missing()),
        }
    };
    let result = match (call.method.as_str(), rest) {
        ("GET", _) => read().map(|answer| json(200, &answer)),
        ("POST", []) => write(state, call, client, project, None, "create"),
        ("POST", ["import"]) => write(state, call, client, project, None, "import"),
        ("POST", [id, op @ ("revise" | "supersede" | "retire")]) => {
            write(state, call, client, project, Some(id), op)
        }
        _ => Err(missing()),
    };
    result.unwrap_or_else(No::reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_log_preserves_markdown_when_it_fits() {
        let entries = vec![
            json!({"number": "D1", "decision": "Use the Director", "status": "active",
            "why": "Shared state", "alternatives": [{"option": "Local state", "why_not": "Drift"}],
            "reopen_when": ["Offline mode"], "revision": 1}),
        ];
        assert_eq!(render_turn(entries.clone()), (render(&entries), 1));
    }

    #[test]
    fn hosted_log_keeps_whole_entries_in_update_order_within_the_character_limit() {
        let entry = |number, revision| {
            json!({"number": number, "decision": "A choice", "status": "active",
                "why": "é".repeat(5_500), "revision": revision})
        };
        let (markdown, count) = render_turn(vec![entry("D1", 5), entry("D2", 2), entry("D3", 3)]);
        assert_eq!(count, 2);
        assert!(markdown.chars().count() <= TURN_LIMIT);
        let headings: Vec<_> = markdown
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect();
        assert_eq!(headings, ["## D1 — A choice", "## D3 — A choice"]);
        assert!(!markdown.contains("## D2"));
        assert!(markdown.ends_with(
            "Omitted entries from the active decision log: 1. Use `decision_log` to fetch them all.\n"
        ));
    }

    #[test]
    fn hosted_log_reports_when_no_entry_fits() {
        let (markdown, count) =
            render_turn(vec![json!({"number": "D1", "why": "é".repeat(TURN_LIMIT)})]);
        assert_eq!(count, 0);
        assert!(!markdown.contains("## D1"));
        assert!(markdown.contains("Omitted entries from the active decision log: 1"));
    }
}
