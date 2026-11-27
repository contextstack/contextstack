use ring::hmac;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::store::{Record, ScopedConnection, touch};

pub(super) const REASON: &str =
    "This browser action requires the owner's approval before it expires.";
pub(super) const EXPIRED: &str = "The browser action proposal expired.";

fn digest(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        s.len() == 64
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

fn identity(input: &Value) -> Option<Value> {
    let origin = input["origin"].as_str()?;
    let url = url::Url::parse(origin).ok()?;
    if url.scheme() != "https" || url.origin().ascii_serialization() != origin {
        return None;
    }
    let action = input["action"].as_str()?;
    if !matches!(action, "submit" | "post" | "buy") {
        return None;
    }
    let target = input["target"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 2048 && !s.chars().any(char::is_control))?;
    Some(json!({
        "origin": origin, "action": action, "target": target
    }))
}

pub(super) fn binding(input: &Value) -> Option<Value> {
    let mut binding = identity(input)?;
    binding["field_values_digest"] = json!(digest(&input["field_values_digest"])?);
    Some(binding)
}
pub(super) fn request(input: &Value, ttl_seconds: i64) -> Option<Value> {
    let mut request = binding(input)?;
    request["kind"] = json!("browser_action");
    request["screenshot_sha"] = json!(digest(&input["screenshot_sha"])?);
    let summary = input["values_summary"].as_array()?;
    if input["values_summary"].to_string().len() > 256 * 1024
        || summary.iter().any(|field| {
            field["name"].as_str().is_none_or(str::is_empty)
                || !field["hidden"].is_boolean()
                || (field["value"].is_string() == field["length"].is_u64())
                || field
                    .get("excluded")
                    .is_some_and(|excluded| excluded != true || field["hidden"] != true)
        })
    {
        return None;
    }
    request["values_summary"] = input["values_summary"].clone();
    add_warning(&mut request);
    let captured = chrono::DateTime::parse_from_rfc3339(input["captured_at"].as_str()?).ok()?;
    request["expires_at"] = json!(
        captured
            .checked_add_signed(chrono::TimeDelta::seconds(ttl_seconds))?
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    Some(request)
}

pub(super) fn screenshot(
    state: &crate::server::State,
    db: &ScopedConnection,
    sha: &str,
) -> crate::server::Reply {
    if !crate::util::is_sha256(sha) {
        return super::missing();
    }
    let listed = db
        .prepare(
            "SELECT 1 FROM action_approvals WHERE +org_id=:org_id
         AND json_extract(request,'$.kind')='browser_action'
         AND json_type(request,'$.values_summary')='array'
         AND json_extract(request,'$.screenshot_sha')=?1",
        )
        .and_then(|mut statement| statement.exists([sha]));
    match listed {
        Ok(true) => match std::fs::read(state.store.dir.join("browser-screenshots").join(sha)) {
            Ok(data) => {
                let mut reply = crate::server::bytes(200, "image/jpeg", data.into());
                reply.headers_mut().insert(
                    "cache-control",
                    hyper::header::HeaderValue::from_static("private, no-store"),
                );
                reply
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => super::missing(),
            Err(error) => crate::server::internal(&error),
        },
        Ok(false) => super::missing(),
        Err(error) => crate::server::internal(&error),
    }
}

pub(super) fn verified_capture(body: &Value, token: &str, job: &str, run: &str) -> Option<Vec<u8>> {
    let capture = &body["capture"];
    if capture["job_id"] != job || capture["run_id"] != run {
        return None;
    }
    let captured = chrono::DateTime::parse_from_rfc3339(capture["captured_at"].as_str()?).ok()?;
    let age = chrono::Utc::now()
        .signed_duration_since(captured)
        .num_seconds();
    if !(0..300).contains(&age) {
        return None;
    }
    let proof = body["capture_proof"]
        .as_str()
        .filter(|proof| proof.len() == 64)?;
    let wire = json!(["contextstack-browser-capture-v1", capture]).to_string();
    hmac::verify(
        &action_key(token),
        wire.as_bytes(),
        &hex::decode(proof).ok()?,
    )
    .ok()?;
    let image = body["image"]
        .as_str()
        .filter(|image| image.len() <= 6 * 1024 * 1024)?;
    let bytes = hex::decode(image).ok()?;
    if !bytes.starts_with(&[0xff, 0xd8, 0xff])
        || crate::util::sha256_hex(&bytes) != digest(&capture["screenshot_sha"])?
    {
        return None;
    }
    Some(bytes)
}

pub(super) fn declared(
    store: &crate::store::Store,
    db: &ScopedConnection,
    job: &str,
    request: &Value,
    input: &Value,
) -> rusqlite::Result<Option<Value>> {
    let spec: Option<String> = db.query_row(
        "SELECT (SELECT sha256 FROM job_files WHERE +org_id=:org_id AND job_id=?1 AND path=j.spec)
         FROM (SELECT * FROM jobs WHERE +org_id=:org_id) j WHERE j.job_id=?1",
        [job], |row| row.get(0),
    ).optional()?.flatten();
    let Some(bytes) = spec.and_then(|sha| store.read_object(&sha)) else {
        return Ok(None);
    };
    let Ok(spec) = serde_yaml::from_slice::<Value>(&bytes) else {
        return Ok(None);
    };
    let Some(page) = input["page_url"]
        .as_str()
        .and_then(|page| url::Url::parse(page).ok())
        .filter(|page| {
            page.scheme() == "https"
                && page.username().is_empty()
                && page.password().is_none()
                && request["origin"] == page.origin().ascii_serialization()
        })
    else {
        return Ok(None);
    };
    let Some(set) = input["origin_set"].as_str().filter(|set| {
        !set.is_empty()
            && set.len() <= 80
            && set
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }) else {
        return Ok(None);
    };
    let action = request["action"].as_str().unwrap_or_default();
    let browser = &spec["browser"];
    let allowed = set != "google-tasks"
        && browser["origins"]
            .as_array()
            .is_some_and(|origins| origins.contains(&request["origin"]))
        && browser["actions"][action]
            .as_array()
            .is_some_and(|actions| {
                actions.iter().any(|entry| {
                    entry["origin"] == request["origin"] && entry["form"] == request["target"]
                })
            });
    Ok(allowed.then(|| json!({"page_url":page.as_str(),"origin_set":set})))
}

pub(super) fn preflight(
    store: &crate::store::Store,
    db: &ScopedConnection,
    job: &str,
    input: &Value,
) -> crate::server::Reply {
    let origin = input["url"]
        .as_str()
        .and_then(|page| url::Url::parse(page).ok())
        .map(|page| page.origin().ascii_serialization());
    let request = json!({"origin":origin,"action":input["action"],"target":input["target"]});
    let mut input = input.clone();
    input["page_url"] = input["url"].clone();
    if identity(&request).is_none() {
        return super::bad("invalid browser action");
    }
    match declared(store, db, job, &request, &input) {
        Ok(Some(_)) => crate::server::json(200, &json!({"ok":true})),
        Ok(None) => super::bad("the job must declare this browser action"),
        Err(error) => crate::server::internal(&error),
    }
}

pub(crate) fn can_clear(db: &ScopedConnection, id: &str) -> rusqlite::Result<bool> {
    db.prepare(
        "SELECT 1 FROM action_approvals WHERE +org_id=:org_id AND id=?1
         AND json_extract(request,'$.kind')='browser_action'
         AND status IN ('needs_owner','approved') AND claimed_at IS NULL",
    )?
    .exists([id])
}

pub(super) fn clear(
    db: &mut ScopedConnection,
    id: &str,
    client: &crate::access::Client,
) -> crate::server::Reply {
    let result = (|| {
        let tx = crate::store::write_tx(db)?;
        let now = crate::util::now_utc();
        let changed = tx.execute(
            "UPDATE action_approvals SET status='denied', decided_by=?2, decided_at=?3,
             outcome=json_object('status','denied','reason','Browser action cleared by the owner.')
             WHERE +org_id=:org_id AND id=?1 AND status IN ('needs_owner','approved')
                AND claimed_at IS NULL",
            params![id, client.member_id(), now],
        )?;
        if changed > 0 {
            let row = super::record(&tx, id)?.unwrap_or_default();
            if let Some(job) = row["followup_job_id"].as_str() {
                tx.execute(
                    "UPDATE jobs SET state='error',finished_at=?2,error='browser action cleared'
                     WHERE +org_id=:org_id AND job_id=?1 AND state='waiting'",
                    params![job, now],
                )?;
                touch(&tx, Record::Job, job)?;
                crate::control::settle_on_terminal(&tx, job)?;
            }
            touch(&tx, Record::ActionApproval, id)?;
            super::touch_conversations(&tx, id)?;
        }
        tx.commit()?;
        Ok::<_, rusqlite::Error>(changed > 0)
    })();
    match result {
        Ok(true) => super::client_detail(db, id, client),
        Ok(false) => super::conflict(),
        Err(error) => crate::server::internal(&error),
    }
}

pub(super) fn enqueue(db: &ScopedConnection, approval: &str) -> rusqlite::Result<()> {
    let scheduled = db.query_row(
        "SELECT json_type(request,'$.execution') IS 'object' FROM action_approvals
         WHERE +org_id=:org_id AND id=?1",
        [approval],
        |row| row.get::<_, bool>(0),
    )?;
    if !scheduled {
        return Ok(());
    }
    let followup = crate::util::ulid()
        .map_err(std::io::Error::other)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let inserted = db.execute(
        "INSERT INTO jobs (org_id,job_id,node_id,spec,task,trials,bundle_sha256,meta,
            state,created_at,intent_id,responsible_member_id)
         SELECT :org_id,?2,j.node_id,j.spec,j.task,1,j.bundle_sha256,
            json_object('kind','browser_action','approval_id',?1,'schedule_id',json_extract(j.meta,'$.schedule_id')),
            'waiting',?3,j.intent_id,a.assigned_member_id
         FROM (SELECT * FROM action_approvals WHERE +org_id=:org_id) a
         JOIN (SELECT * FROM jobs WHERE +org_id=:org_id) j ON j.job_id=a.job_id
         WHERE a.id=?1 AND a.status='approved' AND json_type(a.request,'$.execution')='object'",
        params![approval,followup,crate::util::now_utc()],
    )?;
    if inserted == 1 {
        db.execute(
            "UPDATE action_approvals SET request=json_set(request,'$.followup_job_id',?2)
             WHERE +org_id=:org_id AND id=?1",
            params![approval, followup],
        )?;
        touch(db, Record::Job, &followup)?;
    }
    Ok(())
}

pub(super) fn jobs(db: &ScopedConnection, node: i64) -> rusqlite::Result<Vec<Value>> {
    db.prepare(
        "SELECT a.id,j.job_id,a.request FROM (SELECT * FROM action_approvals WHERE +org_id=:org_id) a
         JOIN (SELECT * FROM jobs WHERE +org_id=:org_id) j ON j.job_id=json_extract(a.request,'$.followup_job_id')
         WHERE j.node_id=?1 AND j.state='waiting' AND a.status='approved' AND a.claimed_at IS NULL
           AND a.decided_at>?2 ORDER BY a.decided_at,a.id LIMIT 50",
    )?
    .query_map(params![node,crate::util::utc_in(-300)], |row| {
        let mut request: Value = serde_json::from_str(&row.get::<_, String>(2)?).unwrap_or_default();
        request["approval_id"] = json!(row.get::<_, String>(0)?);
        request["job_id"] = json!(row.get::<_, String>(1)?);
        Ok(request)
    })?.collect()
}

pub(super) fn expire_jobs(db: &ScopedConnection) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    let ids = tx.prepare(
        "UPDATE jobs SET state='error',finished_at=?1,
                 error=CASE WHEN state='accepted' THEN 'browser action outcome unknown' ELSE 'browser action approval expired' END
         WHERE +org_id=:org_id AND json_extract(meta,'$.kind')='browser_action'
           AND EXISTS (SELECT 1 FROM action_approvals WHERE +org_id=:org_id
              AND json_extract(request,'$.followup_job_id')=jobs.job_id)
           AND ((state='waiting' AND created_at<?2) OR (state='accepted' AND accepted_at<?2))
         RETURNING job_id",
    )?
    .query_map(params![crate::util::now_utc(),crate::util::utc_in(-300)], |row| row.get::<_, String>(0))?
    .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        touch(&tx, Record::Job, &id)?;
        crate::control::settle_on_terminal(&tx, &id)?;
    }
    tx.commit()
}

