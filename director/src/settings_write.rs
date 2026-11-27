//! Idempotent receipts for organization and member settings, using existing member-scoped writes.
use crate::access::Client;
use crate::store::ScopedConnection;
use crate::util::now_utc;
use crate::work::No;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

pub fn prior(
    db: &ScopedConnection,
    key: &str,
    hash: &str,
    client: &Client,
    subject: &str,
) -> Result<Option<Value>, No> {
    let prior: Option<Option<String>> = db.query_row(
        "SELECT m.response FROM (SELECT key, request_sha256 FROM writes WHERE +org_id = :org_id AND key=?1) w
         LEFT JOIN (SELECT key, response FROM member_writes WHERE +org_id = :org_id
                    AND key=?1 AND member_id=?2 AND actor_member_id=?3) m
             ON m.key=w.key AND w.request_sha256=?4",
        params![key, subject, client.member_id(), hash], |row| row.get(0),
    ).optional().map_err(|e| No::internal(&e))?;
    match prior {
        None => Ok(None),
        Some(Some(response)) => serde_json::from_str(&response)
            .map(Some)
            .map_err(|e| No::internal(&e)),
        Some(None) => Err(No(
            409,
            "idempotency_conflict",
            "this Idempotency-Key is unavailable".into(),
            Value::Null,
        )),
    }
}

pub fn save(
    db: &ScopedConnection,
    key: &str,
    hash: &str,
    client: &Client,
    subject: &str,
    response: &Value,
    kind: &str,
) -> Result<(), No> {
    db.execute(
        "INSERT INTO writes (org_id,key,request_sha256,record_kind,record_id,credential_id,created_at)
         VALUES (:org_id,?1,?2,?5,'',?3,?4)",
        params![key,hash,client.id,now_utc(),kind],
    ).map_err(|e| No::internal(&e))?;
    db.execute(
        "INSERT INTO member_writes (org_id,key,member_id,actor_member_id,response) VALUES (:org_id,?1,?2,?3,?4)",
        params![key, subject, client.member_id(), response.to_string()],
    ).map_err(|e| No::internal(&e))?;
    Ok(())
}
