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
    state.store.db().fixture_batch("
        INSERT INTO nodes (org_id, id,name,token_hash,created_at) VALUES(:org_id, 1,'home','hash1','now'),(:org_id, 2,'away','hash2','now');
        INSERT INTO intents (org_id, intent_id,node,spec,task,trials,meta,bundle_sha256,created_by,created_at)
            VALUES(:org_id, 'intent','home','s','t',1,'{}','sha','owner','now');
        INSERT INTO conversations (org_id, conversation_id,node,spec,spec_version,model,instructions_tokens,tools,intent_id,held,created_by,created_at,lead)
            VALUES(:org_id, 'conv','home','s','v','model',0,0,'intent',1,'owner','now',1);
        INSERT INTO jobs (org_id, job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at,intent_id)
            VALUES(:org_id, 'job',1,'s','t',1,'sha','{}','accepted','now','intent');
        INSERT INTO runs (org_id, run_id,node_id,usage,received_at,job_id) VALUES(:org_id, 'run',1,'{}','now','job');
    ")
}

fn lead(owner: &Client) -> Client {
    Client {
        id: "lead".into(),
        kind: Kind::Lead,
        member: owner.member.clone(),
        node: Some("home".into()),
        intent_id: Some("intent".into()),
        conversation_id: Some("conv".into()),
    }
}

#[tokio::test]
async fn browser_api_requires_an_unrevoked_local_credential() -> Res {
    let (_tmp, state, client, cookie) = crate::events::tests::fixture()?;
    let mut headers = hyper::HeaderMap::new();
    headers.insert("cookie", format!("cs_ui={cookie}").parse()?);
    let call = Call {
        method: "GET".into(),
        path: "/ui/api/jobs".into(),
        query: String::new(),
        headers,
        body: bytes::Bytes::new(),
    };
    let peer = crate::server::Peer {
        ip: std::net::Ipv4Addr::LOCALHOST.into(),
        local: std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
        tls: false,
    };
    assert_eq!(crate::ui::handle(&state, &call, peer).status(), 200);
    state.store.db().execute(
        "UPDATE credentials SET revoked_at = 'now' WHERE +org_id = :org_id AND id = ?1",
        rusqlite::params![client.id],
    )?;
    let reply = crate::ui::handle(&state, &call, peer);
    assert_eq!(reply.status(), 401);
    assert_eq!(
        payload(reply).await?,
        json!({
            "error": "Unauthorized",
            "code": "ui_login_required",
            "detail": "ui_login_required",
        })
    );
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn member_identity_is_live_on_bearer_and_browser_requests() -> Res {
    use crate::{access, ui_auth, util};
    use rusqlite::params;

    let (_tmp, state, _, _) = crate::events::tests::fixture()?;
    let store = &state.store;
    let code = ui_auth::issue(store)?;
    {
        let db = store.db();
        db.fixture_batch(
            "INSERT INTO members VALUES ('alex', :org_id, 'Alex', 'admin', 'active');
             INSERT INTO credentials (org_id, member_id, id, kind, name, token_hash, created_at)
                 VALUES (:org_id, 'alex', 'alex-local', 'local', 'Alex', 'alex-local-hash', 'then');
             INSERT INTO orgs VALUES ('foreign-org', 'Foreign');
             INSERT INTO members VALUES ('foreign-member', 'foreign-org', 'Foreign', 'owner', 'active');",
        )?;
        for (org, member, id, token) in [
            (
                db.scope().org_id.as_str(),
                "alex",
                "alex-device",
                "device-token",
            ),
            (
                "foreign-org",
                "foreign-member",
                "foreign-device",
                "foreign-token",
            ),
        ] {
            db.execute(
                "INSERT INTO credentials (org_id, member_id, id, kind, name, token_hash, created_at)
                 VALUES (?1, ?2, ?3, 'device', ?3, ?4, 'then')",
                params![org, member, id, util::sha256_hex(token.as_bytes())],
            )?;
        }
        db.execute(
            "UPDATE ui_codes SET member_id = 'alex' WHERE +org_id = :org_id",
            params![],
        )?;
    }
    let (cookie, _) = ui_auth::exchange(store, &code)?.ok_or("browser pairing failed")?;
    let browser_request = || -> Result<Reply, hyper::http::header::InvalidHeaderValue> {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("cookie", format!("cs_ui={cookie}").parse()?);
        Ok(crate::ui::handle(
            &state,
            &Call {
                method: "GET".into(),
                path: "/ui/api/jobs".into(),
                query: String::new(),
                headers,
                body: bytes::Bytes::new(),
            },
            crate::server::Peer {
                ip: std::net::Ipv4Addr::LOCALHOST.into(),
                local: std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
                tls: false,
            },
        ))
    };
    let (browser, _) = ui_auth::authenticate(store, &cookie)?.ok_or("browser auth failed")?;
    assert!(matches!(browser.kind, Kind::Browser { .. }));
    assert_eq!(browser.member_id(), "alex");
    assert_eq!(browser.member.scope.role, Some(access::Role::Admin));
    assert_eq!(browser.id, "alex-local");

    let bearer = |token: &str| -> Result<Reply, hyper::http::header::InvalidHeaderValue> {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse()?);
        Ok(crate::client_api::handle(
            &state,
            &Call {
                method: "GET".into(),
                path: "/api/v1/client/me".into(),
                query: String::new(),
                headers,
                body: bytes::Bytes::new(),
            },
        ))
    };
    assert_eq!(bearer("foreign-token")?.status(), 401);
    let admin = payload(bearer("device-token")?).await?;
    assert_eq!(
        admin["member"],
        json!({"id": "alex", "display_name": "Alex"})
    );
    assert_eq!(admin["role"], "admin");
    assert_eq!(admin["owner"], false);
    let caps = admin["capabilities"]
        .as_array()
        .ok_or("missing capabilities")?;
    for cap in ["read_work", "approve_action", "manage_director_budgets"] {
        assert!(caps.contains(&json!(cap)), "{cap}");
    }
    assert!(!caps.contains(&json!("manage_source_budgets")));

    store.db().execute(
        "UPDATE members SET role = 'viewer' WHERE +org_id = :org_id AND id = 'alex'",
        params![],
    )?;
    let viewer = payload(bearer("device-token")?).await?;
    assert_eq!(viewer["role"], "viewer");
    assert_eq!(
        viewer["capabilities"],
        json!([
            "read_work",
            "read_member_directory",
            "rename_self",
            "manage_own_devices"
        ])
    );
    let (reloaded, _) = ui_auth::authenticate(store, &cookie)?.ok_or("browser auth failed")?;
    assert_eq!(reloaded.member.scope.role, Some(access::Role::Viewer));
    assert_eq!(reloaded.kind, browser.kind);
    assert_eq!(browser_request()?.status(), 200);

    let (pairing, _) =
        access::issue_pairing(store, "second", &reloaded.id, Some(&reloaded.member))?;
    let (_, _, paired_token) =
        access::exchange_pairing(store, &pairing)?.ok_or("self-pair failed")?;
    assert_eq!(
        payload(bearer(&paired_token)?).await?["member"],
        viewer["member"]
    );

    // Unknown roles fail closed. SQLite accepts the value and authentication validates it.
    store.db().execute(
        "UPDATE members SET role = 'mistyped' WHERE +org_id = :org_id AND id = 'alex'",
        params![],
    )?;
    assert_eq!(bearer("device-token")?.status(), 401);
    store.db().execute(
        "UPDATE members SET role = 'viewer', status = 'disabled' WHERE +org_id = :org_id AND id = 'alex'",
        params![],
    )?;
    assert_eq!(bearer("device-token")?.status(), 401);
    assert_eq!(bearer(&paired_token)?.status(), 401);
    assert!(ui_auth::authenticate(store, &cookie)?.is_none());
    assert_eq!(browser_request()?.status(), 401);
    Ok(())
}

