//! Hosted land requests. The node inspects and executes a pinned local candidate.

use crate::access::Client;
use crate::action_approvals::{record, record_outcome, reply, touch_conversations};
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::{Record, ScopedConnection, touch, write_tx};
use crate::util::{now_utc, ulid};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

fn text<'a>(body: &'a Value, name: &str) -> &'a str {
    body[name].as_str().unwrap_or_default()
}

fn slug(s: &str) -> bool {
    s.len() <= 63
        && s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && !s.contains("..")
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_.".contains(&c))
}

fn oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
}

fn bad(message: &str) -> Reply {
    refusal(422, "invalid_request", message, Value::Null)
}

fn missing() -> Reply {
    refusal(
        404,
        "land_lane_not_found",
        "no finished lane in this hosted conversation's project",
        Value::Null,
    )
}

fn conflict() -> Reply {
    refusal(
        409,
        "conflict",
        "the land request is no longer in the required state",
        Value::Null,
    )
}

// A projectless conversation may use lanes it started. An ambiguous repo/lane
// shared by projects on the same node is never eligible for hosted landing.
const TARGET: &str = "
    SELECT j.node_id,j.run_id FROM (SELECT * FROM conversations WHERE +org_id = :org_id) c
    JOIN (SELECT * FROM intents WHERE +org_id = :org_id) i ON c.node=i.node
    JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.intent_id=i.intent_id
    JOIN (SELECT * FROM runs WHERE +org_id = :org_id) r ON r.run_id=j.run_id AND r.node_id=j.node_id
    WHERE c.conversation_id=?1 AND c.lead=1 AND c.archived_at IS NULL
      AND ((c.project_id IS NOT NULL AND c.project_id=i.project_id)
           OR (c.project_id IS NULL AND i.project_id IS NULL AND i.lead_conversation_id=c.conversation_id))
      AND json_extract(i.meta,'$.repo')=?2 AND json_extract(i.meta,'$.lane')=?3
      AND COALESCE(json_extract(i.meta,'$.repo_read_only'),'false')='false'
      AND COALESCE(json_extract(i.meta,'$.agent'),'') NOT IN ('gate','reviewer')
      AND j.state='done'
      AND json_extract(r.row_line,'$.lane.repo')=?2 AND json_extract(r.row_line,'$.lane.lane')=?3
      AND NOT EXISTS (SELECT 1 FROM intents other WHERE +org_id = :org_id AND other.node=i.node
          AND json_extract(other.meta,'$.repo')=?2 AND json_extract(other.meta,'$.lane')=?3
          AND COALESCE(json_extract(other.meta,'$.repo_read_only'),'false')='false'
          AND (other.project_id IS NOT i.project_id OR
               (i.project_id IS NULL AND other.lead_conversation_id IS NOT i.lead_conversation_id
                 AND COALESCE(json_extract(other.meta,'$.agent'),'') NOT IN ('gate','reviewer'))))
    ORDER BY j.created_at DESC,j.job_id DESC LIMIT 1";

fn target(
    db: &ScopedConnection,
    conversation: &str,
    repo: &str,
    lane: &str,
) -> rusqlite::Result<Option<(i64, String)>> {
    db.query_row(TARGET, params![conversation, repo, lane], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .optional()
}

pub fn request(state: &State, call: &Call, client: &Client, conversation: &str) -> Reply {
    let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
    let (repo, lane) = (text(&body, "repo"), text(&body, "lane"));
    if !slug(repo) || !slug(lane) || body.as_object().is_none_or(|m| m.len() != 2) {
        return bad("supply a repo and lane");
    }
    let mut db = state.store.db();
    let result = (|| -> rusqlite::Result<Reply> {
        crate::action_approvals::expire_land_requests(&db)?;
        let tx = write_tx(&mut db)?;
        if !crate::sponsorship::can_dispatch(&tx, conversation)? {
            return Ok(crate::sponsorship::ineligible());
        }
        let Some((node, run)) = target(&tx, conversation, repo, lane)? else {
            return Ok(missing());
        };
        // A repeated request is also how the lead checks its outcome.
        let open: Option<String> = tx
            .query_row(
                "SELECT id FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) a
             JOIN (SELECT approval_id FROM land_requests WHERE +org_id = :org_id) l ON l.approval_id=a.id
             WHERE node_id=?1
             AND json_extract(request,'$.repo')=?2 AND json_extract(request,'$.lane')=?3
             AND ((json_extract(request,'$.target_run_id')=?4 AND status='approved'
                AND json_extract(outcome,'$.status')='succeeded')
                OR status IN ('pending','needs_owner') OR (status='approved'
                AND (outcome IS NULL OR json_extract(outcome,'$.status') IN ('unknown','busy'))))
             ORDER BY created_at DESC,id DESC LIMIT 1",
                params![node, repo, lane, run],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = open {
            let row = record(&tx, &id)?.unwrap_or_default();
            return Ok(if row["conversation_id"] == conversation {
                json(200, &row)
            } else {
                conflict()
            });
        }
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
                json!({"kind":"land","repo":repo,"lane":lane,
                "target_run_id":run,"reason":format!("Land {repo}/{lane} locally"),
                "argv":["cs","land",repo,lane]})
                .to_string(),
                now_utc()
            ],
        )?;
        tx.execute(
            "INSERT INTO land_requests (org_id,approval_id,requested_by_member_id)
             VALUES (:org_id,?1,?2)",
            params![id, client.member_id()],
        )?;
        touch(&tx, Record::ActionApproval, &id)?;
        tx.commit()?;
        Ok(reply(&db, &id))
    })();
    result.unwrap_or_else(|e| internal(&e))
}

