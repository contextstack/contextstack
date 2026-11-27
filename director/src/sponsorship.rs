use crate::access::{Client, Member, Role};
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::{Record, ScopedConnection, Store, touch, write_tx};
use crate::util::now_utc;
use crate::work::{No, body, content, key, remember, scoped_prior};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub fn sponsor(db: &ScopedConnection, id: &str) -> rusqlite::Result<Option<String>> {
    db.query_row(
        "SELECT COALESCE(sponsor_member_id,responsible_member_id,:member_id)
         FROM conversations WHERE +org_id=:org_id AND conversation_id=?1",
        [id],
        |r| r.get(0),
    )
    .optional()
}

pub fn can_dispatch(db: &ScopedConnection, id: &str) -> rusqlite::Result<bool> {
    match sponsor(db, id)? {
        Some(sponsor) => crate::policy::eligible_sponsor(db, &sponsor),
        None => Ok(false),
    }
}

pub fn ineligible() -> Reply {
    refusal(
        409,
        "sponsor_ineligible",
        "adopt this conversation before requesting a host action",
        Value::Null,
    )
}

fn schedule_owner(db: &ScopedConnection, key: &str) -> rusqlite::Result<Option<String>> {
    db.query_row(
        "SELECT COALESCE(responsible_member_id,:member_id) FROM schedules
         WHERE +org_id=:org_id AND schedule_key=?1",
        [key],
        |r| r.get(0),
    )
    .optional()
}

pub fn schedule_can_fire(db: &ScopedConnection, key: &str) -> rusqlite::Result<bool> {
    match schedule_owner(db, key)? {
        Some(member) => crate::policy::eligible_sponsor(db, &member),
        None => Ok(true),
    }
}

pub fn reassign_schedule(state: &State, call: &Call, client: &Client, id: &str) -> Reply {
    let mut db = state.store.db();
    let result = (|| -> Result<Value, No> {
        let key = key(call)?;
        let input = body(call)?;
        let next = input
            .get("responsible_member_id")
            .and_then(Value::as_str)
            .filter(|member| !member.is_empty() && input.len() == 1)
            .ok_or_else(|| {
                No::invalid("responsible_member_id must be the sole field and name a member")
            })?;
        let hash = content(
            "schedule_reassign",
            id,
            &json!({"responsible_member_id":next}),
        );
        let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
        let previous = schedule_owner(&tx, id)
            .map_err(|e| No::internal(&e))?
            .ok_or_else(missing)?;
        if scoped_prior(&tx, &key, &hash, client)?.is_none() {
            require_assignment(&tx, client, &previous, next)?;
            if previous != next {
                tx.execute(
                    "UPDATE schedules SET responsible_member_id=?2 WHERE +org_id=:org_id AND schedule_key=?1",
                    params![id,next],
                ).map_err(|e| No::internal(&e))?;
                touch(&tx, Record::Schedule, id).map_err(|e| No::internal(&e))?;
                audit_assignment(&tx, "schedules", id, &previous, next, client)
                    .map_err(|e| No::internal(&e))?;
                transfer_attention(&tx, id, next, client, true).map_err(|e| No::internal(&e))?;
                crate::reservations::release(&state.store, &tx).map_err(|e| No::internal(&e))?;
            }
            remember(&tx, &key, &hash, Record::Schedule, id, client)
                .map_err(|e| No::internal(&e))?;
        }
        let response = crate::fleet::schedule_detail(&tx, id).map_err(|e| No::internal(&e))?;
        tx.commit().map_err(|e| No::internal(&e))?;
        Ok(response)
    })();
    match result {
        Ok(response) => json(200, &response),
        Err(e) => e.reply(),
    }
}

fn audit_assignment(
    db: &ScopedConnection,
    table: &str,
    id: &str,
    former: &str,
    next: &str,
    actor: &Client,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO assignment_changes
         (org_id,record_kind,record_id,previous_member_id,assignee_member_id,actor_member_id,actor_credential_id,changed_at)
         VALUES (:org_id,?1,?2,?3,?4,?5,?6,?7)",
        params![table,id,former,next,actor.member_id(),actor.id,now_utc()],
    )?;
    Ok(())
}