#[test]
fn delegated_lead_reloads_sponsor_but_node_keeps_machine_authority() -> Res {
    use crate::access;
    use rusqlite::params;

    let (_tmp, state, _, _) = crate::events::tests::fixture()?;
    let store = &state.store;
    seed(&state)?;
    let node_token = access::register_node(store, "registered")?;
    store.db().fixture_batch(
        "INSERT INTO members VALUES ('alex', :org_id, 'Alex', 'member', 'active');
         UPDATE conversations SET responsible_member_id = 'alex' WHERE +org_id = :org_id;
         UPDATE nodes SET registered_by = 'alex' WHERE +org_id = :org_id AND name = 'registered';",
    )?;
    let minted = access::mint_lead(store, "lead", "conv", Some("intent"), "legacy-actor")?;
    let token = minted["token"].as_str().ok_or("missing lead token")?;
    let lead = access::client(store, token)?.ok_or("lead auth failed")?;
    assert_eq!(lead.kind, Kind::Lead);
    assert_eq!(lead.member_id(), "alex");
    assert_eq!(lead.member.scope.role, Some(access::Role::Member));
    assert_eq!(lead.intent_id.as_deref(), Some("intent"));
    store.db().execute(
        "UPDATE members SET role = 'viewer' WHERE +org_id = :org_id AND id = 'alex'",
        params![],
    )?;
    let lead = access::client(store, token)?.ok_or("lead auth failed after a role change")?;
    assert_eq!(lead.member.scope.role, Some(access::Role::Viewer));
    store.db().execute(
        "UPDATE members SET status = 'disabled' WHERE +org_id = :org_id AND id = 'alex'",
        params![],
    )?;
    assert!(access::client(store, token)?.is_none());
    assert!(access::client(store, &node_token)?.is_none());
    let node = access::node(store, &node_token)?.ok_or("node lost machine authority")?;
    assert_eq!(node.name, "registered");
    assert!(!node.registered_by.can_authenticate());
    assert_eq!(node.registered_by.scope.role, Some(access::Role::Viewer));
    Ok(())
}