// Gate jobs are model-free and tested at the executor's recorded starting HEAD.
// Reviewer metadata is client-controlled, so only a human owner submission qualifies.
const EVIDENCE: &str = "
    SELECT r.row_line,f.sha256,r.verdict FROM (SELECT * FROM intents WHERE +org_id = :org_id) i
    JOIN (SELECT * FROM conversations WHERE +org_id = :org_id) c ON c.node=i.node
    JOIN (SELECT * FROM credentials WHERE +org_id = :org_id) k ON k.id=i.created_by
    LEFT JOIN (SELECT * FROM members WHERE +org_id = :org_id) m ON m.id=k.member_id
    JOIN (SELECT rowid AS insertion_order,* FROM jobs WHERE +org_id = :org_id) j ON j.intent_id=i.intent_id
    LEFT JOIN (SELECT * FROM runs WHERE +org_id = :org_id) r
        ON r.run_id=j.run_id AND r.node_id=j.node_id
        AND ((?5='gate' AND r.verdict='pass' AND j.state='done')
             OR (?5='reviewer' AND j.state IN ('done','error')))
    LEFT JOIN (SELECT * FROM run_files WHERE +org_id = :org_id) f
        ON f.run_id=r.run_id AND f.path='gate.json' AND f.bytes<=65536
    WHERE c.conversation_id=?1 AND c.project_id IS i.project_id AND j.node_id=?2
      AND json_extract(i.meta,'$.repo')=?3 AND json_extract(i.meta,'$.lane')=?4
      AND json_extract(i.meta,'$.agent')=?5 AND (?5='gate' OR (k.kind IN ('local','device') AND m.role='owner' AND m.status='active'))
    ORDER BY j.created_at DESC,j.insertion_order DESC";

fn verified_lane_run(execution: &Value, request: &Value) -> bool {
    execution["base_sha"] == request["commit"]
        && execution["lane"]["repo"] == request["repo"]
        && execution["lane"]["lane"] == request["lane"]
        && execution["lane"]["dirty_at_start"] == false
}

fn policy(
    state: &State,
    db: &ScopedConnection,
    node: i64,
    row: &Value,
) -> rusqlite::Result<&'static str> {
    if row["protected"] != false {
        return Ok("Protected paths changed or could not be verified.");
    }
    if row["tracing"] != false {
        return Ok("The lane changes tracing/.");
    }
    let mut stmt = db.prepare(EVIDENCE)?;
    let mut for_role = |role| {
        stmt.query_map(
            params![
                text(row, "conversation_id"),
                node,
                text(row, "repo"),
                text(row, "lane"),
                role
            ],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
    };
    // The newest gate takes precedence, including a running or failed gate.
    let gate = for_role("gate")?
        .into_iter()
        .next()
        .and_then(|(execution, hash, _)| {
            let execution: Value = serde_json::from_str(execution.as_deref()?).ok()?;
            if execution["model"] != "gate" || !verified_lane_run(&execution, row) {
                return None;
            }
            serde_json::from_slice::<Value>(&state.store.read_object(&hash?)?).ok()
        })
        .unwrap_or_default();
    if gate["verdict"] != "pass"
        || gate["complete"] != true
        || !gate["uncovered"].as_array().is_some_and(Vec::is_empty)
    {
        return Ok("The latest gate lacks a complete pass for this commit.");
    }
    for (execution, _, verdict) in for_role("reviewer")? {
        let Some(execution) = execution.and_then(|json| serde_json::from_str::<Value>(&json).ok())
        else {
            continue;
        };
        if execution["base_sha"] == row["commit"] {
            return Ok(
                if verdict.as_deref() == Some("pass")
                    && verified_lane_run(&execution, row)
                    && execution["review_verdict"] == "approve"
                {
                    ""
                } else {
                    "The newest independent reviewer did not approve this commit."
                },
            );
        }
    }
    Ok("No independent reviewer lane approved this commit.")
}