pub fn assignee(
    db: &ScopedConnection,
    candidate: Option<&str>,
    capability: crate::policy::Capability,
) -> rusqlite::Result<String> {
    if let Some(candidate) = candidate
        && let Some(member) = Member::load(db, candidate.to_owned())?
        && member.active
        && member
            .scope
            .role
            .is_some_and(|role| crate::policy::eligible_role(role, capability))
    {
        return Ok(candidate.to_owned());
    }
    Ok(active_owner(db, None)
        .optional()?
        .unwrap_or_else(|| db.scope().member_id.clone()))
}

pub fn job_assignee(db: &ScopedConnection, job: &str) -> rusqlite::Result<String> {
    let responsible: Option<String> = db.query_row(
        "SELECT COALESCE(c.sponsor_member_id,c.responsible_member_id,s.responsible_member_id,j.responsible_member_id)
         FROM (SELECT * FROM jobs WHERE +org_id=:org_id) j
         LEFT JOIN (SELECT * FROM job_budgets WHERE +org_id=:org_id) b ON b.job_id=j.job_id
         LEFT JOIN (SELECT * FROM intents WHERE +org_id=:org_id) i ON i.intent_id=j.intent_id
         LEFT JOIN (SELECT * FROM conversations WHERE +org_id=:org_id) c
           ON c.conversation_id=COALESCE(b.conversation_id,i.origin_conversation_id,i.lead_conversation_id)
         LEFT JOIN (SELECT * FROM schedules WHERE +org_id=:org_id) s
           ON s.node_id=j.node_id AND s.schedule_id=json_extract(j.meta,'$.schedule_id')
         WHERE j.job_id=?1", [job], |r| r.get(0),
    ).optional()?.flatten();
    assignee(
        db,
        responsible.as_deref(),
        crate::policy::Capability::ApproveAction,
    )
}

pub(crate) fn active_owner(
    db: &ScopedConnection,
    excluded: Option<&str>,
) -> rusqlite::Result<String> {
    db.query_row(
        "SELECT id FROM members WHERE +org_id=:org_id AND role='owner' AND status='active'
           AND (?1 IS NULL OR id<>?1) ORDER BY (id=:member_id) DESC, id LIMIT 1",
        [excluded],
        |row| row.get(0),
    )
}

pub fn reroute_attention(
    db: &ScopedConnection,
    former: &str,
    actor: &Client,
) -> rusqlite::Result<()> {
    use crate::policy::Capability::{ApproveAction, DecideRun};
    let policies = [
        (
            ApproveAction,
            "action_approvals",
            "id",
            "assigned_member_id",
            "status IN ('pending','needs_owner')",
            Record::ActionApproval,
        ),
        (
            DecideRun,
            "conversations",
            "conversation_id",
            "attention_assignee_member_id",
            "attention_question IS NOT NULL",
            Record::Conversation,
        ),
        (
            DecideRun,
            "task_needs_you",
            "needs_you_id",
            "assignee_member_id",
            "status='open'",
            Record::Intent,
        ),
        (
            ApproveAction,
            "backlog_items",
            "item_id",
            "assignee_member_id",
            crate::backlog::OWNER_BLOCKED,
            Record::BacklogItem,
        ),
    ];
    for (capability, table, key, column, open, kind) in policies {
        let next = assignee(db, Some(former), capability)?;
        let record_key = if kind == Record::Intent {
            "intent_id"
        } else {
            key
        };
        let owner_only = match kind {
            Record::ActionApproval => crate::policy::OWNER_ONLY_APPROVAL,
            Record::Conversation => "attention_set_by IS 'director'",
            _ => "0",
        };
        let owner = if matches!(kind, Record::ActionApproval | Record::Conversation) {
            active_owner(db, Some(former))
                .optional()?
                .unwrap_or_else(|| former.to_string())
        } else {
            next.clone()
        };
        let ids = db
            .prepare(&format!(
                "UPDATE {table} SET {column}=CASE WHEN {owner_only} THEN ?3 ELSE ?2 END
             WHERE +org_id=:org_id AND {column}=?1 AND ({open})
             AND {column}!=CASE WHEN {owner_only} THEN ?3 ELSE ?2 END
             RETURNING {key}, {record_key}, {column}",
            ))?
            .query_map(params![former, next, owner], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, record_id, next) in ids {
            audit_assignment(db, table, &id, former, &next, actor)?;
            touch(db, kind, &record_id)?;
            if kind == Record::ActionApproval {
                crate::action_approvals::touch_conversations(db, &id)?;
            }
        }
    }
    Ok(())
}

