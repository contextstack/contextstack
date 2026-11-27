//! Hosted-lead home node: optional `org_settings.lead_node`, else `org_settings.local_node` (first node registered on this Director).

use crate::access;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::ScopedConnection;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

const KEY_LEAD: &str = "lead_node";
const KEY_LOCAL: &str = "local_node";

/// Remember the first node registered on this Director as its local node (one home machine for leads).
pub fn note_local_node(db: &ScopedConnection, name: &str) -> rusqlite::Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO org_settings (org_id, key, value) VALUES (:org_id, ?1, ?2)",
        params![KEY_LOCAL, name],
    )?;
    Ok(())
}

fn setting(db: &ScopedConnection, key: &str) -> Result<Option<String>, String> {
    db.query_row(
        "SELECT value FROM org_settings WHERE +org_id = :org_id AND key = ?1",
        params![key],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())
}

/// The node hosted leads use: explicit setting, else the local node.
pub fn effective(db: &ScopedConnection) -> Result<Option<String>, String> {
    if let Some(name) = setting(db, KEY_LEAD)? {
        return Ok(Some(name));
    }
    if let Some(name) = setting(db, KEY_LOCAL)? {
        return Ok(Some(name));
    }
    // Installs registered before `local_node` was recorded: the first live node is the Director's own.
    db.query_row(
        "SELECT name FROM nodes WHERE +org_id = :org_id AND revoked_at IS NULL ORDER BY id LIMIT 1",
        params![],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn node_live(db: &ScopedConnection, name: &str) -> Result<bool, String> {
    db.query_row(
        "SELECT 1 FROM nodes WHERE +org_id = :org_id AND name = ?1 AND revoked_at IS NULL",
        params![name],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
    .map_err(|e| e.to_string())
}

/// `GET lead-node`: `{node, source}` where `source` is `default` or `set`.
pub fn get_answer(db: &ScopedConnection) -> Result<Value, String> {
    let Some(node) = effective(db)? else {
        return Err("no lead home node is configured; register a node on this Director".into());
    };
    let source = if setting(db, KEY_LEAD)?.is_some() {
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
    let written = (|| -> rusqlite::Result<()> {
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO org_settings (org_id, key, value) VALUES (:org_id, ?1, ?2) ON CONFLICT(org_id, key) DO UPDATE SET value = excluded.value WHERE +org_settings.org_id = :org_id",
            params![KEY_LEAD, name],
        )?;
        tx.commit()
    })();
    if let Err(e) = written {
        return internal(&e);
    }
    match get_answer(&db) {
        Ok(v) => json(200, &v),
        Err(e) => internal(&e),
    }
}

/// Enforce hosted-lead placement on the lead home node; inject `node` when omitted.
pub fn apply_create_node(
    db: &ScopedConnection,
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
    let mut db = state.store.db();
    let tx = match crate::store::write_tx(&mut db) {
        Ok(tx) => tx,
        Err(e) => return internal(&e),
    };
    let row = tx
        .query_row(
            "SELECT lead, node FROM conversations WHERE +org_id = :org_id AND conversation_id = ?1",
            params![conversation_id],
            |r| Ok((r.get::<_, bool>(0)?, r.get::<_, String>(1)?)),
        )
        .optional();
    let row = match row {
        Ok(row) => row,
        Err(e) => return internal(&e),
    };
    let Some((lead, conv_node)) = row else {
        return refusal(404, "not_found", "no such conversation", Value::Null);
    };
    if !lead {
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
    let live = match tx.prepare(
        "SELECT 1 FROM nodes WHERE +org_id = :org_id AND id = ?1 AND name = ?2 AND revoked_at IS NULL",
    ).and_then(|mut stmt| stmt.exists(params![node_id, node_name])) {
        Ok(live) => live,
        Err(e) => return internal(&e),
    };
    if !live {
        return refusal(
            403,
            "not_this_node",
            &format!("conversation {conversation_id} is not on this node"),
            Value::Null,
        );
    }
    match crate::sponsorship::can_dispatch(&tx, conversation_id) {
        Ok(true) => {}
        Ok(false) => return crate::policy::denied(),
        Err(e) => return internal(&e),
    }
    let actor = format!("node:{node_id}");
    let minted = match access::mint_lead_in(&tx, "hosted-lead", conversation_id, None, &actor) {
        Ok(v) => v,
        Err(e) => return refusal(422, "invalid_request", &e, Value::Null),
    };
    let revoked = crate::sponsorship::revoke_leads(
        &tx,
        conversation_id,
        minted["credential_id"].as_str(),
        &actor,
    );
    let ids = match revoked.and_then(|ids| tx.commit().map(|()| ids)) {
        Ok(ids) => ids,
        Err(e) => return internal(&e),
    };
    for id in ids {
        state.hub.revoked(&id);
    }
    json(201, &minted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    type Res = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn falls_back_to_the_first_live_node_without_local_node() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let db = store.db();
        db.fixture_batch(
            "INSERT INTO nodes (org_id, id, name, token_hash, created_at)
             VALUES (:org_id, 1, 'node-b', 'hash-b', 'now'),
                    (:org_id, 2, 'node-a', 'hash-a', 'now');",
        )?;
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