fn inspect(
    state: &State,
    db: &ScopedConnection,
    node: i64,
    id: &str,
    mut row: Value,
    body: &Value,
) -> rusqlite::Result<Reply> {
    let (commit, base) = (text(body, "commit"), text(body, "base"));
    if !oid(commit)
        || !oid(base)
        || !body["protected"].is_boolean()
        || !body["tracing"].is_boolean()
    {
        return Ok(bad(
            "a pinned commit, base and path inspection are required",
        ));
    }
    for key in ["commit", "base", "protected", "tracing"] {
        row[key] = body[key].clone();
    }
    let prior: Option<bool> = db
        .query_row(
            "SELECT COALESCE(json_extract(outcome,'$.status')='failed',0)
           FROM action_approvals WHERE +org_id = :org_id AND node_id=?1
           AND json_extract(request,'$.repo')=?2 AND json_extract(request,'$.lane')=?3
           AND json_extract(request,'$.commit')=?4 AND (status='denied' OR claimed_at IS NOT NULL)
           ORDER BY 1 LIMIT 1",
            params![node, text(&row, "repo"), text(&row, "lane"), commit],
            |r| r.get(0),
        )
        .optional()?;
    let already_decided = prior == Some(false);
    let reason = if already_decided {
        "This lane commit was already claimed or denied."
    } else if prior == Some(true) {
        "An earlier landing attempt failed. The owner must approve another attempt."
    } else {
        policy(state, db, node, &row)?
    };
    let (status, verdict) = if reason.is_empty() {
        ("approved", "go")
    } else if already_decided {
        ("expired", "no_go")
    } else {
        ("needs_owner", "no_go")
    };
    let pin = json!({
        "commit":commit,"base":base,"protected":body["protected"],"tracing":body["tracing"],
        "reason": format!("Land {}/{} at {commit} locally", text(&row,"repo"), text(&row,"lane"))
    });
    let tx = db.unchecked_transaction()?;
    if tx.execute(
        "UPDATE action_approvals SET request=json_patch(request,?2),status=?3,verdict=?4,
            review_reason=?5,reviewed_at=?6,
            outcome=CASE WHEN ?3='needs_owner' THEN json_object('status','waiting_on_owner') END
            WHERE +org_id = :org_id AND id=?1 AND status='pending'",
        params![id, pin.to_string(), status, verdict, reason, now_utc()],
    )? == 0
    {
        return Ok(conflict());
    }
    touch(&tx, Record::ActionApproval, id)?;
    touch_conversations(&tx, id)?;
    tx.commit()?;
    Ok(reply(db, id))
}

fn claim(
    state: &State,
    db: &ScopedConnection,
    node: i64,
    id: &str,
    row: &Value,
) -> rusqlite::Result<Reply> {
    if row["status"] != "approved" || !row["claimed_at"].is_null() {
        return Ok(conflict());
    }
    let current = target(
        db,
        text(row, "conversation_id"),
        text(row, "repo"),
        text(row, "lane"),
    )?;
    let target_changed = current
        .as_ref()
        .is_none_or(|(n, run)| *n != node || run != text(row, "target_run_id"));
    let reason = if target_changed {
        "The lane's finished run changed before execution."
    } else if row["decided_by"].is_null() {
        policy(state, db, node, row)?
    } else {
        ""
    };
    let tx = db.unchecked_transaction()?;
    if reason.is_empty() {
        if tx.execute(
            "UPDATE action_approvals SET claimed_at=?2,outcome=json_object('status','unknown')
             WHERE +org_id = :org_id AND id=?1 AND status='approved' AND claimed_at IS NULL",
            params![id, now_utc()],
        )? == 0
        {
            return Ok(conflict());
        }
    } else {
        tx.execute(
            "UPDATE action_approvals SET status=?2,verdict='no_go',review_reason=?3,reviewed_at=?4,
             outcome=json_object('status',CASE WHEN ?2='expired' THEN 'expired' ELSE 'waiting_on_owner' END)
             WHERE +org_id = :org_id AND id=?1 AND status='approved' AND claimed_at IS NULL",
            params![
                id,
                if target_changed {
                    "expired"
                } else {
                    "needs_owner"
                },
                reason,
                now_utc()
            ],
        )?;
    }
    touch(&tx, Record::ActionApproval, id)?;
    touch_conversations(&tx, id)?;
    tx.commit()?;
    Ok(if reason.is_empty() {
        reply(db, id)
    } else {
        conflict()
    })
}

