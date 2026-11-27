use crate::action_approvals::{record, reply, touch_conversations};
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::{Record, ScopedConnection, touch, write_tx};
use crate::util::{now_utc, ulid};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

// Covers the one-hour build, both ten-minute drain windows, smoke and backup.
const CLAIM_SECONDS: i64 = 90 * 60;
const KIND: &str = "json_extract(request,'$.kind') IN ('install','rollback')";
const CLEARABLE: &str = "(status IN ('pending','needs_owner') OR (status='approved' AND
    (outcome IS NULL OR json_extract(outcome,'$.status') IN ('unknown','interrupted') OR
     (json_extract(outcome,'$.status')='rollback_failed' AND
      json_extract(outcome,'$.waiting_jobs') IS NOT NULL))))";

pub(crate) fn can_clear(db: &ScopedConnection, id: &str) -> rusqlite::Result<bool> {
    db.prepare(&format!(
        "SELECT 1 FROM action_approvals WHERE +org_id = :org_id AND id=?1 AND {KIND} AND {CLEARABLE}
         AND COALESCE(json_extract(outcome,'$.cleared'),0)=0
         AND COALESCE(json_extract(outcome,'$.acknowledged'),0)=0"
    ))?
    .exists([id])
}

fn bad() -> Reply {
    refusal(
        422,
        "invalid_request",
        "invalid release request or receipt",
        Value::Null,
    )
}

fn conflict() -> Reply {
    refusal(
        409,
        "conflict",
        "a release request is already open or has changed",
        Value::Null,
    )
}

fn slug(s: &str) -> bool {
    s.len() <= 100
        && s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && !s.contains("..")
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        && crate::failure::redact(s) == s
}

