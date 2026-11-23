//! Installation-owned project backlogs, shared by paired devices and project-bound leads.

use crate::access::Client;
use crate::records::{self, View};
use crate::server::{Call, Reply, State, json};
use crate::store::{Record, touch, write_tx};
use crate::util::{now_utc, ulid};
use crate::work::{self, No};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS backlog_items (
  item_id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(project_id),
  status TEXT NOT NULL CHECK (status IN ('next','now','blocked','done','dropped')),
  priority INTEGER NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL, created_by TEXT NOT NULL,
  updated_at TEXT NOT NULL, updated_by TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS backlog_project_order ON backlog_items(project_id, priority, item_id);
CREATE TABLE IF NOT EXISTS backlog_revisions (
  item_id TEXT NOT NULL REFERENCES backlog_items(item_id), revision INTEGER NOT NULL, snapshot TEXT NOT NULL,
  PRIMARY KEY(item_id, revision));
CREATE TABLE IF NOT EXISTS project_heartbeats (
  project_id TEXT PRIMARY KEY REFERENCES projects(project_id),
  conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id),
  interval_seconds INTEGER NOT NULL CHECK (interval_seconds > 0),
  due_at INTEGER NOT NULL);
";

const KIND: Record = Record::BacklogItem;
const SELECT: &str = "SELECT item_id, project_id, status, priority, data, created_at, created_by,
    updated_at, updated_by, revision FROM backlog_items";
const ORDER: &str = "priority, item_id";
const STATUSES: [&str; 5] = ["next", "now", "blocked", "done", "dropped"];
const LINKS: [&str; 3] = ["intent_ids", "job_ids", "decision_log_entry_ids"];
const FIELDS: [&str; 7] = [
    "title",
    "body",
    "status",
    "priority",
    "blocked_on",
    "blocked_reason",
    "stop",
];

pub(crate) const VIEW: View = View {
    select: SELECT,
    key: "item_id",
    order: ORDER,
    shape: |_, row| shape(row),
};

fn snapshot(raw: &str) -> rusqlite::Result<Value> {
    serde_json::from_str(raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn shape(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut item = snapshot(&row.get::<_, String>(4)?)?;
    for (name, column) in [
        ("item_id", 0),
        ("project_id", 1),
        ("status", 2),
        ("created_at", 5),
        ("created_by", 6),
        ("updated_at", 7),
        ("updated_by", 8),
    ] {
        item[name] = json!(row.get::<_, String>(column)?);
    }
    item["priority"] = json!(row.get::<_, i64>(3)?);
    item["revision"] = json!(row.get::<_, i64>(9)?);
    Ok(item)
}

pub(crate) const OWNER_BLOCKED: &str = "status IN ('next','now','blocked') AND EXISTS (
    SELECT 1 FROM json_each(data, '$.blocked_on') b WHERE b.value ->> 'kind' = 'owner')";

const CLEAR: &str = "NOT EXISTS (
    SELECT 1 FROM json_each(data, '$.blocked_on') b
    WHERE NOT (
        (b.value ->> 'kind' = 'item' AND EXISTS (
            SELECT 1 FROM backlog_items dep WHERE dep.item_id = b.value ->> 'ref'
              AND dep.project_id = backlog_items.project_id AND dep.status IN ('done','dropped')))
        OR
        (b.value ->> 'kind' = 'job' AND EXISTS (
            SELECT 1 FROM jobs j JOIN intents i ON i.intent_id = j.intent_id
             WHERE j.job_id = b.value ->> 'ref' AND i.project_id = backlog_items.project_id
               AND j.finished_at IS NOT NULL))
    ))";

fn missing() -> No {
    No(
        404,
        "not_found",
        "no such project or backlog item".into(),
        Value::Null,
    )
}

pub(crate) fn check_project(db: &Connection, client: &Client, project: &str) -> Result<(), No> {
    if !crate::decision_log::project_visible(db, client, project).map_err(|e| No::internal(&e))? {
        return Err(missing());
    }
    work::project_exists(db, Some(project)).map_err(|e| if e.0 == 422 { missing() } else { e })
}

fn item(db: &Connection, project: &str, id: &str) -> Result<Value, No> {
    records::one(db, KIND, id)
        .map_err(|e| No::internal(&e))?
        .filter(|item| item["project_id"] == project)
        .ok_or_else(missing)
}

