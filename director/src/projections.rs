use crate::policy::projected_action;
use crate::store::{Record, ScopedConnection};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

pub fn names(db: &ScopedConnection, row: &mut Value) -> rusqlite::Result<()> {
    for (id, name) in [
        ("sponsor_member_id", "sponsor_display_name"),
        ("responsible_member_id", "responsible_display_name"),
        ("assignee_member_id", "assignee_display_name"),
    ] {
        if let Some(member) = row.get(id) {
            let display: Option<String> = db
                .query_row(
                    "SELECT display_name FROM members WHERE +org_id=:org_id AND id=?1",
                    [member.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            row[name] = json!(display);
        }
    }
    Ok(())
}

pub fn approval_actions(db: &ScopedConnection, row: &mut Value, open: bool) {
    let id = row["action_approval_id"]
        .as_str()
        .or_else(|| row["id"].as_str())
        .unwrap_or_default();
    let decide = open && projected_action(db, "POST", &["action-approvals", id, "decide"], false);
    let acknowledge = row["options"][0]["id"] == "acknowledge"
        || (row["outcome"]["status"] == "rollback_failed"
            && row["outcome"]["waiting_jobs"].is_number()
            && row["outcome"]["acknowledged"] != true);
    let answer = (open || acknowledge)
        && projected_action(db, "POST", &["action-approvals", id, "answer"], false);
    let clear = projected_action(db, "POST", &["action-approvals", id, "clear"], false);
    row["actions"] = json!({"decide":decide, "answer":answer, "clear":clear});
}

pub fn record(db: &ScopedConnection, kind: Record, row: &mut Value) -> rusqlite::Result<()> {
    names(db, row)?;
    let allowed = |method, parts: &[&str]| projected_action(db, method, parts, false);
    let id = |field| row[field].as_str().unwrap_or_default();
    let actions = match kind {
        Record::Intent => {
            json!({"control": allowed("PUT", &["intents", id("intent_id"), "owner-messages", "_", "mark"])})
        }
        Record::Job => json!({"cancel": row["cancel"].is_null()
            && !matches!(row["state"].as_str(), Some("done" | "error"))
            && allowed("POST", &["jobs", id("job_id"), "cancel"])}),
        Record::Run => json!({"decide": row["decision"].is_null()
            && allowed("POST", &["runs", id("run_id"), "decision"])}),
        Record::Conversation => {
            let conversation = id("conversation_id").to_owned();
            let actions = json!({
                "control": allowed("PATCH", &["conversations", &conversation]),
                "archive": allowed("POST", &["conversations", &conversation, "archive"]),
                "adopt": projected_action(db, "PATCH", &["conversations", &conversation], true),
            });
            if !row["attention"].is_null() {
                attention(db, &conversation, &mut row["attention"])?;
            }
            row["paused_reason"] = paused(crate::sponsorship::can_dispatch(db, &conversation)?);
            actions
        }
        Record::Schedule => {
            row["paused_reason"] = paused(crate::sponsorship::schedule_can_fire(
                db,
                id("schedule_key"),
            )?);
            return Ok(());
        }
        _ => return Ok(()),
    };
    row["actions"] = actions;
    Ok(())
}

fn attention(db: &ScopedConnection, conversation: &str, item: &mut Value) -> rusqlite::Result<()> {
    names(db, item)?;
    if item["action_approval_id"].is_string() {
        let open = item["options"][0]["id"] != "acknowledge";
        approval_actions(db, item, open);
    } else if item["source_kind"] == "backlog_item" {
        item["actions"] = backlog_actions(db, &item["project_id"], &item["backlog_item_id"]);
    } else {
        item["actions"] = json!({"control":projected_action(db, "DELETE", &["conversations", conversation, "attention"], false)});
    }
    Ok(())
}

pub fn backlog_actions(db: &ScopedConnection, project: &Value, item: &Value) -> Value {
    let answer = project
        .as_str()
        .zip(item.as_str())
        .is_some_and(|(project, item)| {
            projected_action(
                db,
                "POST",
                &["projects", project, "backlog", item, "answer"],
                false,
            )
        });
    json!({"answer":answer})
}

fn paused(eligible: bool) -> Value {
    json!((!eligible).then_some("sponsor_ineligible"))
}