pub fn node(state: &State, call: &Call, node: i64, path: &str) -> Reply {
    let db = state.store.db();
    let parts: Vec<_> = path.split('/').collect();
    let result = (|| {
        crate::action_approvals::expire_land_requests(&db)?;
        match (call.method.as_str(), parts.as_slice()) {
            ("GET", ["land-requests"]) => {
                let mut stmt = db.prepare(
                "SELECT id FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) a
                    JOIN (SELECT approval_id FROM land_requests WHERE +org_id = :org_id) l ON l.approval_id=a.id
                    WHERE node_id=?1
                    AND status IN ('pending','approved') AND claimed_at IS NULL
                    ORDER BY created_at,id LIMIT 100",
            )?;
                let ids = stmt.query_map([node], |r| r.get::<_, String>(0))?;
                let mut rows = Vec::new();
                for id in ids {
                    rows.extend(record(&db, &id?)?);
                }
                Ok(json(200, &json!({"land_requests":rows})))
            }
            ("POST", ["land-requests", id, op]) => {
                if !db.prepare("SELECT 1 FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) a
                JOIN (SELECT approval_id FROM land_requests WHERE +org_id = :org_id) l ON l.approval_id=a.id
                WHERE id=?1 AND node_id=?2")?
                    .exists(params![id,node])? {
                    return Ok(missing());
                }
                let row = record(&db, id)?.unwrap_or_default();
                let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
                match *op {
                    "inspect" => inspect(state, &db, node, id, row, &body),
                    "unavailable" => unavailable(&db, id),
                    "claim" => claim(state, &db, node, id, &row),
                    "outcome" => {
                        if body["status"] == "busy" {
                            return release_busy(&db, id, &row, &body);
                        }
                        if !valid_outcome(&body) {
                            return Ok(bad("invalid land outcome"));
                        }
                        Ok(if record_outcome(&db, id, &body.to_string())? {
                            reply(&db, id)
                        } else {
                            conflict()
                        })
                    }
                    _ => Ok(missing()),
                }
            }
            _ => Ok(missing()),
        }
    })();
    result.unwrap_or_else(|e| internal(&e))
}

fn unavailable(db: &ScopedConnection, id: &str) -> rusqlite::Result<Reply> {
    let tx = db.unchecked_transaction()?;
    if tx.execute(
        "UPDATE action_approvals SET status='expired',verdict='no_go',
           review_reason='The node could not pin a local commit and base. Check the lane and request landing again.',
           outcome=json_object('status','unavailable'),
           reviewed_at=?2 WHERE +org_id = :org_id AND id=?1 AND status='pending'",
        params![id,now_utc()],
    )? == 0 {
        return Ok(conflict());
    }
    touch(&tx, Record::ActionApproval, id)?;
    touch_conversations(&tx, id)?;
    tx.commit()?;
    Ok(reply(db, id))
}

fn release_busy(
    db: &ScopedConnection,
    id: &str,
    row: &Value,
    body: &Value,
) -> rusqlite::Result<Reply> {
    let Some(revision) = body["claim_revision"].as_i64().filter(|r| *r > 0) else {
        return Ok(bad("busy outcome requires a claim revision"));
    };
    if row["revision"]
        .as_i64()
        .is_some_and(|current| current > revision)
    {
        // A lost response can be retried after the release or subsequent claim.
        return Ok(reply(db, id));
    }
    let tx = db.unchecked_transaction()?;
    if tx.execute(
        "UPDATE action_approvals SET claimed_at=NULL,outcome=json_object('status','busy')
         WHERE +org_id = :org_id AND id=?1 AND revision=?2 AND status='approved'
         AND claimed_at IS NOT NULL AND json_extract(outcome,'$.status')='unknown'",
        params![id, revision],
    )? == 0
    {
        return Ok(conflict());
    }
    touch(&tx, Record::ActionApproval, id)?;
    tx.commit()?;
    Ok(reply(db, id))
}

fn valid_outcome(body: &Value) -> bool {
    let Some(attempts) = body["attempts"].as_array() else {
        return false;
    };
    let valid_count = match text(body, "status") {
        "succeeded" | "failed" => matches!(attempts.len(), 1 | 2),
        "interrupted" => attempts.len() < 2,
        _ => false,
    };
    valid_count && body.to_string().len() <= 16_000
}
