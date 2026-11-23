//! Client regressions using a real store and in-process request dispatch.
use crate::access::{Client, Kind};
use crate::server::{Call, Reply, State};
use http_body_util::BodyExt;
use serde_json::{Value, json};

type Res = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn call(
    state: &std::sync::Arc<State>,
    client: &Client,
    method: &str,
    path: &str,
    body: &Value,
) -> Reply {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        "idempotency-key",
        hyper::header::HeaderValue::from_static("review-regression-key"),
    );
    crate::client_api::handle_as(
        state,
        &Call {
            method: method.into(),
            path: format!("/api/v1/client/{path}"),
            query: String::new(),
            headers,
            body: body.to_string().into(),
        },
        client,
    )
}

async fn payload(reply: Reply) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    Ok(serde_json::from_slice(
        &reply.into_body().collect().await?.to_bytes(),
    )?)
}

fn seed(state: &State) -> rusqlite::Result<()> {
    state.store.db().execute_batch("
        INSERT INTO nodes(id,name,token_hash,created_at) VALUES(1,'home','hash1','now'),(2,'away','hash2','now');
        INSERT INTO intents(intent_id,node,spec,task,trials,meta,bundle_sha256,created_by,created_at)
            VALUES('intent','home','s','t',1,'{}','sha','owner','now');
        INSERT INTO conversations(conversation_id,node,spec,spec_version,model,instructions_tokens,tools,intent_id,held,created_by,created_at,lead)
            VALUES('conv','home','s','v','model',0,0,'intent',1,'owner','now',1);
        INSERT INTO jobs(job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at,intent_id)
            VALUES('job',1,'s','t',1,'sha','{}','accepted','now','intent');
        INSERT INTO runs(run_id,node_id,usage,received_at,job_id) VALUES('run',1,'{}','now','job');
    ")
}

fn lead() -> Client {
    Client {
        id: "lead".into(),
        kind: Kind::Lead,
        node: Some("home".into()),
        intent_id: Some("intent".into()),
        conversation_id: Some("conv".into()),
    }
}

#[tokio::test]
async fn thread_returns_more_than_500_turns() -> Res {
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    state.store.db().execute_batch(
        "
        WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM nums WHERE n<501)
        INSERT INTO turns(turn_id,conversation_id,seq,message,state,posted_by,posted_at)
        SELECT 'turn'||n,'conv',n,'message','done','owner','now' FROM nums;
    ",
    )?;
    let thread = payload(call(
        &state,
        &client,
        "GET",
        "conversations/conv/thread",
        &Value::Null,
    ))
    .await?;
    assert_eq!(thread["turns"].as_array().map(Vec::len), Some(501));
    assert_eq!(thread["turns"][500]["seq"], 501);
    Ok(())
}

