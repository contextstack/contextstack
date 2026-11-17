//! Director-owned chief-of-staff slot, stored atomically with its conversation in meta.
use crate::access::{Client, Kind};
use crate::server::{Call, Reply, State, internal, json};
use crate::store::{Record, touch};
use crate::util::{now_utc, ulid};
use crate::work::{No, body, content, key, remember, scoped_prior};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub fn read(db: &Connection) -> rusqlite::Result<Value> {
    let raw: Option<String> = db.query_row("SELECT value FROM meta WHERE key = 'chief_of_staff'", [], |r| r.get(0)).optional()?;
    Ok(raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({"enabled": false, "name": "Chief of staff", "conversation_id": null, "revision": 0})))
}

fn save(db: &Connection, slot: &mut Value) -> rusqlite::Result<()> {
    db.execute("INSERT INTO changes (kind, record_id, at) VALUES ('chief_of_staff', 'chief_of_staff', ?1)", [now_utc()])?;
    slot["revision"] = json!(db.last_insert_rowid());
    db.execute("INSERT INTO meta (key, value) VALUES ('chief_of_staff', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [slot.to_string()])?;
    Ok(())
}

pub fn get(state: &State) -> Reply {
    match read(&state.store.db()) {
        Ok(slot) => json(200, &json!({"chief_of_staff": slot})),
        Err(e) => internal(&e),
    }
}

pub fn protect(db: &Connection, id: &str) -> Result<(), No> {
    let slot = read(db).map_err(|e| No::internal(&e))?;
    if slot["enabled"] == true && slot["conversation_id"].as_str() == Some(id) {
        return Err(No(409, "chief_of_staff", "turn the chief of staff off first".into(), Value::Null));
    }
    Ok(())
}

fn title(value: &Value) -> Result<&str, No> {
    value.as_str().map(str::trim).filter(|s| (1..=200).contains(&s.chars().count()))
        .ok_or_else(|| No::invalid("title or name must contain 1 to 200 characters, trimmed"))
}

fn owner(client: &Client) -> Result<(), No> {
    if client.kind != Kind::Local {
        return Err(No(403, "owner_only", "only the owner may do this".into(), Value::Null));
    }
    Ok(())
}

fn create(state: &State, db: &Connection, client: &Client, name: &str) -> Result<(String, bool), No> {
    let node = crate::lead_node::effective(db).map_err(|e| No::internal(&e))?
        .ok_or_else(|| No::invalid("register a node before enabling the chief of staff"))?;
    let status: Option<String> = db.query_row("SELECT status FROM nodes WHERE name = ?1 AND revoked_at IS NULL", [&node], |r| r.get(0))
        .optional().map_err(|e| No::internal(&e))?.ok_or_else(|| No::invalid("lead node is not live"))?;
    let status: Value = status.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let limited = |model: &&str| status["provider_limits"].as_array().is_some_and(|rows| rows.iter().any(|r| {
        r["template"].as_str() == Some(*model) && r["resets_at"].as_str().is_none_or(|at| at > now_utc().as_str())
    }));
    let available = crate::thread::LEAD_MODELS.iter().find(|m| !limited(m));
    let model = available.copied().unwrap_or(crate::thread::LEAD_MODELS[0]);
    let mut m = serde_json::Map::from_iter([("model".into(), json!(model))]);
    crate::conversations::lead_bundle(state, &mut m)?;
    let files = crate::jobs::bundle_of_objects(&state.store, &m["files"]).map_err(|_| No::invalid("hosted lead bundle is unavailable"))?;
    let (facts, instructions) = crate::conversations::check_bundle(state, "spec.yaml", &files)?;
    let id = format!("con_{}", ulid().map_err(|e| No::internal(&e))?.to_lowercase());
    db.execute("INSERT INTO conversations (conversation_id, title, node, spec, spec_version, model, instructions_tokens, tools, created_by, created_at, lead) VALUES (?1, ?2, ?3, 'spec.yaml', ?4, ?5, ?6, ?7, ?8, ?9, 1)", params![id, name, node, crate::jobs::bundle_sha(&files), facts.model, instructions, facts.tools, client.id, now_utc()]).map_err(|e| No::internal(&e))?;
    for f in files {
        db.execute("INSERT INTO conversation_files (conversation_id, path, sha256, bytes, mode) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, f.path, f.sha256, i64::try_from(f.bytes).unwrap_or(i64::MAX), f.mode]).map_err(|e| No::internal(&e))?;
    }
    touch(db, Record::Conversation, &id).map_err(|e| No::internal(&e))?;
    Ok((id, available.is_none()))
}

fn put_answer(db: &Connection, slot: &Value) -> Result<Value, No> {
    let mut answer = json!({"chief_of_staff": slot});
    let note: Option<String> = db.query_row(
        "SELECT value FROM meta WHERE key = 'chief_of_staff_model_note'", [], |r| r.get(0),
    ).optional().map_err(|e| No::internal(&e))?;
    if answer["chief_of_staff"]["enabled"] == true && let Some(note) = note {
        answer["model_note"] = json!(note);
    }
    Ok(answer)
}

pub fn put(state: &State, call: &Call, client: &Client) -> Reply {
    let result = (|| -> Result<Value, No> {
        owner(client)?;
        let key = key(call)?;
        let m = body(call)?;
        if m.get("enabled").is_some_and(|v| !v.is_boolean()) { return Err(No::invalid("enabled must be a boolean")); }
        let name = m.get("name").map(title).transpose()?;
        // Adopt an existing hosted-lead conversation as the chief of staff instead of creating one.
        let adopt = match m.get("conversation_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if !id.is_empty() && id.len() <= 200 => Some(id.clone()),
            Some(_) => return Err(No::invalid("conversation_id must be a conversation id")),
        };
        let want = json!({"enabled": m.get("enabled"), "name": name, "conversation_id": adopt});
        let sha = content("chief_of_staff", "", &want);
        let mut db = state.store.db();
        let tx = crate::store::write_tx(&mut db).map_err(|e| No::internal(&e))?;
        if scoped_prior(&tx, &key, &sha, client)?.is_some() {
            return put_answer(&tx, &read(&tx).map_err(|e| No::internal(&e))?);
        }
        let mut slot = read(&tx).map_err(|e| No::internal(&e))?;
        let old = slot.clone();
        if let Some(name) = name { slot["name"] = json!(name); }
        if let Some(enabled) = m.get("enabled") { slot["enabled"] = enabled.clone(); }
        if let Some(id) = &adopt {
            let lead: Option<i64> = tx
                .query_row("SELECT lead FROM conversations WHERE conversation_id = ?1", [id], |r| r.get(0))
                .optional()
                .map_err(|e| No::internal(&e))?;
            if lead != Some(1) {
                return Err(No::invalid("conversation_id must name an existing hosted-lead conversation"));
            }
            slot["conversation_id"] = json!(id);
        }
        let id = slot["conversation_id"].as_str().map(str::to_string);
        let exists = if let Some(id) = &id { tx.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE conversation_id = ?1)", [id], |r| r.get::<_, bool>(0)).map_err(|e| No::internal(&e))? } else { false };
        if slot["enabled"] == true && !exists {
            let (id, limited) = create(state, &tx, client, slot["name"].as_str().unwrap_or_default())?;
            slot["conversation_id"] = json!(id);
            tx.execute("DELETE FROM meta WHERE key = 'chief_of_staff_model_note'", [])
                .map_err(|e| No::internal(&e))?;
            if limited {
                tx.execute("INSERT INTO meta (key, value) VALUES ('chief_of_staff_model_note', ?1)",
                    ["all lead models were usage-limited at creation; selected the first lead model"])
                    .map_err(|e| No::internal(&e))?;
            }
        } else if exists {
            let id = id.as_deref().unwrap_or_default();
            let at = (slot["enabled"] != true).then(now_utc);
            let changed = tx.execute("UPDATE conversations SET title = ?2, archived_at = ?3 WHERE conversation_id = ?1 AND (title IS NOT ?2 OR (archived_at IS NULL) != (?3 IS NULL))", params![id, slot["name"].as_str(), at]).map_err(|e| No::internal(&e))?;
            if changed > 0 { touch(&tx, Record::Conversation, id).map_err(|e| No::internal(&e))?; }
        }
        if slot != old { save(&tx, &mut slot).map_err(|e| No::internal(&e))?; }
        remember(&tx, &key, &sha, Record::Conversation, "chief_of_staff", client).map_err(|e| No::internal(&e))?;
        let answer = put_answer(&tx, &slot)?;
        tx.commit().map_err(|e| No::internal(&e))?;
        Ok(answer)
    })();
    match result { Ok(v) => json(200, &v), Err(e) => e.reply() }
}

pub fn rename(state: &State, call: &Call, client: &Client, id: &str) -> Reply {
    let result = (|| -> Result<Value, No> {
        owner(client)?;
        let key = key(call)?;
        let m = body(call)?;
        let name = title(&m.get("title").cloned().unwrap_or_default())?.to_string();
        let sha = content("conversation_rename", id, &json!({"title": name}));
        let mut db = state.store.db();
        let tx = crate::store::write_tx(&mut db).map_err(|e| No::internal(&e))?;
        if scoped_prior(&tx, &key, &sha, client)?.is_none() {
            if tx.execute("UPDATE conversations SET title = ?2 WHERE conversation_id = ?1", params![id, name]).map_err(|e| No::internal(&e))? == 0 {
                return Err(No(404, "not_found", "no such conversation".into(), Value::Null));
            }
            touch(&tx, Record::Conversation, id).map_err(|e| No::internal(&e))?;
            let mut slot = read(&tx).map_err(|e| No::internal(&e))?;
            if slot["conversation_id"].as_str() == Some(id) && slot["name"] != name {
                slot["name"] = json!(name);
                save(&tx, &mut slot).map_err(|e| No::internal(&e))?;
            }
            remember(&tx, &key, &sha, Record::Conversation, id, client).map_err(|e| No::internal(&e))?;
        }
        let conversation = crate::records::one(&tx, Record::Conversation, id).map_err(|e| No::internal(&e))?;
        tx.commit().map_err(|e| No::internal(&e))?;
        Ok(json!({"conversation": conversation}))
    })();
    match result { Ok(v) => json(200, &v), Err(e) => e.reply() }
}
