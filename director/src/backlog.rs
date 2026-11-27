//! Installation-owned project backlogs, shared by paired devices and project-bound leads.

use crate::access::Client;
use crate::records::{self, View};
use crate::server::{Call, Reply, State, json};
use crate::store::ScopedConnection;
use crate::store::{Record, touch, write_tx};
use crate::util::{now_utc, ulid};
use crate::work::{self, No};
use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fmt::Write as _;

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
    updated_at, updated_by, revision, assignee_member_id FROM (SELECT * FROM backlog_items WHERE +org_id = :org_id) AS backlog_items";
const ORDER: &str = "priority, item_id";
const STATUSES: [&str; 5] = ["next", "now", "blocked", "done", "dropped"];
const LINKS: [&str; 3] = ["intent_ids", "job_ids", "decision_log_entry_ids"];
const FIELDS: [&str; 8] = [
    "title",
    "body",
    "status",
    "priority",
    "blocked_on",
    "blocked_reason",
    "needs_you",
    "stop",
];

pub(crate) const VIEW: View = View {
    select: SELECT,
    key: "item_id",
    order: ORDER,
    shape,
};

fn snapshot(raw: &str) -> rusqlite::Result<Value> {
    serde_json::from_str(raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn shape(db: &ScopedConnection, row: &Row<'_>) -> rusqlite::Result<Value> {
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
    item["assignee_member_id"] = json!(row.get::<_, Option<String>>(10)?);
    crate::projections::names(db, &mut item)?;
    Ok(item)
}

pub(crate) const OWNER_BLOCKED: &str = "status IN ('next','now','blocked') AND EXISTS (
    SELECT 1 FROM json_each(data, '$.blocked_on') b WHERE b.value ->> 'kind' = 'owner')";

const CLEAR: &str = "NOT EXISTS (
    SELECT 1 FROM json_each(data, '$.blocked_on') b
    WHERE NOT (
        (b.value ->> 'kind' = 'item' AND EXISTS (
            SELECT 1 FROM backlog_items dep WHERE +org_id = :org_id AND dep.item_id = b.value ->> 'ref'
              AND dep.project_id = backlog_items.project_id AND dep.status IN ('done','dropped')))
        OR
        (b.value ->> 'kind' = 'job' AND EXISTS (
            SELECT 1 FROM (SELECT * FROM jobs WHERE +org_id = :org_id) j JOIN (SELECT * FROM intents WHERE +org_id = :org_id) i ON i.intent_id = j.intent_id
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

pub(crate) fn check_project(
    db: &ScopedConnection,
    client: &Client,
    project: &str,
) -> Result<(), No> {
    if !crate::decision_log::project_visible(db, client, project).map_err(|e| No::internal(&e))? {
        return Err(missing());
    }
    work::project_exists(db, Some(project)).map_err(|e| if e.0 == 422 { missing() } else { e })
}

fn item(db: &ScopedConnection, project: &str, id: &str) -> Result<Value, No> {
    records::one(db, KIND, id)
        .map_err(|e| No::internal(&e))?
        .filter(|item| item["project_id"] == project)
        .ok_or_else(missing)
}

fn read(
    db: &ScopedConnection,
    project: &str,
    rest: &[&str],
    status: Option<&str>,
) -> Result<Value, No> {
    let result = match rest {
        ["next_unblocked"] => db.query_row(
            &format!("{SELECT} WHERE project_id = ?1 AND status = 'next' AND {CLEAR} ORDER BY {ORDER} LIMIT 1"),
            [project], |row| shape(db, row),
        ).optional().map(|item| json!({"item": item})),
        [] | ["needs_you"] => {
            if status.is_some_and(|s| !STATUSES.contains(&s)) {
                return Err(No::invalid("unknown backlog status"));
            }
            let filter = if rest.is_empty() { "1" } else { OWNER_BLOCKED };
            let mut stmt = db.prepare(&format!(
                "{SELECT} WHERE project_id = ?1 AND (?2 IS NULL OR status = ?2) AND {filter} ORDER BY {ORDER}"
            )).map_err(|e| No::internal(&e))?;
            stmt.query_map(params![project, status], |row| shape(db, row))
                .and_then(Iterator::collect::<Result<Vec<_>, _>>)
                .map(|mut items| {
                    if !rest.is_empty() {
                        items.retain(|item| crate::policy::attention_visible(
                            item["assignee_member_id"].as_str(), None,
                        ));
                        for item in &mut items {
                            item["actions"] = crate::projections::backlog_actions(db, &item["project_id"], &item["item_id"]);
                        }
                    }
                    json!({"items": items})
                })
        }
        [id] => return Ok(json!({"item": item(db, project, id)?, "revisions": history(db, id).map_err(|e| No::internal(&e))?})),
        _ => return Err(missing()),
    };
    result.map_err(|e| No::internal(&e))
}

fn history(db: &ScopedConnection, id: &str) -> rusqlite::Result<Vec<Value>> {
    db.prepare("SELECT snapshot FROM backlog_revisions WHERE +org_id = :org_id AND item_id = ?1 ORDER BY revision")?
        .query_map([id], |row| snapshot(&row.get::<_, String>(0)?))?
        .collect()
}

pub(crate) fn attention(db: &ScopedConnection, conversation: &str) -> rusqlite::Result<Value> {
    db.query_row(&format!(
        "{SELECT} WHERE project_id = (SELECT project_id FROM conversations WHERE +org_id = :org_id AND conversation_id = ?1)
          AND {OWNER_BLOCKED} ORDER BY {ORDER} LIMIT 1"
    ), [conversation], |row| shape(db, row)).optional().map(|item| item.filter(|item| {
        crate::policy::attention_visible(item["assignee_member_id"].as_str(), item["updated_by"].as_str())
    }).map_or(Value::Null, |item| {
        let mut attention = json!({"source_kind": "backlog_item", "backlog_item_id": item["item_id"],
               "project_id": item["project_id"], "revision": item["revision"],
               "assignee_member_id": item["assignee_member_id"],
               "question": item["blocked_reason"].as_str().filter(|s| !s.is_empty())
                   .or_else(|| item["title"].as_str()),
               "set_at": item["updated_at"], "set_by": item["updated_by"], "job_id": null});
        for (key, value) in choices(&item).as_object().into_iter().flatten() {
            attention[key] = value.clone();
        }
        attention
    }))
}

fn choices(item: &Value) -> Value {
    let mut result = item["needs_you"].as_object().cloned().map_or_else(
        || json!({"options":[{"id":"unblock","label":"Unblock"}],"default":"unblock"}),
        Value::Object,
    );
    if !result["links"].is_object() {
        result["links"] = json!({});
    }
    result["links"]["backlog"] = item["item_id"].clone();
    result
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
    if let Some(question) = data.get("needs_you").filter(|value| !value.is_null()) {
        validate_choices(question)?;
    }
    if let Some(answer) = old.and_then(|item| item.get("needs_you_answer")) {
        data["needs_you_answer"] = answer.clone();
    }
    Ok(data)
}

fn validate_choices(question: &Value) -> Result<(), No> {
    let mut ids = HashSet::new();
    if !question.as_object().is_some_and(|fields| {
        fields
            .keys()
            .all(|key| matches!(key.as_str(), "options" | "default" | "links"))
    }) || !question["options"].as_array().is_some_and(|options| {
        (1..=6).contains(&options.len())
            && options.iter().all(|option| {
                option.as_object().is_some_and(|fields| fields.len() == 2)
                    && nonempty(&option["id"])
                    && nonempty(&option["label"])
                    && option["id"]
                        .as_str()
                        .is_some_and(|id| id.len() <= 64 && ids.insert(id))
                    && option["label"]
                        .as_str()
                        .is_some_and(|label| label.len() <= 160)
            })
    }) {
        return Err(No::invalid(
            "needs_you requires one to six uniquely identified labelled options",
        ));
    }
    if !question["default"].is_null()
        && !question["default"]
            .as_str()
            .is_some_and(|id| ids.contains(id))
    {
        return Err(No::invalid("needs_you default must identify an option"));
    }
    if !question["links"].is_null()
        && !question["links"].as_object().is_some_and(|links| {
            links.iter().all(|(key, value)| {
                matches!(
                    key.as_str(),
                    "job" | "run" | "lane" | "approval" | "backlog"
                ) && nonempty(value)
            })
        })
    {
        return Err(No::invalid(
            "needs_you links must map resource kinds to IDs",
        ));
    }
    Ok(())
}

fn changed(
    db: &ScopedConnection,
    project: &str,
    id: &str,
    conversation_changed: bool,
) -> Result<(), No> {
    touch(db, KIND, id).map_err(|e| No::internal(&e))?;
    if conversation_changed {
        let mut stmt = db
            .prepare("SELECT conversation_id FROM conversations WHERE +org_id = :org_id AND lead = 1 AND project_id = ?1")
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
    db: &ScopedConnection,
    client: &Client,
    project: &str,
    id: Option<&str>,
    input: &Map<String, Value>,
) -> Result<String, No> {
    let old = id.map(|id| item(db, project, id)).transpose()?;
    if let Some(old) = &old {
        crate::policy::check_backlog_update(client, old, input)?;
    }
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
            "INSERT INTO backlog_revisions (org_id, item_id, revision, snapshot) VALUES (:org_id, ?1, ?2, ?3)",
            params![id, old["revision"].as_i64(), old.to_string()],
        )
        .map_err(|e| No::internal(&e))?;
        db.execute("UPDATE backlog_items SET status = ?2, priority = ?3, data = ?4, updated_at = ?5, updated_by = ?6
                    WHERE +org_id = :org_id AND (item_id = ?1)",
                   params![id, data["status"].as_str(), data["priority"].as_i64(), data.to_string(), now, client.id])
            .map_err(|e| No::internal(&e))?;
        id.to_owned()
    } else {
        let id = format!(
            "bli_{}",
            ulid().map_err(|e| No::internal(&e))?.to_lowercase()
        );
        db.execute("INSERT INTO backlog_items (org_id, item_id, project_id, status, priority, data, created_at, created_by, updated_at, updated_by)
                    VALUES (:org_id, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?7)",
                   params![id, project, data["status"].as_str(), data["priority"].as_i64(), data.to_string(), now, client.id])
            .map_err(|e| No::internal(&e))?;
        id
    };
    let assignee: Option<&str> = if owner_blocked(&data) {
        old.as_ref()
            .and_then(|value| value["assignee_member_id"].as_str())
            .unwrap_or(&db.scope().member_id)
            .into()
    } else {
        None
    };
    db.execute(
        "UPDATE backlog_items SET assignee_member_id=?2 WHERE +org_id=:org_id AND item_id=?1",
        params![id, assignee],
    )
    .map_err(|e| No::internal(&e))?;
    changed(
        db,
        project,
        &id,
        owner_blocked(&data) || old.as_ref().is_some_and(owner_blocked),
    )?;
    Ok(id)
}

pub(crate) fn owner_blocker_assignee(
    db: &ScopedConnection,
    project: &str,
    id: &str,
) -> rusqlite::Result<Option<String>> {
    db.query_row(
        "SELECT COALESCE(assignee_member_id,'') FROM backlog_items
         WHERE +org_id=:org_id AND item_id=?1 AND project_id=?2",
        params![id, project],
        |row| row.get(0),
    )
    .optional()
}

fn answer(
    db: &ScopedConnection,
    client: &Client,
    project: &str,
    id: &str,
    input: &Map<String, Value>,
) -> Result<String, No> {
    let old = item(db, project, id)?;
    if !owner_blocked(&old) {
        return Err(No(
            409,
            "conflict",
            "the item has no open owner blocker".into(),
            Value::Null,
        ));
    }
    if !crate::policy::can_resolve_owner_blocker(client, old["assignee_member_id"].as_str()) {
        return Err(No(
            403,
            "forbidden",
            "only the assigned member or an owner can answer".into(),
            Value::Null,
        ));
    }
    let options = choices(&old);
    let choice = input
        .get("choice")
        .and_then(Value::as_str)
        .and_then(|id| {
            options["options"]
                .as_array()?
                .iter()
                .find(|option| option["id"] == id)
        })
        .ok_or_else(|| No::invalid("choice must identify an available option"))?;
    let blockers: Vec<_> = old["blocked_on"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|blocker| blocker["kind"] != "owner")
        .collect();
    let mut update = Map::from_iter([
        (
            "revision".into(),
            input.get("revision").cloned().unwrap_or_default(),
        ),
        ("blocked_on".into(), json!(blockers)),
    ]);
    if blockers.is_empty() {
        update.insert("blocked_reason".into(), json!(""));
        if old["status"] == "blocked" {
            update.insert("status".into(), json!("next"));
        }
    }
    save(db, client, project, Some(id), &update)?;
    let answer = json!({"choice":choice["id"], "label":choice["label"],
        "member_id":client.member_id(), "answered_at":now_utc()});
    db.execute(
        "UPDATE backlog_items SET data = json_set(data, '$.needs_you_answer', json(?2))
         WHERE +org_id = :org_id AND item_id = ?1",
        params![id, answer.to_string()],
    )
    .map_err(|e| No::internal(&e))?;
    Ok(id.into())
}

fn reorder(
    db: &ScopedConnection,
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
            [id, "answer"] => answer(&tx, client, project, id, &input)?,
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

pub(crate) fn markdown(state: &State, client: &Client, project: &str) -> Reply {
    let db = state.store.db();
    let result = (|| -> Result<String, No> {
        check_project(&db, client, project)?;
        let backlog = read(&db, project, &[], None)?;
        let items = backlog["items"].as_array().into_iter().flatten();
        let mut out = String::from("# Project backlog\n");
        for (heading, status) in [
            ("Now", "now"),
            ("Next", "next"),
            ("Blocked", "blocked"),
            ("Needs you", ""),
        ] {
            let _ = writeln!(out, "\n## {heading}\n");
            for item in items.clone().filter(|item| {
                if status.is_empty() {
                    owner_blocked(item)
                } else {
                    item["status"] == status
                }
            }) {
                let title = item["title"]
                    .as_str()
                    .unwrap_or_default()
                    .replace(['\r', '\n'], " ");
                let reason = item["blocked_reason"]
                    .as_str()
                    .unwrap_or_default()
                    .replace(['\r', '\n'], " ");
                let id = item["item_id"].as_str().unwrap_or_default();
                let _ = write!(out, "- [{title}](backlog/{id})");
                if !reason.is_empty() {
                    let _ = write!(out, " — {reason}");
                }
                out.push('\n');
            }
        }
        Ok(out)
    })();
    match result {
        Ok(out) => crate::server::bytes(200, "text/markdown; charset=utf-8", out.into()),
        Err(error) => error.reply(),
    }
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
        ("POST", [] | ["reorder"] | [_, "update" | "answer"]) => {
            write(state, call, client, project, rest)
        }
        _ => Err(missing()),
    };
    result.unwrap_or_else(No::reply)
}