#[tokio::test]
async fn thread_returns_more_than_500_turns() -> Res {
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    state.store.db().fixture_batch(
        "
        WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM nums WHERE n<501)
        INSERT INTO turns(org_id, turn_id,conversation_id,seq,message,state,posted_by,posted_at)
        SELECT :org_id, 'turn'||n,'conv',n,'message','done','owner','now' FROM nums;
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
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    state.store.db().fixture_batch("
        CREATE TEMP TABLE nums AS WITH RECURSIVE seq(n) AS
          (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<500) SELECT n FROM seq;
        INSERT INTO intents(org_id, intent_id,node,spec,task,trials,meta,bundle_sha256,created_by,created_at)
          SELECT :org_id, 'i'||n,CASE WHEN n%2=0 THEN 'away' ELSE 'home' END,'s','t',1,'{}','sha','other','now' FROM nums;
        INSERT INTO jobs(org_id, job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at,intent_id)
          SELECT :org_id, 'j'||n,1+n%2,'s','t',1,'sha','{}','accepted','now','i'||n FROM nums;
        INSERT INTO runs(org_id, run_id,node_id,usage,received_at,job_id)
          SELECT :org_id, 'r'||n,1+n%2,'{}','now','j'||n FROM nums;
    ")?;
    let mut listed = Vec::new();
    for (kind, key) in [
        ("jobs", "job_id"),
        ("runs", "run_id"),
        ("intents", "intent_id"),
    ] {
        let value = payload(call(&state, &lead(&client), "GET", kind, &Value::Null)).await?;
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
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    let reply = call(
        &state,
        &lead(&client),
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
    state.store.db().fixture_batch("
        UPDATE conversations SET attention_question='Question' WHERE conversation_id='conv';
        INSERT INTO task_needs_you (org_id, needs_you_id,intent_id,conversation_id,raised_at,raised_by,status)
            VALUES(:org_id, 'attention','intent','conv','now','lead','open');
    ")
}

fn attention_status(state: &State) -> rusqlite::Result<String> {
    state.store.db().query_row(
        "SELECT status FROM task_needs_you WHERE needs_you_id='attention'",
        rusqlite::params![],
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
        rusqlite::params![],
        |r| r.get(0),
    )?;
    assert!(revision > 0);
    Ok(())
}

#[tokio::test]
async fn approval_list_prioritizes_owner_queue_and_limits_after_lead_scope() -> Res {
    let (_tmp, state, client, _) = crate::events::tests::fixture()?;
    seed(&state)?;
    state.store.db().fixture_batch(
        "
        INSERT INTO jobs (org_id, job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at)
            VALUES(:org_id, 'foreign',2,'s','t',1,'sha','{}','accepted','now');
        INSERT INTO action_approvals (org_id, id,job_id,run_id,request,status,created_at)
            VALUES(:org_id, 'own','job','run','{}','needs_owner','2000'),
                  (:org_id, 'own_history','job','old-run','{}','approved','1999');
        WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM nums WHERE n<501)
        INSERT INTO action_approvals(org_id, id,job_id,run_id,request,status,created_at)
            SELECT :org_id, 'a'||n,'foreign','r'||n,'{}','approved','2001' FROM nums;
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
        &lead(&client),
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
    state.store.db().fixture_batch(
        "
         INSERT INTO turns (org_id, turn_id,conversation_id,seq,message,state,posted_by,posted_at) VALUES
         (:org_id, 'running','conv',1,'First','running','owner','2000'),
         (:org_id, 'pending','conv',2,'Next','queued','owner','2000'),
         (:org_id, 'later','conv',3,'Later','queued','owner','2000');
    ",
    )?;
    let reply = call(
        &state,
        &lead(&client),
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
        "INSERT INTO action_approvals (org_id, id,job_id,run_id,request,status,created_at)
            VALUES(:org_id, 'approval','job','run','{}','approved','now')",
        rusqlite::params![],
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
        rusqlite::params![],
        |r| r.get(0),
    )?;
    assert!(revision > 0);
    Ok(())
}