fn hex(value: &Value, lengths: &[usize]) -> bool {
    value
        .as_str()
        .is_some_and(|s| lengths.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn request(state: &State, call: &Call, conversation: &str, operation: &str) -> Reply {
    let body: Value = if operation == "smoke" {
        json!({})
    } else {
        serde_json::from_slice(&call.body).unwrap_or_default()
    };
    let bundle = body["bundle"].as_str();
    if !body.as_object().is_some_and(|obj| {
        obj.is_empty()
            || (operation == "install-release" && obj.len() == 1 && bundle.is_some_and(slug))
    }) {
        return bad();
    }
    let mut db = state.store.db();
    let result = (|| -> rusqlite::Result<Reply> {
        let node: Option<i64> = db
            .query_row(
                "SELECT n.id FROM (SELECT * FROM conversations WHERE +org_id = :org_id) c
             JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.name=c.node
             WHERE c.conversation_id=?1 AND c.lead=1 AND c.archived_at IS NULL",
                [conversation],
                |r| r.get(0),
            )
            .optional()?;
        let Some(node) = node else {
            return Ok(crate::client_api::not_found());
        };
        if operation == "smoke" {
            let outcome: Option<String> = db.query_row(
                &format!("SELECT outcome FROM action_approvals WHERE +org_id = :org_id AND node_id=?1
                 AND {KIND} AND json_extract(outcome,'$.smoke_ok') IN (0,1)
                 ORDER BY created_at DESC,rowid DESC LIMIT 1"),
                [node], |r| r.get(0),
            ).optional()?;
            return Ok(json(
                200,
                &json!({"last_smoke":outcome.and_then(|s| serde_json::from_str::<Value>(&s).ok())}),
            ));
        }
        crate::action_approvals::expire_land_requests(&db)?;
        let tx = write_tx(&mut db)?;
        if !crate::sponsorship::can_dispatch(&tx, conversation)? {
            return Ok(crate::sponsorship::ineligible());
        }
        if tx.prepare(&format!("SELECT 1 FROM action_approvals WHERE +org_id = :org_id AND {KIND}
              AND (status IN ('pending','needs_owner') OR
              (status='approved' AND (outcome IS NULL OR json_extract(outcome,'$.status')='unknown')))"))?
            .exists(params![])? {
            return Ok(conflict());
        }
        let kind = if operation == "rollback" {
            "rollback"
        } else {
            "install"
        };
        let id = format!(
            "act_{}",
            ulid()
                .map_err(rusqlite::Error::InvalidParameterName)?
                .to_lowercase()
        );
        tx.execute(
            "INSERT INTO action_approvals (org_id,id,conversation_id,node_id,request,created_at,assigned_member_id)
             VALUES (:org_id,?1,?2,?3,?4,?5,:member_id)",
            params![
                id,
                conversation,
                node,
                json!({"kind":kind,"bundle":bundle,
                "reason":"A node release switch requires owner approval."})
                .to_string(),
                now_utc()
            ],
        )?;
        touch(&tx, Record::ActionApproval, &id)?;
        tx.commit()?;
        Ok(reply(&db, &id))
    })();
    result.unwrap_or_else(|e| internal(&e))
}

pub(crate) fn expire_claims(db: &ScopedConnection) -> rusqlite::Result<()> {
    let ids = db.prepare(&format!(
        "UPDATE action_approvals SET outcome=json_object('status','interrupted',
            'reason','Release worker lease expired. Verify the live release and smoke status on the node.')
         WHERE +org_id = :org_id AND {KIND} AND status='approved' AND claimed_at IS NOT NULL
         AND json_extract(outcome,'$.status')='unknown'
         AND COALESCE(datetime(json_extract(request,'$.claim_until')),datetime(claimed_at, '+90 minutes')) <= datetime(?1)
         RETURNING id"
    ))?.query_map([now_utc()], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        touch(db, Record::ActionApproval, &id)?;
        touch_conversations(db, &id)?;
    }
    Ok(())
}

pub(crate) fn clear(db: &mut ScopedConnection, id: &str, owner: &str, acknowledge: bool) -> Reply {
    let result = (|| -> rusqlite::Result<Reply> {
        let tx = write_tx(db)?;
        let changed = tx.execute(&format!(
            "UPDATE action_approvals SET status=CASE WHEN claimed_at IS NULL THEN 'denied' ELSE status END,
             outcome=CASE WHEN json_extract(outcome,'$.status')='rollback_failed'
                          THEN json_set(outcome,'$.acknowledged',json('true'))
                          WHEN json_extract(outcome,'$.status')='interrupted'
                          THEN json_set(outcome,'$.cleared',json('true'))
                          ELSE json_object('status','interrupted','reason','Release request cleared by the owner.','cleared',json('true')) END,
             decided_by=CASE WHEN claimed_at IS NULL THEN ?2 ELSE decided_by END,
             decided_at=CASE WHEN claimed_at IS NULL THEN ?3 ELSE decided_at END,
             request=json_set(request,'$.closed_by',?2,'$.closed_at',?3)
             WHERE +org_id = :org_id AND id=?1 AND {KIND}
              AND (NOT ?4 OR (json_extract(outcome,'$.status')='rollback_failed'
                  AND json_type(outcome,'$.waiting_jobs') IN ('integer','real'))) AND {CLEARABLE}"
        ), params![id, owner, now_utc(), acknowledge])?;
        if changed == 0 {
            return Ok(conflict());
        }
        touch(&tx, Record::ActionApproval, id)?;
        touch_conversations(&tx, id)?;
        tx.commit()?;
        Ok(reply(db, id))
    })();
    result.unwrap_or_else(|error| internal(&error))
}

fn list(db: &ScopedConnection, node: i64) -> rusqlite::Result<Reply> {
    let mut stmt = db.prepare(&format!(
        "SELECT id FROM action_approvals WHERE +org_id = :org_id AND node_id=?1 AND {KIND}
         AND (status='pending' OR (status='approved' AND
           (outcome IS NULL OR json_extract(outcome,'$.status')='unknown' OR
            (json_extract(outcome,'$.status')='interrupted' AND
             json_extract(outcome,'$.release') IS NULL AND json_extract(outcome,'$.cleared') IS NULL)))) ORDER BY created_at,id"
    ))?;
    let mut rows = Vec::new();
    for id in stmt.query_map([node], |r| r.get::<_, String>(0))? {
        rows.extend(record(db, &id?)?);
    }
    Ok(json(200, &json!({"release_requests":rows})))
}

pub fn node(state: &State, call: &Call, node: i64, path: &str) -> Reply {
    let mut db = state.store.db();
    let parts: Vec<_> = path.split('/').collect();
    let result = (|| -> rusqlite::Result<Reply> {
        crate::action_approvals::expire_land_requests(&db)?;
        let ["release-requests", id, operation] = parts.as_slice() else {
            return list(&db, node);
        };
        let tx = write_tx(&mut db)?;
        if !tx
            .prepare(&format!(
                "SELECT 1 FROM action_approvals WHERE +org_id = :org_id
             AND node_id=?1 AND id=?2 AND {KIND}"
            ))?
            .exists(params![node, id])?
        {
            return Ok(crate::client_api::not_found());
        }
        let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
        let row = record(&tx, id)?.unwrap_or_default();
        let changed = match *operation {
            "inspect" => inspect(&tx, id, &body)?,
            "unavailable" => tx.execute(
                "UPDATE action_approvals SET status='expired',outcome=json_object('status','unavailable'),
                 review_reason='The node could not inspect and pin the requested release.'
                 WHERE +org_id = :org_id AND id=?1 AND status='pending'", [id],
            )?,
            "claim" => {
                if !hex(&body["target_hash"], &[40,64]) || body["target_hash"] != row["target_hash"] {
                    return Ok(conflict());
                }
                if row["status"] == "approved" && row["outcome"]["status"] == "unknown" {
                    return Ok(reply(&tx, id));
                }
                tx.execute("UPDATE action_approvals SET claimed_at=?2,outcome=json_object('status','unknown'),
                    request=json_set(request,'$.claim_until',?3)
                    WHERE +org_id = :org_id AND id=?1 AND status='approved' AND
                    decided_by IS NOT NULL AND claimed_at IS NULL", params![id,now_utc(),crate::util::utc_in(CLAIM_SECONDS)])?
            }
            "outcome" => {
                if body["target_hash"] != row["target_hash"] || !hex(&body["target_hash"], &[40,64]) {
                    return Ok(conflict());
                }
                outcome(state, &tx, id, &row, &body)?
            }
            _ => return Ok(crate::client_api::not_found()),
        };
        if changed == 0 {
            return Ok(conflict());
        }
        touch(&tx, Record::ActionApproval, id)?;
        touch_conversations(&tx, id)?;
        tx.commit()?;
        Ok(reply(&db, id))
    })();
    result.unwrap_or_else(|e| internal(&e))
}

fn inspect(db: &ScopedConnection, id: &str, body: &Value) -> rusqlite::Result<usize> {
    let Some(notes) = body["release_notes_diff"]
        .as_str()
        .filter(|s| s.len() <= 16000)
    else {
        return Ok(0);
    };
    if !hex(&body["commit"], &[40, 64])
        || !hex(&body["target_hash"], &[40, 64])
        || !["schema_current", "schema_target"].iter().all(|key| {
            body[key].is_null() || body[key].as_u64().is_some_and(|n| i32::try_from(n).is_ok())
        })
    {
        return Ok(0);
    }
    let notes = notes
        .split_inclusive('\n')
        .map(crate::failure::redact)
        .collect::<String>();
    let provenance = match body["commit_provenance"].as_str() {
        Some("local_main") => "Pinned from local main and built by node-bundle",
        _ => "claimed commit (from bundle.json)",
    };
    let backup = match body["backup"].as_str() {
        Some("local_and_s3") => "Local and configured off-machine backup",
        _ => "Local backup only",
    };
    let pin = json!({"commit":body["commit"], "target_hash":body["target_hash"],
        "schema_current":body["schema_current"],"schema_target":body["schema_target"],"release_notes_diff":notes,
        "commit_provenance":provenance,"backup":backup});
    db.execute(
        "UPDATE action_approvals SET request=json_patch(request,?2), status='needs_owner',
         verdict='no_go',reviewed_at=?3,outcome=json_object('status','waiting_on_owner'),
         review_reason='The node pinned the release. Approve the switch?'
         WHERE +org_id = :org_id AND id=?1 AND status='pending'",
        params![id, pin.to_string(), now_utc()],
    )
}

fn outcome(
    state: &State,
    db: &ScopedConnection,
    id: &str,
    row: &Value,
    body: &Value,
) -> rusqlite::Result<usize> {
    let (status, smoke, reason) = match body["status"].as_str() {
        Some("installed") if row["kind"] == "install" => {
            ("installed", Some(true), "Installation smoke passed")
        }
        Some("rolled_back") if row["kind"] == "install" => (
            "rolled_back",
            Some(false),
            "Installation smoke failed and the previous release was restored",
        ),
        Some("restored") if row["kind"] == "rollback" => {
            ("restored", None, "Owner-approved rollback completed")
        }
        Some("rollback_failed") if row["kind"] == "install" => (
            "rollback_failed",
            Some(false),
            "Installation smoke failed and rollback could not be verified",
        ),
        Some("interrupted") => ("interrupted", None, "Release worker interrupted"),
        Some("timed_out") => (
            "timed_out",
            None,
            "Release switch timed out without switching",
        ),
        Some("failed") => (
            "failed",
            None,
            "The node could not complete the approved release switch",
        ),
        _ => return Ok(0),
    };
    let Some(release) = body["release"].as_str().filter(|s| slug(s)) else {
        return Ok(0);
    };
    let waiting = (status == "rollback_failed")
        .then(|| body["waiting_jobs"].as_u64())
        .flatten();
    let reason = if let Some(jobs) = waiting {
        format!("failed release is live; rollback waiting on {jobs} jobs")
    } else {
        clean_text(
            body["reason"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(reason),
        )
    };
    let mut receipt = json!({"status":status,"reason":reason,"release":release});
    if let Some(ok) = smoke {
        receipt["smoke_ok"] = json!(ok);
        receipt["smoke"] = json!({"ok":ok,"checks":smoke_checks(&body["smoke"])});
    }
    if let Some(jobs) = waiting {
        receipt["waiting_jobs"] = json!(jobs);
    }
    if status == "interrupted" {
        receipt["switched_without_smoke"] = json!(body["switched_without_smoke"] == true);
    }
    if row["outcome"]["acknowledged"] == true && waiting.is_some() {
        receipt["acknowledged"] = json!(true);
    }
    if row["outcome"] == receipt {
        return Ok(1);
    }
    let changed = db.execute(
        "UPDATE action_approvals SET outcome=?2 WHERE +org_id = :org_id AND
         id=?1 AND claimed_at IS NOT NULL AND (json_extract(outcome,'$.status')='unknown' OR
            (?3='interrupted' AND json_extract(outcome,'$.status')='interrupted'
             AND json_extract(outcome,'$.release') IS NULL AND json_extract(outcome,'$.cleared') IS NULL))",
        params![id, receipt.to_string(), status],
    )?;
    if changed != 0 {
        if status == "restored" {
            acknowledge_recovered_rollbacks(db, id, release)?;
        }
        let conversation = row["conversation_id"].as_str().unwrap_or_default();
        if db
            .prepare(
                "SELECT 1 FROM conversations WHERE +org_id = :org_id
                 AND conversation_id=?1 AND archived_at IS NULL",
            )?
            .exists([conversation])?
        {
            crate::turns::enqueue(&state.store, db, conversation,
                &format!("Post-install status for {id}, commit {}. {reason}. Current release: {release}.",
                    row["commit"].as_str().unwrap_or_default()), "node-install", "wake",
                Some(&json!({"approval_id":id})))
                .map_err(rusqlite::Error::InvalidParameterName)?;
        }
    }
    Ok(changed)
}

fn acknowledge_recovered_rollbacks(
    db: &ScopedConnection,
    id: &str,
    live: &str,
) -> rusqlite::Result<()> {
    let ids = db.prepare(
        "UPDATE action_approvals SET outcome=json_set(outcome,'$.acknowledged',json('true'))
         WHERE +org_id = :org_id AND node_id=(SELECT node_id FROM action_approvals WHERE +org_id = :org_id AND id=?1)
         AND json_extract(request,'$.kind')='install'
         AND json_extract(outcome,'$.waiting_jobs') IS NOT NULL
         AND json_extract(outcome,'$.release') != ?2
         AND COALESCE(json_extract(outcome,'$.acknowledged'),0)=0 RETURNING id",
    )?.query_map(params![id,live], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        touch(db, Record::ActionApproval, &id)?;
        touch_conversations(db, &id)?;
    }
    Ok(())
}

fn smoke_checks(smoke: &Value) -> Vec<Value> {
    smoke["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .take(64)
        .filter_map(|check| {
            let mut result = json!({"name":clean_text(check["name"].as_str()?),
                "ok":check["ok"].as_bool()?});
            if let Some(reason) = check["reason"].as_str() {
                result["reason"] = json!(clean_text(reason));
            }
            for field in ["seconds", "skipped"] {
                if check[field].is_number() || check[field].is_boolean() {
                    result[field] = check[field].clone();
                }
            }
            Some(result)
        })
        .collect()
}

fn clean_text(value: &str) -> String {
    value
        .lines()
        .map(crate::failure::redact)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(2000)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_and_slug_validate_release_receipts() {
        assert!(slug("release-1.2"));
        assert!(!slug("../release"));
        assert!(!slug("release\n"));
        assert!(hex(&json!("e".repeat(64)), &[40, 64]));
        assert!(!hex(&json!("g".repeat(64)), &[40, 64]));
    }

    #[test]
    fn expiry_only_interrupts_aged_claimed_release_requests()
    -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let store = crate::store::Store::open(&tmp.path().join("director"))?;
        let mut db = store.db();
        db.fixture_batch(
            "INSERT INTO nodes (org_id,id,name,token_hash,created_at) VALUES(:org_id,1,'node','hash','now');
             INSERT INTO conversations (org_id,conversation_id,node,spec,spec_version,model,
                 instructions_tokens,tools,created_by,created_at)
             VALUES(:org_id,'con','node','s','v','m',0,0,'local','now');
             INSERT INTO action_approvals (org_id,id,conversation_id,node_id,status,claimed_at,request,outcome,created_at)
             VALUES (:org_id,'expired','con',1,'approved','2000-01-01T00:00:00Z',
                     '{\"kind\":\"install\",\"claim_until\":\"2000-01-01T00:15:00Z\"}','{\"status\":\"unknown\"}','2000'),
                    (:org_id,'active','con',1,'approved','2000-01-01T00:00:00Z',
                     '{\"kind\":\"rollback\",\"claim_until\":\"9999-01-01T00:15:00Z\"}','{\"status\":\"unknown\"}','2000'),
                    (:org_id,'land','con',1,'approved','2000-01-01T00:00:00Z',
                     '{\"kind\":\"land\"}','{\"status\":\"unknown\"}','2000')",
        )?;
        expire_claims(&db)?;
        assert_eq!(
            record(&db, "expired")?.ok_or("missing expired row")?["outcome"]["status"],
            "interrupted"
        );
        assert_eq!(
            record(&db, "active")?.ok_or("missing active row")?["outcome"]["status"],
            "unknown"
        );
        assert_eq!(
            record(&db, "land")?.ok_or("missing land row")?["outcome"]["status"],
            "unknown"
        );
        assert_eq!(clear(&mut db, "active", "owner", false).status(), 200);
        assert_eq!(
            record(&db, "active")?.ok_or("missing active row")?["outcome"]["cleared"],
            true
        );
        Ok(())
    }
}