fn transfer_attention(
    db: &ScopedConnection,
    id: &str,
    sponsor: &str,
    actor: &Client,
    schedule: bool,
) -> rusqlite::Result<()> {
    use crate::policy::{
        Capability::{ApproveAction, DecideRun},
        OWNER_ONLY_APPROVAL,
    };
    let scheduled_jobs = "SELECT j.job_id FROM (SELECT * FROM jobs WHERE +org_id=:org_id) j
        JOIN (SELECT * FROM schedules WHERE +org_id=:org_id) s
          ON s.node_id=j.node_id AND s.schedule_id=json_extract(j.meta,'$.schedule_id')
        WHERE s.schedule_key=?1";
    let approval_scope = if schedule {
        format!("job_id IN ({scheduled_jobs})")
    } else {
        let link = crate::action_approvals::CONVERSATION_LINK;
        format!("id IN (SELECT a.id FROM (SELECT * FROM action_approvals WHERE +org_id=:org_id) a
            LEFT JOIN (SELECT * FROM jobs WHERE +org_id=:org_id) j ON j.job_id=a.job_id
            LEFT JOIN (SELECT * FROM intents WHERE +org_id=:org_id) i ON i.intent_id=j.intent_id
            JOIN (SELECT * FROM conversations WHERE +org_id=:org_id) c ON ({link} OR c.conversation_id=(
                SELECT conversation_id FROM job_budgets WHERE +org_id=:org_id AND job_id=a.job_id
            )) WHERE c.conversation_id=?1)")
    };
    let task_scope = if schedule {
        format!("job_id IN ({scheduled_jobs})")
    } else {
        "conversation_id=?1".into()
    };
    let mut targets = vec![
        (
            Record::ActionApproval,
            "action_approvals",
            "id",
            "id",
            "assigned_member_id",
            format!(
                "({approval_scope}) AND status IN ('pending','needs_owner') AND NOT ({OWNER_ONLY_APPROVAL})"
            ),
            assignee(db, Some(sponsor), ApproveAction)?,
        ),
        (
            Record::Intent,
            "task_needs_you",
            "needs_you_id",
            "intent_id",
            "assignee_member_id",
            format!("({task_scope}) AND status='open'"),
            assignee(db, Some(sponsor), DecideRun)?,
        ),
    ];
    if crate::members::active_role(db, sponsor)? == Some(Role::Owner) {
        targets.push((
            Record::ActionApproval,
            "action_approvals",
            "id",
            "id",
            "assigned_member_id",
            format!("({approval_scope}) AND status IN ('pending','needs_owner') AND ({OWNER_ONLY_APPROVAL})"),
            sponsor.to_owned(),
        ));
    }
    if !schedule {
        targets.push((Record::Conversation, "conversations", "conversation_id", "conversation_id",
            "attention_assignee_member_id",
            "conversation_id=?1 AND attention_question IS NOT NULL AND attention_set_by IS NOT 'director'".into(),
            assignee(db, Some(sponsor), DecideRun)?));
    }
    for (kind, table, key, record_key, column, scope, next) in targets {
        let rows = db
            .prepare(&format!(
                "SELECT {key},{record_key},COALESCE({column},:member_id) FROM {table}
             WHERE +org_id=:org_id AND ({scope}) AND {column} IS NOT ?2",
            ))?
            .query_map(params![id, next], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (key_value, record_id, former) in rows {
            db.execute(
                &format!("UPDATE {table} SET {column}=?2 WHERE +org_id=:org_id AND {key}=?1"),
                params![key_value, next],
            )?;
            audit_assignment(db, table, &key_value, &former, &next, actor)?;
            touch(db, kind, &record_id)?;
            if kind == Record::ActionApproval {
                crate::action_approvals::touch_conversations(db, &key_value)?;
            }
        }
    }
    Ok(())
}

pub fn revoke_leads(
    db: &ScopedConnection,
    conversation: &str,
    except: Option<&str>,
    actor: &str,
) -> rusqlite::Result<Vec<String>> {
    db.prepare(
        "UPDATE credentials SET revoked_at=?2,revoked_by=?3
         WHERE +org_id=:org_id AND conversation_id=?1 AND kind='lead' AND revoked_at IS NULL
         AND (?4 IS NULL OR id!=?4) RETURNING id",
    )?
    .query_map(params![conversation, now_utc(), actor, except], |r| {
        r.get(0)
    })?
    .collect()
}

fn forbidden() -> No {
    No(
        403,
        "forbidden",
        "sponsorship change is outside this member's authority".into(),
        Value::Null,
    )
}

fn missing() -> No {
    No(
        404,
        "not_found",
        "no such conversation or member".into(),
        Value::Null,
    )
}

fn require_assignment(
    db: &ScopedConnection,
    client: &Client,
    previous: &str,
    next: &str,
) -> Result<(), No> {
    let actor = crate::members::actor_role(db, client)?;
    let role = |id: &str| {
        Member::load(db, id.to_owned())
            .map_err(|e| No::internal(&e))?
            .and_then(|member| member.scope.role)
            .ok_or_else(missing)
    };
    let destination = role(next)?;
    if (actor != Role::Owner && !crate::members::manages(actor, role(previous)?))
        || (next != client.member_id() && !crate::members::manages(actor, destination))
        || !crate::policy::eligible_sponsor(db, next).map_err(|e| No::internal(&e))?
    {
        return Err(forbidden());
    }
    Ok(())
}

pub fn resume_member(store: &Store, db: &ScopedConnection, member: &str) -> Result<(), No> {
    crate::reservations::release(store, db).map_err(|e| No::internal(&e))?;
    let ids = db
        .prepare(
            "SELECT conversation_id FROM conversations WHERE +org_id=:org_id
         AND COALESCE(sponsor_member_id,responsible_member_id,:member_id)=?1 ORDER BY rowid",
        )
        .and_then(|mut stmt| {
            stmt.query_map([member], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|e| No::internal(&e))?;
    for id in ids {
        crate::turns::advance(store, db, &id).map_err(|e| No::internal(&e))?;
    }
    Ok(())
}

pub fn can_adopt(db: &ScopedConnection, client: &Client, id: &str) -> bool {
    sponsor(db, id).ok().flatten().is_some_and(|previous| {
        previous != client.member_id()
            && require_assignment(db, client, &previous, client.member_id()).is_ok()
    })
}

pub fn adopt(state: &State, call: &Call, client: &Client, id: &str) -> Reply {
    let mut db = state.store.db();
    let result = (|| -> Result<Vec<String>, No> {
        let key = key(call)?;
        let input = body(call)?;
        let next = input
            .get("sponsor_member_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && input.len() == 1)
            .ok_or_else(|| {
                No::invalid("sponsor_member_id must be the sole field and name a member")
            })?;
        let hash = content("conversation_adopt", id, &json!({"sponsor_member_id":next}));
        let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
        let previous = sponsor(&tx, id)
            .map_err(|e| No::internal(&e))?
            .ok_or_else(missing)?;
        let mut revoked = Vec::new();
        if scoped_prior(&tx, &key, &hash, client)?.is_none() {
            require_assignment(&tx, client, &previous, next)?;
            if previous != next {
                tx.execute(
                    "UPDATE conversations SET sponsor_member_id=?2 WHERE +org_id=:org_id AND conversation_id=?1",
                    params![id,next],
                ).map_err(|e| No::internal(&e))?;
                transfer_attention(&tx, id, next, client, false).map_err(|e| No::internal(&e))?;
                revoked = revoke_leads(&tx, id, None, &client.id).map_err(|e| No::internal(&e))?;
                touch(&tx, Record::Conversation, id).map_err(|e| No::internal(&e))?;
                tx.execute(
                    "INSERT INTO sponsorship_changes (org_id,conversation_id,revision,previous_member_id,
                        sponsor_member_id,actor_member_id,actor_credential_id,changed_at)
                     SELECT :org_id,conversation_id,revision,?2,?3,?4,?5,?6
                     FROM conversations WHERE +org_id=:org_id AND conversation_id=?1",
                    params![id,previous,next,client.member_id(),client.id,now_utc()],
                ).map_err(|e| No::internal(&e))?;
                crate::reservations::release(&state.store, &tx).map_err(|e| No::internal(&e))?;
                crate::turns::advance(&state.store, &tx, id).map_err(|e| No::internal(&e))?;
            }
            remember(&tx, &key, &hash, Record::Conversation, id, client)
                .map_err(|e| No::internal(&e))?;
        }
        tx.commit().map_err(|e| No::internal(&e))?;
        Ok(revoked)
    })();
    match result {
        Err(e) => e.reply(),
        Ok(revoked) => {
            for id in revoked {
                state.hub.revoked(&id);
            }
            match crate::records::one(&db, Record::Conversation, id) {
                Ok(Some(row)) => json(200, &json!({"conversation": row})),
                Ok(None) => refusal(404, "not_found", "no such conversation", Value::Null),
                Err(e) => internal(&e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    #[test]
    fn adoption_revokes_a_concurrent_mint_or_makes_it_use_the_new_sponsor() -> TestResult {
        let (_temp, state, owner, _) = crate::events::tests::fixture()?;
        state.store.db().fixture_batch(
            "INSERT INTO members (org_id,id,display_name,role,status)
             VALUES (:org_id,'former','Former','member','active');
             INSERT INTO conversations (org_id,conversation_id,node,spec,spec_version,model,
                instructions_tokens,tools,created_by,created_at,held,lead,sponsor_member_id)
             VALUES (:org_id,'conversation','node','spec','1','model',0,0,'owner','now',1,1,'former');",
        )?;
        let mut headers = hyper::HeaderMap::new();
        headers.insert("idempotency-key", "adoption-race".parse()?);
        let call = Call {
            method: "PATCH".into(),
            path: "/api/v1/client/conversations/conversation".into(),
            query: String::new(),
            headers,
            body: json!({"sponsor_member_id": owner.member_id()})
                .to_string()
                .into(),
        };
        let mint_store = Store::open(&state.store.dir)?;
        let start = Barrier::new(2);
        let minted = std::thread::scope(|threads| {
            let mint = threads.spawn(|| {
                start.wait();
                crate::access::mint_lead(&mint_store, "lead", "conversation", None, &owner.id)
            });
            start.wait();
            assert_eq!(adopt(&state, &call, &owner, "conversation").status(), 200);
            mint.join().map_err(|_| "mint thread panicked")?
        })?;
        let credential = minted["credential_id"]
            .as_str()
            .ok_or("missing credential ID")?;
        let db = state.store.db();
        let (member, revoked): (String, bool) = db.query_row(
            "SELECT member_id, revoked_at IS NOT NULL FROM credentials
             WHERE +org_id=:org_id AND id=?1",
            [credential],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if member == "former" {
            assert!(revoked);
        } else {
            assert_eq!(member, owner.member_id());
            assert!(!revoked);
        }
        Ok(())
    }

    #[test]
    fn needs_you_reroutes_and_audits_the_specific_item() -> TestResult {
        let (_temp, state, owner, _) = crate::events::tests::fixture()?;
        let mut db = state.store.db();
        db.fixture_batch(
            "INSERT INTO members (org_id,id,display_name,role,status)
             VALUES (:org_id,'former','Former','viewer','active');
             INSERT INTO intents (org_id,intent_id,node,spec,task,trials,meta,bundle_sha256,created_by,created_at)
             VALUES (:org_id,'intent','node','spec','task',1,'{}','bundle','owner','now');
             INSERT INTO task_needs_you (org_id,needs_you_id,intent_id,conversation_id,raised_at,raised_by,status,assignee_member_id)
             VALUES (:org_id,'item','intent','conversation','now','owner','open','former');",
        )?;
        let tx = write_tx(&mut db)?;
        reroute_attention(&tx, "former", &owner)?;
        tx.commit()?;
        let (item, assignee): (String, String) = db.query_row(
            "SELECT record_id, assignee_member_id FROM assignment_changes WHERE +org_id=:org_id",
            params![],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(item, "item");
        assert_eq!(assignee, owner.member_id());
        let revision: i64 = db.query_row(
            "SELECT revision FROM intents WHERE +org_id=:org_id AND intent_id='intent'",
            params![],
            |row| row.get(0),
        )?;
        assert!(revision > 0);
        Ok(())
    }
}
