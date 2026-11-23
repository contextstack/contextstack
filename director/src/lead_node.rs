//! Hosted-lead home node: optional `meta.lead_node`, else `meta.local_node` (first node registered on this Director).

use crate::access;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::util::now_utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const KEY_LEAD: &str = "lead_node";
const KEY_LOCAL: &str = "local_node";

/// Remember the first node registered on this Director as its local node (one home machine for leads).
pub fn note_local_node(db: &Connection, name: &str) -> rusqlite::Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
        params![KEY_LOCAL, name],
    )?;
    Ok(())
}

fn meta_value(db: &Connection, key: &str) -> Result<Option<String>, String> {
    db.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        r.get(0)
    })
    .optional()
    .map_err(|e| e.to_string())
}

/// The node hosted leads use: explicit setting, else the local node.
pub fn effective(db: &Connection) -> Result<Option<String>, String> {
    if let Some(name) = meta_value(db, KEY_LEAD)? {
        return Ok(Some(name));
    }
    if let Some(name) = meta_value(db, KEY_LOCAL)? {
        return Ok(Some(name));
    }
    // Installs registered before `local_node` was recorded: the first live node is the Director's own.
    db.query_row(
        "SELECT name FROM nodes WHERE revoked_at IS NULL ORDER BY id LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn node_live(db: &Connection, name: &str) -> Result<bool, String> {
    db.query_row(
        "SELECT 1 FROM nodes WHERE name = ?1 AND revoked_at IS NULL",
        params![name],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
    .map_err(|e| e.to_string())
}

/// `GET lead-node`: `{node, source}` where `source` is `default` or `set`.
pub fn get_answer(db: &Connection) -> Result<Value, String> {
    let Some(node) = effective(db)? else {
        return Err("no lead home node is configured; register a node on this Director".into());
    };
    let source = if meta_value(db, KEY_LEAD)?.is_some() {
        "set"
    } else {
        "default"
    };
    Ok(json!({"node": node, "source": source}))
}

/// `PUT lead-node {"node"}` (owner only): persist the home machine for hosted leads.
pub fn put(state: &State, call: &Call) -> Reply {
    let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
    let Some(name) = body["node"].as_str().filter(|s| !s.is_empty()) else {
        return refusal(422, "invalid_request", "node is required", Value::Null);
    };
    let db = state.store.db();
    match node_live(&db, name) {
        Ok(true) => {}
        Ok(false) => {
            return refusal(
                422,
                "unknown_node",
                &format!("no live node named {name}"),
                Value::Null,
            );
        }
        Err(e) => return internal(&e),
    }
    if let Err(e) = db.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![KEY_LEAD, name],
    ) {
        return internal(&e.to_string());
    }
    match get_answer(&db) {
        Ok(v) => json(200, &v),
        Err(e) => internal(&e),
    }
}

/// Enforce hosted-lead placement on the lead home node; inject `node` when omitted.
pub fn apply_create_node(
    db: &Connection,
    m: &mut serde_json::Map<String, Value>,
    built_lead: bool,
) -> Result<(), crate::work::No> {
    if !built_lead {
        return Ok(());
    }
    let home = effective(db).map_err(|e| crate::work::No::internal(&e))?;
    let Some(home) = home else {
        return Err(crate::work::No::invalid(
            "register a node on this Director before creating a hosted lead",
        ));
    };
    match m.get("node") {
        None | Some(Value::Null) => {
            m.insert("node".into(), json!(home));
        }
        Some(Value::String(given)) if given == &home => {}
        Some(Value::String(given)) => {
            return Err(crate::work::No(
                422,
                "lead_node_mismatch",
                format!("hosted leads run on {home}; this request named {given}"),
                json!({"lead_node": home}),
            ));
        }
        Some(_) => return Err(crate::work::No::invalid("node must be a string")),
    }
    Ok(())
}

/// `POST conversations/{id}/lead-credential` for the conversation's home node (protocol 2).
pub fn issue_credential(
    state: &State,
    node_id: i64,
    node_name: &str,
    conversation_id: &str,
) -> Reply {
    let row: Option<(i64, String)> = {
        let db = state.store.db();
        db.query_row(
            "SELECT lead, node FROM conversations WHERE conversation_id = ?1",
            params![conversation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap_or(None)
    };
    let Some((lead, conv_node)) = row else {
        return refusal(404, "not_found", "no such conversation", Value::Null);
    };
    if lead == 0 {
        return refusal(
            409,
            "not_a_lead",
            "only a hosted-lead conversation may fetch a lead credential",
            Value::Null,
        );
    }
    if conv_node != node_name {
        return refusal(
            403,
            "not_this_node",
            &format!("conversation {conversation_id} runs on {conv_node}, not {node_name}"),
            Value::Null,
        );
    }
    let live = {
        let db = state.store.db();
        db.query_row(
            "SELECT 1 FROM nodes WHERE id = ?1 AND name = ?2 AND revoked_at IS NULL",
            params![node_id, node_name],
            |_| Ok(()),
        )
        .optional()
        .is_ok_and(|o| o.is_some())
    };
    if !live {
        return refusal(
            403,
            "not_this_node",
            &format!("conversation {conversation_id} is not on this node"),
            Value::Null,
        );
    }
    // Mint first, then revoke the older ones: a failed mint leaves the machine's current credential working. If this
    // answer is lost, the machine's next call with its old credential gets 401 and it fetches again.
    let actor = format!("node:{node_id}");
    let minted = match access::mint_lead(&state.store, "hosted-lead", conversation_id, None, &actor)
    {
        Ok(v) => v,
        Err(e) => return refusal(422, "invalid_request", &e, Value::Null),
    };
    let Some(new_id) = minted["credential_id"].as_str().map(str::to_string) else {
        return internal(&"minted lead credential has no id".to_string());
    };
    let revoked = state
        .store
        .db()
        .prepare(
            "UPDATE credentials SET revoked_at = ?1, revoked_by = ?4
         WHERE kind = 'lead' AND conversation_id = ?2 AND revoked_at IS NULL AND id != ?3
         RETURNING id",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![now_utc(), conversation_id, new_id, actor], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        });
    match revoked {
        Ok(ids) => {
            for id in ids {
                state.hub.revoked(&id);
            }
        }
        Err(e) => return internal(&e),
    }
    json(201, &minted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    type Res = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn falls_back_to_the_first_live_node_without_local_node() -> Result<(), String> {
        let db = Connection::open_in_memory().map_err(|e| e.to_string())?;
        db.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE nodes (id INTEGER PRIMARY KEY, name TEXT NOT NULL, revoked_at TEXT);
             INSERT INTO nodes (id, name) VALUES (1, 'node-b'), (2, 'node-a');",
        )
        .map_err(|e| e.to_string())?;
        assert_eq!(effective(&db)?, Some("node-b".to_string()));
        Ok(())
    }

    #[test]
    fn default_lead_node_is_first_registered() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        let store = Store::open(&dir)?;
        crate::access::ensure_local_credential(&store)?;
        crate::access::register_node(&store, "estate")?;
        crate::access::register_node(&store, "node-a")?;
        let db = store.db();
        let answer = get_answer(&db)?;
        assert_eq!(answer["node"], "estate");
        assert_eq!(answer["source"], "default");
        Ok(())
    }
}