fn read(db: &Connection, project: &str, rest: &[&str], status: Option<&str>) -> Result<Value, No> {
    let result = match rest {
        ["next_unblocked"] => db.query_row(
            &format!("{SELECT} WHERE project_id = ?1 AND status = 'next' AND {CLEAR} ORDER BY {ORDER} LIMIT 1"),
            [project], shape,
        ).optional().map(|item| json!({"item": item})),
        [] | ["needs_you"] => {
            if status.is_some_and(|s| !STATUSES.contains(&s)) {
                return Err(No::invalid("unknown backlog status"));
            }
            let filter = if rest.is_empty() { "1" } else { OWNER_BLOCKED };
            let mut stmt = db.prepare(&format!(
                "{SELECT} WHERE project_id = ?1 AND (?2 IS NULL OR status = ?2) AND {filter} ORDER BY {ORDER}"
            )).map_err(|e| No::internal(&e))?;
            stmt.query_map(params![project, status], shape)
                .and_then(Iterator::collect::<Result<Vec<_>, _>>)
                .map(|items| json!({"items": items}))
        }
        [id] => return Ok(json!({"item": item(db, project, id)?, "revisions": history(db, id).map_err(|e| No::internal(&e))?})),
        _ => return Err(missing()),
    };
    result.map_err(|e| No::internal(&e))
}

fn history(db: &Connection, id: &str) -> rusqlite::Result<Vec<Value>> {
    db.prepare("SELECT snapshot FROM backlog_revisions WHERE item_id = ?1 ORDER BY revision")?
        .query_map([id], |row| snapshot(&row.get::<_, String>(0)?))?
        .collect()
}

pub(crate) fn attention(db: &Connection, conversation: &str) -> rusqlite::Result<Value> {
    db.query_row(&format!(
        "{SELECT} WHERE project_id = (SELECT project_id FROM conversations WHERE conversation_id = ?1)
          AND {OWNER_BLOCKED} ORDER BY {ORDER} LIMIT 1"
    ), [conversation], shape).optional().map(|item| item.map_or(Value::Null, |item| {
        json!({"source_kind": "backlog_item", "backlog_item_id": item["item_id"],
               "question": item["blocked_reason"].as_str().filter(|s| !s.is_empty())
                   .or_else(|| item["title"].as_str()),
               "set_at": item["updated_at"], "set_by": item["updated_by"], "job_id": null})
    }))
}

fn nonempty(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.trim().is_empty())
}

fn fields(input: &Map<String, Value>, old: Option<&Value>) -> Result<Value, No> {
    if input.keys().any(|key| {
        !FIELDS.contains(&key.as_str())
            && !LINKS.contains(&key.as_str())
            && (old.is_none() || key != "revision")
    }) {
        return Err(No::invalid("unknown backlog field"));
    }
    let mut data = json!({
        "title": null, "body": "", "status": "next", "priority": 0, "blocked_on": [],
        "blocked_reason": "", "stop": false, "intent_ids": [], "job_ids": [], "decision_log_entry_ids": []
    });
    for key in FIELDS.into_iter().chain(LINKS) {
        if let Some(value) = input.get(key).or_else(|| old.and_then(|v| v.get(key))) {
            data[key] = value.clone();
        }
    }
    if !nonempty(&data["title"]) || !data["body"].is_string() || !data["blocked_reason"].is_string()
    {
        return Err(No::invalid(
            "title must be non-empty and body and blocked_reason must be text",
        ));
    }
    if !STATUSES.contains(&data["status"].as_str().unwrap_or_default())
        || !data["priority"].is_i64()
        || !data["stop"].is_boolean()
    {
        return Err(No::invalid("invalid backlog status, priority or stop flag"));
    }
    for key in LINKS {
        if !data[key]
            .as_array()
            .is_some_and(|ids| ids.iter().all(nonempty))
        {
            return Err(No::invalid(&format!("{key} must be an array of IDs")));
        }
    }
    let blockers = data["blocked_on"]
        .as_array()
        .ok_or_else(|| No::invalid("blocked_on must be an array"))?;
    if blockers.iter().any(|b| {
        b.as_object().is_none_or(|m| m.len() != 2)
            || !matches!(
                b["kind"].as_str(),
                Some("owner" | "job" | "item" | "external")
            )
            || !nonempty(&b["ref"])
    }) {
        return Err(No::invalid(
            "each blocker must have a kind and non-empty ref",
        ));
    }
    Ok(data)
}

fn changed(db: &Connection, project: &str, id: &str, conversation_changed: bool) -> Result<(), No> {
    touch(db, KIND, id).map_err(|e| No::internal(&e))?;
    if conversation_changed {
        let mut stmt = db
            .prepare("SELECT conversation_id FROM conversations WHERE lead = 1 AND project_id = ?1")
            .map_err(|e| No::internal(&e))?;
        let conversations = stmt
            .query_map([project], |r| r.get::<_, String>(0))
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(|e| No::internal(&e))?;
        for conversation in conversations {
            touch(db, Record::Conversation, &conversation).map_err(|e| No::internal(&e))?;
        }
    }
    Ok(())
}