#[tokio::test]
async fn lead_work_scope_precedes_list_limits() -> Res {
    let (_tmp, state, _, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    state.store.db().execute_batch("
        CREATE TEMP TABLE nums AS WITH RECURSIVE seq(n) AS
          (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<500) SELECT n FROM seq;
        INSERT INTO intents(intent_id,node,spec,task,trials,meta,bundle_sha256,created_by,created_at)
          SELECT 'i'||n,CASE WHEN n%2=0 THEN 'away' ELSE 'home' END,'s','t',1,'{}','sha','other','now' FROM nums;
        INSERT INTO jobs(job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at,intent_id)
          SELECT 'j'||n,1+n%2,'s','t',1,'sha','{}','accepted','now','i'||n FROM nums;
        INSERT INTO runs(run_id,node_id,usage,received_at,job_id)
          SELECT 'r'||n,1+n%2,'{}','now','j'||n FROM nums;
    ")?;
    let mut listed = Vec::new();
    for (kind, key) in [
        ("jobs", "job_id"),
        ("runs", "run_id"),
        ("intents", "intent_id"),
    ] {
        let value = payload(call(&state, &lead(), "GET", kind, &Value::Null)).await?;
        let rows = value[kind].as_array().ok_or("missing list")?;
        listed.push(
            rows.iter()
                .map(|row| row[key].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(listed, [vec!["job"], vec!["run"], vec!["intent"]]);
    Ok(())
}

#[tokio::test]
async fn lead_cannot_submit_to_another_project() -> Res {
    let (_tmp, state, _, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    let reply = call(
        &state,
        &lead(),
        "POST",
        "intents",
        &json!({
            "node":"home","project_id":"other","spec":"s","task":"t","files":[]
        }),
    );
    assert_eq!(reply.status(), 403);
    assert_eq!(payload(reply).await?["code"], "owner_only");
    Ok(())
}

fn open_attention(state: &State) -> rusqlite::Result<()> {
    state.store.db().execute_batch("
        UPDATE conversations SET attention_question='Question' WHERE conversation_id='conv';
        INSERT INTO task_needs_you(needs_you_id,intent_id,conversation_id,raised_at,raised_by,status)
            VALUES('attention','intent','conv','now','lead','open');
    ")
}

fn attention_status(state: &State) -> rusqlite::Result<String> {
    state.store.db().query_row(
        "SELECT status FROM task_needs_you WHERE needs_you_id='attention'",
        [],
        |r| r.get(0),
    )
}

#[test]
fn device_turn_clears_work_trace_attention() -> Res {
    let (_tmp, state, mut client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    open_attention(&state)?;
    client.kind = Kind::Device;
    assert_eq!(
        call(
            &state,
            &client,
            "POST",
            "conversations/conv/turns",
            &json!({"message":"Answer"})
        )
        .status(),
        201
    );
    assert_eq!(attention_status(&state)?, "cleared");
    Ok(())
}

#[test]
fn deleting_conversation_clears_work_trace_attention() -> Res {
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    open_attention(&state)?;
    assert_eq!(
        call(
            &state,
            &client,
            "DELETE",
            "conversations/conv",
            &Value::Null
        )
        .status(),
        200
    );
    assert_eq!(attention_status(&state)?, "cleared");
    Ok(())
}

#[test]
fn deleting_conversation_touches_attached_intent() -> Res {
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    assert_eq!(
        call(
            &state,
            &client,
            "DELETE",
            "conversations/conv",
            &Value::Null
        )
        .status(),
        200
    );
    let revision: i64 = state.store.db().query_row(
        "SELECT revision FROM intents WHERE intent_id='intent'",
        [],
        |r| r.get(0),
    )?;
    assert!(revision > 0);
    Ok(())
}

#[tokio::test]
async fn approval_list_prioritizes_owner_queue_and_limits_after_lead_scope() -> Res {
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    state.store.db().execute_batch(
        "
        INSERT INTO jobs(job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at)
            VALUES('foreign',2,'s','t',1,'sha','{}','accepted','now');
        INSERT INTO action_approvals(id,job_id,run_id,request,status,created_at)
            VALUES('own','job','run','{}','needs_owner','2000'),
                  ('own_history','job','old-run','{}','approved','1999');
        WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM nums WHERE n<501)
        INSERT INTO action_approvals(id,job_id,run_id,request,status,created_at)
            SELECT 'a'||n,'foreign','r'||n,'{}','approved','2001' FROM nums;
    ",
    )?;
    let own = payload(call(
        &state,
        &client,
        "GET",
        "action-approvals",
        &Value::Null,
    ))
    .await?;
    let scoped = payload(call(
        &state,
        &lead(),
        "GET",
        "action-approvals",
        &Value::Null,
    ))
    .await?;
    assert_eq!(
        (
            own["action_approvals"][0]["id"].as_str(),
            own["action_approvals"].as_array().map(Vec::len),
            scoped["action_approvals"].as_array().map(Vec::len)
        ),
        (Some("own"), Some(500), Some(2))
    );
    assert_eq!(scoped["action_approvals"][0]["id"], "own");
    assert_eq!(scoped["action_approvals"][1]["id"], "own_history");
    Ok(())
}

#[tokio::test]
async fn wake_reply_precedes_pending_turns_in_thread_order() -> Res {
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    state.store.db().execute_batch(
        "
         INSERT INTO turns(turn_id,conversation_id,seq,message,state,posted_by,posted_at) VALUES
         ('running','conv',1,'First','running','owner','2000'),
         ('pending','conv',2,'Next','queued','owner','2000'),
         ('later','conv',3,'Later','queued','owner','2000');
    ",
    )?;
    let reply = call(
        &state,
        &lead(),
        "POST",
        "conversations/conv/wake-turns",
        &json!({"nudge":"Check","reply":"Woke"}),
    );
    assert_eq!(reply.status(), 201);
    let thread = payload(call(
        &state,
        &client,
        "GET",
        "conversations/conv/thread",
        &Value::Null,
    ))
    .await?;
    let turns = thread["turns"].as_array().ok_or("missing turns")?;
    assert_eq!(turns[0]["turn_id"], "running");
    assert_eq!(turns[1]["reply"], "Woke");
    assert_eq!(turns[2]["turn_id"], "pending");
    assert_eq!(turns[3]["turn_id"], "later");
    Ok(())
}

#[test]
fn claiming_approval_touches_revision() -> Res {
    let (_tmp, state, _, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    let db = state.store.db();
    db.execute(
        "INSERT INTO action_approvals(id,job_id,run_id,request,status,created_at)
        VALUES('approval','job','run','{}','approved','now')",
        [],
    )?;
    drop(db);
    let claim = Call {
        method: "POST".into(),
        path: String::new(),
        query: String::new(),
        headers: hyper::HeaderMap::new(),
        body: r#"{"run_id":"run"}"#.into(),
    };
    assert_eq!(
        crate::action_approvals::node(&state, &claim, 1, "job", &["approval", "claim"]).status(),
        200
    );
    let revision: i64 = state.store.db().query_row(
        "SELECT revision FROM action_approvals WHERE id='approval'",
        [],
        |r| r.get(0),
    )?;
    assert!(revision > 0);
    Ok(())
}