pub(super) fn finish_job(db: &ScopedConnection, id: &str, outcome: &str) -> rusqlite::Result<()> {
    let job: Option<String> = db.query_row(
        "SELECT json_extract(request,'$.followup_job_id') FROM action_approvals
         WHERE +org_id=:org_id AND id=?1",
        [id],
        |row| row.get(0),
    )?;
    if let Some(job) = job {
        let result: Value = serde_json::from_str(outcome).unwrap_or_default();
        let (state, stop) = match result["status"].as_str() {
            Some("succeeded" | "submitted") => ("done", "browser_action"),
            Some("submission_unknown") => ("done", "browser_action_unknown"),
            _ => ("error", "browser_action"),
        };
        db.execute(
            "UPDATE jobs SET state=?2,finished_at=?3,terminal_row=json_object('state',?2,'stop',?4)
             WHERE +org_id=:org_id AND job_id=?1 AND state='accepted'",
            params![job, state, crate::util::now_utc(), stop],
        )?;
        touch(db, Record::Job, &job)?;
        crate::control::settle_on_terminal(db, &job)?;
    }
    Ok(())
}

pub(super) fn claim(
    db: &ScopedConnection,
    id: &str,
    input: &Value,
    request: &Value,
) -> rusqlite::Result<bool> {
    let presented = binding(input);
    if !request["values_summary"].is_array() || presented.is_none() || presented != binding(request)
    {
        return Ok(false);
    }
    let tx = db.unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE action_approvals SET claimed_at=?2,outcome=json_object('status','unknown')
         WHERE +org_id=:org_id AND id=?1 AND status='approved' AND claimed_at IS NULL
           AND decided_at > ?3",
        params![id, crate::util::now_utc(), crate::util::utc_in(-300)],
    )? == 1;
    if changed {
        if let Some(job) = request["followup_job_id"].as_str() {
            let accepted = tx.execute(
                "UPDATE jobs SET state='accepted',accepted_at=?2
                 WHERE +org_id=:org_id AND job_id=?1 AND state='waiting'
                   AND NOT EXISTS (SELECT 1 FROM cancels WHERE +org_id=:org_id
                    AND job_id=?1 AND state='requested')",
                params![job, crate::util::now_utc()],
            )?;
            if accepted != 1 {
                return Ok(false);
            }
            touch(&tx, Record::Job, job)?;
        }
        touch(&tx, Record::ActionApproval, id)?;
    }
    tx.commit()?;
    Ok(changed)
}