fn owner_blocked(item: &Value) -> bool {
    matches!(item["status"].as_str(), Some("next" | "now" | "blocked"))
        && item["blocked_on"]
            .as_array()
            .is_some_and(|blockers| blockers.iter().any(|b| b["kind"] == "owner"))
}

fn save(
    db: &Connection,
    client: &Client,
    project: &str,
    id: Option<&str>,
    input: &Map<String, Value>,
) -> Result<String, No> {
    let old = id.map(|id| item(db, project, id)).transpose()?;
    if let Some(old) = &old
        && input.get("revision") != old.get("revision")
    {
        return Err(No(
            409,
            "conflict",
            "supply the current backlog item revision".into(),
            Value::Null,
        ));
    }
    let data = fields(input, old.as_ref())?;
    let now = now_utc();
    let id = if let (Some(id), Some(old)) = (id, &old) {
        db.execute(
            "INSERT INTO backlog_revisions VALUES (?1, ?2, ?3)",
            params![id, old["revision"].as_i64(), old.to_string()],
        )
        .map_err(|e| No::internal(&e))?;
        db.execute("UPDATE backlog_items SET status = ?2, priority = ?3, data = ?4, updated_at = ?5, updated_by = ?6
                    WHERE item_id = ?1",
                   params![id, data["status"].as_str(), data["priority"].as_i64(), data.to_string(), now, client.id])
            .map_err(|e| No::internal(&e))?;
        id.to_owned()
    } else {
        let id = format!(
            "bli_{}",
            ulid().map_err(|e| No::internal(&e))?.to_lowercase()
        );
        db.execute("INSERT INTO backlog_items (item_id, project_id, status, priority, data, created_at, created_by, updated_at, updated_by)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?7)",
                   params![id, project, data["status"].as_str(), data["priority"].as_i64(), data.to_string(), now, client.id])
            .map_err(|e| No::internal(&e))?;
        id
    };
    changed(
        db,
        project,
        &id,
        owner_blocked(&data) || old.as_ref().is_some_and(owner_blocked),
    )?;
    Ok(id)
}

fn reorder(
    db: &Connection,
    client: &Client,
    project: &str,
    input: &Map<String, Value>,
) -> Result<String, No> {
    let changes = input
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty() && input.len() == 1)
        .ok_or_else(|| No::invalid("reorder needs a non-empty items array"))?;
    let mut ids = HashSet::new();
    for change in changes {
        let id = change["item_id"]
            .as_str()
            .filter(|_| change.as_object().is_some_and(|m| m.len() == 3))
            .ok_or_else(|| {
                No::invalid("each reorder entry needs item_id, revision and priority")
            })?;
        if !ids.insert(id) {
            return Err(No::invalid("reorder item IDs must be unique"));
        }
        let input = Map::from_iter([
            ("revision".into(), change["revision"].clone()),
            ("priority".into(), change["priority"].clone()),
        ]);
        save(db, client, project, Some(id), &input)?;
    }
    Ok(changes[0]["item_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

fn write(
    state: &State,
    call: &Call,
    client: &Client,
    project: &str,
    rest: &[&str],
) -> Result<Reply, No> {
    let input = work::body(call)?;
    let key = work::key(call)?;
    let sha = work::content(
        "backlog",
        &format!("{project}/{}", rest.join("/")),
        &json!(input),
    );
    let mut db = state.store.db();
    let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
    check_project(&tx, client, project)?;
    let prior = work::scoped_prior(&tx, &key, &sha, client)?;
    let replayed = prior.is_some();
    let id = if let Some((_, id)) = prior {
        id
    } else {
        let id = match rest {
            ["reorder"] => reorder(&tx, client, project, &input)?,
            [] => save(&tx, client, project, None, &input)?,
            [id, "update"] => save(&tx, client, project, Some(id), &input)?,
            _ => return Err(missing()),
        };
        work::remember(&tx, &key, &sha, KIND, &id, client).map_err(|e| No::internal(&e))?;
        id
    };
    let answer = if rest == ["reorder"] {
        read(&tx, project, &[], None)?
    } else {
        json!({"item": item(&tx, project, &id)?})
    };
    tx.commit().map_err(|e| No::internal(&e))?;
    let mut reply = json(
        if !replayed && rest.is_empty() {
            201
        } else {
            200
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

pub(crate) fn handle(
    state: &State,
    call: &Call,
    client: &Client,
    project: &str,
    rest: &[&str],
) -> Reply {
    let result = match (call.method.as_str(), rest) {
        ("GET", _) => {
            let db = state.store.db();
            check_project(&db, client, project)
                .and_then(|()| read(&db, project, rest, call.query("status")))
                .map(|answer| json(200, &answer))
        }
        ("POST", [] | ["reorder"] | [_, "update"]) => write(state, call, client, project, rest),
        _ => Err(missing()),
    };
    result.unwrap_or_else(No::reply)
}