pub(super) fn add_warning(record: &mut Value) {
    let Some(summary) = record["values_summary"].as_array() else {
        return;
    };
    let count = summary
        .iter()
        .filter(|field| field["hidden"] == true || field["length"].is_u64())
        .count();
    record["has_field_warning"] = json!(count != 0);
    record["masked_or_hidden_count"] = json!(count);
}

pub(super) fn project(item: &mut Value, request: &Value) {
    if request["kind"] == "browser_action" {
        for key in [
            "kind",
            "origin",
            "action",
            "target",
            "screenshot_sha",
            "values_summary",
            "expires_at",
        ] {
            item[key] = request[key].clone();
        }
        add_warning(item);
    }
}

fn action_key(node_token: &str) -> hmac::Key {
    let token = hmac::Key::new(hmac::HMAC_SHA256, node_token.as_bytes());
    let derived = hmac::sign(&token, b"contextstack-browser-action-key-v1");
    hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref())
}

fn sign_claim(record: &Value, node_token: &str) -> String {
    let fields = json!([
        "contextstack-browser-action-v1",
        record["id"],
        record["origin"],
        record["action"],
        record["target"],
        record["field_values_digest"],
        record["screenshot_sha"],
        record["decided_at"],
        record["claimed_at"]
    ]);
    let key = action_key(node_token);
    hex::encode(ring::hmac::sign(&key, fields.to_string().as_bytes()).as_ref())
}

pub(super) fn claim_reply(
    db: &ScopedConnection,
    id: &str,
    body: &Value,
    request: &Value,
    node_token: Option<&str>,
) -> crate::server::Reply {
    let result = claim(db, id, body, request);
    crate::logf!(
        "browser action approval {id}: claim accepted={}",
        matches!(result, Ok(true))
    );
    match result {
        Ok(true) => match super::record(db, id) {
            Ok(Some(mut row)) => {
                if let Some(token) = node_token {
                    row["claim_proof"] = json!(sign_claim(&row, token));
                }
                crate::server::json(200, &row)
            }
            Ok(None) => super::missing(),
            Err(error) => crate::server::internal(&error),
        },
        Ok(false) => super::conflict(),
        Err(error) => crate::server::internal(&error),
    }
}
