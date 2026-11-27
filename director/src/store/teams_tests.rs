use super::*;

type Res = Result<(), Box<dyn std::error::Error>>;

#[test]
fn v13_sponsorship_upgrade_keeps_data_and_never_lowers_version() -> Res {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("data");
    drop(Store::open(&dir)?);
    let mut db = RawConnection::open(dir.join("director.sqlite"))?;
    let scope = teams::bootstrap(&db)?;
    db.execute_batch(
        "ALTER TABLE conversations DROP COLUMN sponsor_member_id;
         ALTER TABLE conversations DROP COLUMN attention_assignee_member_id;
         ALTER TABLE task_needs_you DROP COLUMN assignee_member_id;
         ALTER TABLE backlog_items DROP COLUMN assignee_member_id;
         DROP TABLE sponsorship_changes;
         DROP TABLE assignment_changes;
         PRAGMA user_version=13;",
    )?;
    db.execute(
        "INSERT INTO conversations (org_id,conversation_id,node,spec,spec_version,model,
          instructions_tokens,tools,created_by,created_at,lead,responsible_member_id,attention_question)
         VALUES (?1,'legacy','machine','spec.yaml','version','model',0,0,'credential','now',1,?2,'Review?')",
        params![scope.org_id,scope.member_id],
    )?;
    db.execute(
        "INSERT INTO intents (org_id,intent_id,node,spec,task,trials,meta,bundle_sha256,created_by,created_at)
         VALUES (?1,'legacy','machine','spec.yaml','task',1,'{}','bundle','credential','now')",
        [&scope.org_id],
    )?;
    db.execute(
        "INSERT INTO task_needs_you (org_id,needs_you_id,intent_id,conversation_id,raised_at,raised_by,status)
         VALUES (?1,'needs','legacy','legacy','now','credential','open')",
        [&scope.org_id],
    )?;
    db.execute(
        "INSERT INTO projects (org_id,project_id,name,created_by,created_at)
         VALUES (?1,'project','Legacy','credential','now')",
        [&scope.org_id],
    )?;
    db.execute(
        "INSERT INTO backlog_items (org_id,item_id,project_id,status,priority,data,created_at,created_by,updated_at,updated_by)
         VALUES (?1,'backlog','project','blocked',1,json_object('blocked_on',json_array(json_object('kind','owner'))),
                 'now','credential','now','credential')",
        [&scope.org_id],
    )?;
    db.execute(
        "INSERT INTO nodes (org_id,id,name,token_hash,created_at)
         VALUES (?1,1,'machine','hash','now')",
        [&scope.org_id],
    )?;
    db.execute(
        "INSERT INTO action_approvals (org_id,id,conversation_id,node_id,request,created_at)
         VALUES (?1,'approval','legacy',1,'{}','now')",
        [&scope.org_id],
    )?;
    teams::migrate_sponsorship(&mut db)?;
    let schema_version =
        |db: &RawConnection| db.pragma_query_value(None, "schema_version", |r| r.get::<_, u32>(0));
    let revision = schema_version(&db)?;
    let read = |db: &RawConnection| {
        db.query_row(
            "SELECT sponsor_member_id,attention_assignee_member_id,attention_question
             FROM conversations WHERE conversation_id='legacy'",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
    };
    assert_eq!(user_version(&db)?, 14);
    assert_eq!(
        read(&db)?,
        (
            scope.member_id.clone(),
            scope.member_id.clone(),
            "Review?".into()
        )
    );
    for (table, column) in [
        ("task_needs_you", "assignee_member_id"),
        ("backlog_items", "assignee_member_id"),
        ("action_approvals", "assigned_member_id"),
    ] {
        let assigned: String =
            db.query_row(&format!("SELECT {column} FROM {table}"), [], |r| r.get(0))?;
        assert_eq!(assigned, scope.member_id, "{table}");
    }
    db.execute("UPDATE conversations SET attention_assignee_member_id='retained' WHERE conversation_id='legacy'", [])?;
    teams::migrate_sponsorship(&mut db)?;
    assert_eq!(user_version(&db)?, 14);
    assert_eq!(schema_version(&db)?, revision);
    assert_eq!(read(&db)?.1, "retained");
    db.pragma_update(None, "user_version", 15)?;
    teams::migrate_sponsorship(&mut db)?;
    assert_eq!(user_version(&db)?, 15);
    assert_eq!(schema_version(&db)?, revision);
    Ok(())
}

#[test]
fn fresh_store_has_work_attribution() -> Res {
    let temp = tempfile::tempdir()?;
    let store = Store::open(&temp.path().join("data"))?;
    let db = store.db();
    for table in ["intents", "jobs", "conversations", "schedules"] {
        let columns: i64 = db.raw().query_row(
            "SELECT count(*) FROM pragma_table_info(?1) WHERE name = 'responsible_member_id'",
            [table],
            |row| row.get(0),
        )?;
        assert_eq!(columns, 1, "{table}");
    }
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn v10_upgrade_keeps_live_identity_and_in_flight_work_across_restart() -> Res {
    use crate::{access, ui_auth, util};

    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("data");
    let mut db = v9_fixture(&dir)?;
    let bootstrap = teams::migrate(&mut db, None)?;
    assert_eq!(user_version(&db)?, teams::VERSION);
    let token_hash = |token: &str| sha256_hex(token.as_bytes());
    let local = "fixture-credential";
    let cookie = "a".repeat(64);
    let code = "b".repeat(64);
    let future = chrono::Utc::now().timestamp() + 300;
    db.execute(
        "UPDATE credentials SET token_hash = ?1 WHERE id = 'owner'",
        [token_hash(local)],
    )?;
    db.execute(
        "UPDATE nodes SET token_hash = ?1 WHERE id = 7",
        [token_hash("old-node")],
    )?;
    db.execute("DELETE FROM ui_sessions", [])?;
    db.execute("DELETE FROM ui_codes", [])?;
    db.execute("INSERT INTO ui_sessions (org_id, member_id, token_hash, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![bootstrap.org_id, bootstrap.member_id, token_hash(&cookie), future])?;
    db.execute(
        "INSERT INTO ui_codes (org_id, member_id, code_hash, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            bootstrap.org_id,
            bootstrap.member_id,
            token_hash(&code),
            future
        ],
    )?;
    for (id, kind, conversation, node) in [
        ("old-device", "device", None, None),
        ("old-lead", "lead", Some("conv"), Some("machine")),
    ] {
        db.execute(
            "INSERT INTO credentials (org_id, member_id, id, kind, name, token_hash, created_at,
                                      conversation_id, node, created_by)
             VALUES (?1, ?2, ?3, ?4, ?3, ?5, 'then', ?6, ?7, 'owner')",
            params![
                bootstrap.org_id,
                bootstrap.member_id,
                id,
                kind,
                token_hash(id),
                conversation,
                node
            ],
        )?;
    }
    let pairing = util::pairing_code()?;
    db.execute(
        "INSERT INTO pairings (org_id, member_id, code_hash, name, created_at, expires_at, issued_by)
         VALUES (?1, ?2, ?3, 'phone', 'then', ?4, 'old-device')",
        params![bootstrap.org_id, bootstrap.member_id, token_hash(&util::canonical_code(&pairing)), util::utc_in(600)],
    )?;
    let job_query = "SELECT rowid, job_id, revision, state, offered_at FROM jobs";
    let conversation_query = "SELECT conversation_id, created_by, revision FROM conversations";
    let job = rows(&db, job_query)?;
    let conversation = rows(&db, conversation_query)?;
    let revision = rows(&db, "SELECT seq FROM changes")?;
    drop(db);

    let store = Store::open(&dir)?;
    assert_eq!(store.db().scope().member_id, bootstrap.member_id);
    assert_eq!(rows(store.db().raw(), job_query)?, job);
    assert_eq!(rows(store.db().raw(), conversation_query)?, conversation);
    assert_eq!(rows(store.db().raw(), "SELECT seq FROM changes")?, revision);
    let owner = access::client(&store, local)?.ok_or("lost local token")?;
    assert_eq!(owner.id, "owner");
    assert_eq!(owner.kind, access::Kind::Local);
    assert_eq!(owner.member_id(), bootstrap.member_id);
    assert_eq!(
        fs::read_to_string(access::local_credential_path(&store))?.trim(),
        local
    );
    let (browser, csrf) = ui_auth::authenticate(&store, &cookie)?.ok_or("lost old cookie")?;
    assert!(matches!(browser.kind, access::Kind::Browser { .. }));
    assert_eq!(browser.id, owner.id);
    assert_eq!(browser.member_id(), owner.member_id());
    drop(store);

    let store = Store::open(&dir)?;
    let (session, restarted_csrf) =
        ui_auth::authenticate(&store, &cookie)?.ok_or("lost old cookie on restart")?;
    assert_eq!(session.kind, browser.kind);
    assert_eq!(restarted_csrf, csrf);
    let (new_cookie, _) = ui_auth::exchange(&store, &code)?.ok_or("lost old UI link")?;
    assert!(ui_auth::authenticate(&store, &new_cookie)?.is_some());
    let device = access::client(&store, "old-device")?.ok_or("lost device")?;
    assert_eq!(device.kind, access::Kind::Device);
    assert_eq!(device.member_id(), owner.member_id());
    let lead = access::client(&store, "old-lead")?.ok_or("lost lead")?;
    assert_eq!(lead.kind, access::Kind::Lead);
    assert_eq!(lead.member_id(), owner.member_id());
    assert_eq!(lead.conversation_id.as_deref(), Some("conv"));
    let node = access::node(&store, "old-node")?.ok_or("lost node")?;
    assert_eq!((node.id, node.name.as_str()), (7, "machine"));
    assert_eq!(node.registered_by.scope.member_id, owner.member_id());
    let (id, name, token) =
        access::exchange_pairing(&store, &pairing)?.ok_or("lost old pairing")?;
    assert_eq!(name, "phone");
    let paired = access::client(&store, &token)?.ok_or("new device did not authenticate")?;
    assert_eq!(paired.id, id);
    assert_eq!(paired.member_id(), owner.member_id());
    assert!(access::exchange_pairing(&store, &pairing)?.is_none());
    for table in ["jobs", "conversations"] {
        let member: String = store.db().query_row(
            &format!("SELECT responsible_member_id FROM {table} WHERE +org_id = :org_id"),
            params![],
            |row| row.get(0),
        )?;
        assert_eq!(member, owner.member_id());
    }
    Ok(())
}

#[test]
fn v11_foreign_key_check_rolls_back_schema_and_backfill() -> Res {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("data");
    let mut db = v9_fixture(&dir)?;
    let scope = teams::migrate(&mut db, None)?;
    db.pragma_update(None, "foreign_keys", false)?;
    db.execute(
        "UPDATE credentials SET member_id = 'missing' WHERE id = 'owner'",
        [],
    )?;
    db.pragma_update(None, "foreign_keys", true)?;
    let schema = "SELECT name, sql FROM sqlite_schema ORDER BY name";
    let before = rows(&db, schema)?;
    let Err(error) = teams::migrate_identity(&mut db) else {
        panic!("orphaned credential migrated");
    };
    assert!(error.to_string().contains("foreign_key_check"), "{error}");
    assert_eq!(user_version(&db)?, teams::VERSION);
    assert_eq!(rows(&db, schema)?, before);
    assert!(db.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))?);
    assert_eq!(
        rows(&db, "SELECT registered_by FROM nodes")?,
        rows(&db, "VALUES (NULL)")?
    );
    db.execute(
        "UPDATE credentials SET member_id = ?1 WHERE id = 'owner'",
        [&scope.member_id],
    )?;
    drop(db);

    let store = Store::open(&dir)?;
    let db = store.db();
    db.execute("INSERT INTO orgs VALUES ('other', 'Other')", params![])?;
    db.execute(
        "INSERT INTO members VALUES ('foreign-member', 'other', 'Foreign', 'owner', 'active')",
        params![],
    )?;
    let Err(error) = db.execute(
        "UPDATE jobs SET responsible_member_id = 'foreign-member' WHERE +org_id = :org_id",
        params![],
    ) else {
        panic!("job was attributed to a member of another org");
    };
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(e, _) if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY)
    );
    Ok(())
}

fn v9_fixture(dir: &Path) -> Result<RawConnection, Box<dyn std::error::Error>> {
    let db = legacy_fixture(dir, 9)?;
    assert_eq!(user_version(&db)?, 9);
    db.execute_batch(
        "INSERT INTO meta VALUES ('installation','inst_old'), ('local_node','machine'),
           ('lead_node','machine'), ('chief_of_staff','conv'), ('pairing_failures','4');
         INSERT INTO nodes (id,name,token_hash,created_at,revision)
           VALUES (7,'machine','node-hash','then',11);
         INSERT INTO credentials (id,kind,name,token_hash,created_at,last_used_at,revoked_at,revoked_by)
           VALUES ('owner','local','Owner','local-hash','then','recent',NULL,NULL),
                  ('revoked','device','Old','revoked-hash','then','recent','later','owner');
         INSERT INTO pairings (code_hash,name,created_at,expires_at,used_at,credential_id,issued_by)
           VALUES ('pair-hash','Old','then','past','later','revoked','owner');
         INSERT INTO ui_codes VALUES ('ui-code', 20);
         INSERT INTO ui_sessions VALUES ('ui-session', 30);
         INSERT INTO jobs (job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at,revision,offered_at)
           VALUES ('job',7,'spec','task',2,'blob','{}','accepted','then',12,'later');
         INSERT INTO job_files VALUES ('job','spec','blob',5,420);
         INSERT INTO runs (run_id,node_id,usage,received_at,revision,job_id)
           VALUES ('run',7,'{}','then',13,'job-that-has-not-arrived');
         INSERT INTO run_files VALUES ('run','evidence','blob',5);
         INSERT INTO conversations (conversation_id,node,spec,spec_version,model,instructions_tokens,
                                    tools,created_by,created_at,revision,archived_at)
           VALUES ('conv','machine','spec','blob','model',19,1,'owner','then',14,'later');
         INSERT INTO budgets VALUES ('conversation','conv',200,'month','owner','then');
         INSERT INTO job_budgets (job_id,conversation_id,state,trials,created_at)
           VALUES ('job','conv','reserved',2,'then');
         INSERT INTO writes VALUES ('receipt','request','job','job','owner','then');
         INSERT INTO changes VALUES (14,'conversation','conv','then'), (200,'job','job','later');
         DELETE FROM changes WHERE seq = 200;",
    )?;
    db.execute(
        "UPDATE credentials SET token_hash=?1 WHERE id='owner'",
        [sha256_hex(b"fixture-credential")],
    )?;
    write_private(&dir.join("local-credential"), b"fixture-credential\n")?;
    let sha = sha256_hex(b"staged blob");
    let shard = dir.join("objects").join(&sha[..2]);
    private_dir(&shard)?;
    write_private(&shard.join(sha), b"staged blob")?;
    Ok(db)
}

fn rows(db: &RawConnection, sql: &str) -> rusqlite::Result<Vec<Vec<rusqlite::types::Value>>> {
    let mut stmt = db.prepare(sql)?;
    let columns = stmt.column_count();
    stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())?
        .collect()
}

#[test]
fn v9_upgrade_preserves_rows_bindings_artifacts_and_sequence() -> Res {
    rehearse_upgrade(9)
}

#[test]
fn v10_upgrade_preserves_rows_bindings_artifacts_and_sequence() -> Res {
    rehearse_upgrade(10)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn upgraded_v10_authorizes_mixed_roles() -> Res {
    use crate::access;
    use crate::events::Hub;
    use crate::server::{Call, State};
    use crate::settings::{Args, Settings};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use std::sync::Arc;

    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("data");
    let mut old = v9_fixture(&dir)?;
    old.execute_batch(
        "UPDATE conversations SET lead=1, archived_at=NULL;
         DELETE FROM meta WHERE key='chief_of_staff';",
    )?;
    teams::migrate(&mut old, None)?;
    drop(old);
    let store = Store::open(&dir)?;
    let token = fs::read_to_string(access::local_credential_path(&store))?;
    let args = Args::parse(&["--data-dir".into(), dir.to_string_lossy().into_owned()])?;
    let state = Arc::new(State {
        hub: Hub::new(&store)?,
        store,
        settings: Settings::from_args(&args)?,
    });
    let (admin, member_id, member, viewer) = {
        let db = state.store.db();
        for role in ["admin", "member", "viewer"] {
            db.execute(
                "INSERT INTO members VALUES (?1,:org_id,?1,?1,'active')",
                [role],
            )?;
        }
        let (_, admin) = access::mint_device(&db, "admin", "Admin")?;
        let (member_id, member) = access::mint_device(&db, "member", "Member")?;
        let (_, viewer) = access::mint_device(&db, "viewer", "Viewer")?;
        db.execute(
            "UPDATE conversations SET sponsor_member_id='member', responsible_member_id='member'
             WHERE org_id=:org_id AND conversation_id='conv'",
            params![],
        )?;
        (admin, member_id, member, viewer)
    };
    let lead = access::mint_lead(&state.store, "Hosted", "conv", None, &member_id)?;
    let lead = lead["token"].as_str().ok_or("missing lead token")?;

    let call = |bearer: &str,
                method: &str,
                route: &str,
                key: &str,
                body: Value|
     -> Result<_, hyper::header::InvalidHeaderValue> {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", format!("Bearer {bearer}").parse()?);
        headers.insert("idempotency-key", key.parse()?);
        Ok(crate::client_api::handle(
            &state,
            &Call {
                method: method.into(),
                path: format!("/api/v1/client/{route}"),
                query: String::new(),
                headers,
                body: body.to_string().into(),
            },
        ))
    };
    // The migrated job predates this conversation's lead credential and has no
    // attached intent. Sharing its node does not give the lead access to it.
    assert_eq!(
        call(lead, "GET", "jobs/job", "unrelated", Value::Null)?.status(),
        403
    );
    for (label, bearer, role, control, settings_status, approvals_status) in [
        ("owner", token.trim(), "owner", true, 200, 200),
        ("admin", admin.as_str(), "admin", true, 403, 200),
        ("member", member.as_str(), "member", true, 403, 403),
        ("viewer", viewer.as_str(), "viewer", false, 403, 403),
        ("lead", lead, "member", false, 403, 200),
    ] {
        let reply = call(bearer, "GET", "me", label, Value::Null)?;
        assert_eq!(reply.status(), 200, "{label}");
        let me: Value = serde_json::from_slice(
            &reply
                .into_body()
                .collect()
                .await
                .map_err(|error| error.to_string())?
                .to_bytes(),
        )?;
        assert_eq!(me["role"], role);
        assert_eq!(me["owner"], label == "owner");
        if label == "lead" {
            assert_eq!(me["member"]["id"], "member");
            assert_eq!(me["conversation_id"], "conv");
        }
        let reply = call(bearer, "GET", "conversations/conv", label, Value::Null)?;
        assert_eq!(reply.status(), 200, "{label}");
        let body: Value = serde_json::from_slice(
            &reply
                .into_body()
                .collect()
                .await
                .map_err(|error| error.to_string())?
                .to_bytes(),
        )?;
        assert_eq!(body["conversation"]["sponsor_member_id"], "member");
        let flags = &body["conversation"]["actions"];
        assert_eq!(flags["control"], control, "{label}");
        assert_eq!(flags["archive"], control, "{label}");
        assert_eq!(
            flags["adopt"],
            matches!(label, "owner" | "admin"),
            "{label}"
        );
        let write = call(
            bearer,
            "PATCH",
            "conversations/conv",
            label,
            json!({"title":label}),
        )?;
        assert_eq!(write.status(), if control { 200 } else { 403 }, "{label}");
        let settings = call(bearer, "GET", "peer-message-allowlist", label, Value::Null)?;
        assert_eq!(settings.status(), settings_status, "{label}");
        let approvals = call(bearer, "GET", "action-approvals", label, Value::Null)?;
        assert_eq!(approvals.status(), approvals_status, "{label}");
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn rehearse_upgrade(version: u32) -> Res {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("data");
    let mut old = v9_fixture(&dir)?;
    old.execute_batch(
        "UPDATE conversations SET lead=1, attention_question='Review?', attention_job_id='job';
         UPDATE runs SET job_id='job';
         INSERT INTO credentials (id,kind,name,token_hash,created_at,conversation_id,node)
           VALUES ('hosted','lead','Hosted lead','lead-hash','then','conv','machine'),
                  ('paired','device','Paired device','device-hash','then',NULL,NULL);
         INSERT INTO pairings (code_hash,name,created_at,expires_at,used_at,credential_id,issued_by)
           VALUES ('paired-hash','Paired device','then','later','then','paired','owner');
         INSERT INTO conversations (conversation_id,node,spec,spec_version,model,
                                    instructions_tokens,tools,created_by,created_at)
           VALUES ('other','machine','spec','blob','model',19,1,'owner','then');
         INSERT INTO intents (intent_id,node,spec,task,trials,meta,bundle_sha256,created_by,created_at)
           VALUES ('intent','machine','spec','task',2,'{}','blob','owner','then');
         INSERT INTO projects (project_id,name,created_by,created_at)
           VALUES ('project','App','owner','then');
         INSERT INTO backlog_items (item_id,project_id,status,priority,data,created_at,created_by,updated_at,updated_by)
           VALUES ('backlog','project','blocked',1,
             json_object('title','Pick a deployment window','blocked_on',json_array(json_object('kind','owner'))),
             'then','owner','then','owner');
         INSERT INTO task_needs_you (needs_you_id,intent_id,conversation_id,job_id,raised_at,raised_by,status)
           VALUES ('question','intent','conv','job','then','hosted','open');
         INSERT INTO action_approvals (id,job_id,run_id,request,status,created_at)
           VALUES ('publish','job','run',json_object('argv',json_array('git','push')),'needs_owner','then');
         INSERT INTO schedules (schedule_key,node_id,schedule_id,schedule_revision,body,reported_at)
           VALUES ('machine/nightly',7,'nightly',2,json_object('cron','0 0 * * *','enabled',1),'then');",
    )?;
    if version == 10 {
        let scope = teams::migrate(&mut old, None)?;
        old.execute(
            "INSERT INTO org_settings VALUES (?1,'peer_message_allowlist',
                json_array(json_object('from_conversation','conv','to_conversation','other')))",
            [&scope.org_id],
        )?;
    }
    assert_eq!(user_version(&old)?, version);
    assert_eq!(
        old.pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0))?,
        "ok"
    );
    let tables = [
        "nodes",
        "credentials",
        "pairings",
        "ui_codes",
        "ui_sessions",
        "jobs",
        "job_files",
        "runs",
        "run_files",
        "conversations",
        "budgets",
        "job_budgets",
        "writes",
        "changes",
        "intents",
        "projects",
        "backlog_items",
        "task_needs_you",
        "action_approvals",
        "schedules",
    ];
    let before = tables
        .iter()
        .map(|table| -> rusqlite::Result<_> {
            let columns: String = old.query_row(
                "SELECT group_concat(printf('\"%w\"', name)) FROM pragma_table_info(?1)
                 WHERE name != 'registered_by'",
                [table],
                |row| row.get(0),
            )?;
            let sql = format!("SELECT {columns} FROM {table} ORDER BY {columns}");
            Ok((rows(&old, &sql)?, sql))
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(old);

    let store = Store::open(&dir)?;
    assert_eq!(store.installation, "inst_old");
    assert_eq!(
        fs::read(dir.join("local-credential"))?,
        b"fixture-credential\n"
    );
    assert_eq!(
        store.read_object(&sha256_hex(b"staged blob")).as_deref(),
        Some(&b"staged blob"[..])
    );
    let db = store.db();
    assert_eq!(user_version(db.raw())?, SCHEMA_VERSION);
    assert!(
        db.raw()
            .pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))?
    );
    assert_eq!(
        rows(db.raw(), "PRAGMA foreign_key_check")?,
        Vec::<Vec<rusqlite::types::Value>>::new()
    );
    for (table, (expected, sql)) in tables.into_iter().zip(before) {
        assert_eq!(rows(db.raw(), &sql)?, expected, "{table}");
        let invalid = format!("SELECT 1 FROM {table} WHERE org_id != :org_id");
        assert!(!db.prepare(&invalid)?.exists(params![])?, "{table}");
    }
    assert_eq!(
        rows(db.raw(), "PRAGMA integrity_check")?,
        rows(db.raw(), "VALUES ('ok')")?
    );
    for (table, column) in [
        ("intents", "responsible_member_id"),
        ("jobs", "responsible_member_id"),
        ("conversations", "responsible_member_id"),
        ("conversations", "sponsor_member_id"),
        ("schedules", "responsible_member_id"),
        ("task_needs_you", "assignee_member_id"),
        ("backlog_items", "assignee_member_id"),
        ("action_approvals", "assigned_member_id"),
    ] {
        let invalid = format!("SELECT 1 FROM {table} WHERE {column} IS NOT :member_id");
        assert!(
            !db.prepare(&invalid)?.exists(params![])?,
            "{table}.{column}"
        );
    }
    assert!(
        !db.prepare(
            "SELECT 1 FROM conversations WHERE attention_assignee_member_id IS NOT
               CASE WHEN attention_question IS NOT NULL THEN :member_id END",
        )?
        .exists(params![])?
    );
    for table in ["credentials", "pairings", "ui_codes", "ui_sessions"] {
        let invalid = format!("SELECT 1 FROM {table} WHERE member_id != :member_id");
        assert!(!db.prepare(&invalid)?.exists(params![])?, "{table}");
    }
    assert!(
        !db.prepare("SELECT 1 FROM nodes WHERE registered_by IS NOT :member_id")?
            .exists(params![])?
    );
    assert_eq!(
        rows(
            db.raw(),
            "SELECT key,value FROM org_settings WHERE key != 'peer_message_allowlist' ORDER BY key"
        )?,
        rows(
            db.raw(),
            "VALUES ('chief_of_staff','conv'), ('lead_node','machine'),
                          ('local_node','machine'), ('pairing_failures','4')"
        )?,
    );
    assert_eq!(
        rows(
            db.raw(),
            "SELECT value FROM org_settings WHERE key='peer_message_allowlist'"
        )?,
        if version == 10 {
            rows(
                db.raw(),
                "VALUES (json_array(json_object('from_conversation','conv','to_conversation','other')))",
            )?
        } else {
            vec![]
        }
    );
    let data_version = |conn: &RawConnection| {
        conn.pragma_query_value(None, "data_version", |row| row.get::<_, u32>(0))
    };
    let before_reopen = data_version(db.raw())?;
    drop(Store::open(&dir)?);
    assert_eq!(data_version(db.raw())?, before_reopen);
    assert!(
        !db.prepare(
            "SELECT 1 FROM meta WHERE key IN
        ('chief_of_staff','chief_of_staff_model_note','lead_node','local_node','pairing_failures')"
        )?
        .exists(params![])?
    );
    db.execute(
        "INSERT INTO changes (org_id,kind,record_id,at)
                VALUES (:org_id,'job','job','now')",
        params![],
    )?;
    assert_eq!(revision(&db)?, 201);

    db.execute("INSERT INTO orgs VALUES ('other','Other')", params![])?;
    let Err(error) = db.execute(
        "INSERT INTO run_files (org_id,run_id,path,sha256,bytes) VALUES ('other','run','cross','blob',1)",
        params![],
    ) else {
        panic!("inserted a file for a run in a different org");
    };
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(e, _) if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY)
    );
    Ok(())
}

#[test]
fn repeat_open_uses_existing_bootstrap() -> Res {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("data");
    let store = Store::open(&dir)?;
    let pointers = "SELECT key,value FROM meta ORDER BY key";
    let before = rows(store.db().raw(), pointers)?;
    let db = store.db();
    db.execute(
        "INSERT INTO org_settings (org_id, key, value) VALUES (:org_id, 'local_node', 'custom')",
        params![],
    )?;
    db.raw().execute_batch(
        "UPDATE orgs SET name = 'Renamed';
         CREATE TRIGGER refuse_seed BEFORE INSERT ON orgs BEGIN SELECT RAISE(ABORT, 'reseed'); END;",
    )?;
    drop(db);
    drop(store);

    let store = Store::open(&dir)?;
    let db = store.db();
    assert_eq!(rows(db.raw(), pointers)?, before);
    assert_eq!(
        rows(db.raw(), "SELECT name FROM orgs")?,
        rows(db.raw(), "VALUES ('Renamed')")?
    );
    assert_eq!(
        rows(db.raw(), "SELECT value FROM org_settings")?,
        rows(db.raw(), "VALUES ('custom')")?
    );
    assert_eq!(revision(&db)?, 0);
    Ok(())
}

#[test]
fn failed_foreign_key_verification_rolls_back_and_can_retry() -> Res {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("data");
    let mut old = v9_fixture(&dir)?;
    old.pragma_update(None, "foreign_keys", false)?;
    old.execute(
        "INSERT INTO run_files VALUES ('missing','orphan','blob',5)",
        [],
    )?;
    old.pragma_update(None, "foreign_keys", true)?;
    let schema = "SELECT name,sql FROM sqlite_schema ORDER BY name";
    let before = rows(&old, schema)?;
    let Err(error) = teams::migrate(&mut old, None) else {
        panic!("migrated a database with an orphaned run file");
    };
    assert!(error.to_string().contains("foreign_key_check"), "{error}");
    assert_eq!(user_version(&old)?, 9);
    assert_eq!(rows(&old, schema)?, before);
    assert!(old.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))?);
    assert!(
        old.prepare("SELECT 1 FROM meta WHERE key = 'local_node'")?
            .exists(params![])?
    );
    assert!(
        !old.prepare("SELECT 1 FROM meta WHERE key = 'default_org_id'")?
            .exists(params![])?
    );
    assert_eq!(
        rows(
            &old,
            "SELECT seq FROM sqlite_sequence WHERE name = 'changes'"
        )?,
        rows(&old, "VALUES (200)")?
    );

    assert!(Store::open(&dir).is_err());
    old.execute("DELETE FROM run_files WHERE run_id = 'missing'", [])?;
    drop(old);
    let store = Store::open(&dir)?;
    assert_eq!(user_version(store.db().raw())?, SCHEMA_VERSION);
    assert_eq!(
        rows(store.db().raw(), "PRAGMA foreign_key_check")?,
        Vec::<Vec<rusqlite::types::Value>>::new()
    );
    Ok(())
}

#[test]
fn invalid_bootstrap_graph_refuses_startup() -> Res {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("data");
    let store = Store::open(&dir)?;
    store
        .db()
        .raw()
        .execute("UPDATE members SET status = 'inactive'", [])?;
    drop(store);
    let Err(error) = Store::open(&dir) else {
        panic!("opened an invalid bootstrap graph");
    };
    assert!(
        error.contains("invalid Director bootstrap graph"),
        "{error}"
    );
    Ok(())
}

#[test]
fn browser_writes_and_expiry_use_the_validated_scope() -> Res {
    use crate::{ui_auth, util};

    let tmp = tempfile::tempdir()?;
    let store = Store::open(&tmp.path().join("data"))?;
    let foreign_code = "f".repeat(64);
    let foreign_hash = util::sha256_hex(foreign_code.as_bytes());
    {
        let db = store.db();
        db.raw().execute_batch(
            "INSERT INTO orgs VALUES ('foreign', 'Foreign');
             INSERT INTO members VALUES ('foreign-member', 'foreign', 'Owner', 'owner', 'active');
             INSERT INTO ui_codes (org_id, member_id, code_hash, expires_at)
             VALUES ('foreign', 'foreign-member', 'foreign-expired', 0);",
        )?;
        db.raw().execute(
            "INSERT INTO ui_codes (org_id, member_id, code_hash, expires_at)
             VALUES ('foreign', 'foreign-member', ?1, ?2)",
            params![foreign_hash, i64::MAX],
        )?;
        db.execute(
            "INSERT INTO ui_codes (org_id, member_id, code_hash, expires_at)
             VALUES (:org_id, :member_id, 'local-expired', 0)",
            params![],
        )?;
    }

    assert!(ui_auth::exchange(&store, &foreign_code)?.is_none());
    let local_code = ui_auth::issue(&store)?;
    assert!(ui_auth::exchange(&store, &local_code)?.is_some());

    let db = store.db();
    let codes = db
        .raw()
        .prepare("SELECT code_hash FROM ui_codes")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    assert_eq!(codes, [foreign_hash, "foreign-expired".into()].into());
    let session_count: i64 = db.query_row(
        "SELECT COUNT(*) FROM ui_sessions WHERE org_id = :org_id AND member_id = :member_id",
        params![],
        |row| row.get(0),
    )?;
    assert_eq!(session_count, 1);
    Ok(())
}

#[test]
fn v9_upgrade_keeps_rowid_order() -> Res {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("data");
    let db = v9_fixture(&dir)?;
    // Insert out of key order, then delete one, so rowid order differs from key order and has a gap.
    db.execute_batch(
        "INSERT INTO writes VALUES ('k-zulu','r1','job','job','owner','then'),
                                  ('k-alpha','r2','job','job','owner','then'),
                                  ('k-mike','r3','job','job','owner','then'),
                                  ('k-bravo','r4','job','job','owner','then');
         DELETE FROM writes WHERE key = 'k-mike';",
    )?;
    let order = "SELECT key FROM writes ORDER BY rowid";
    let before = rows(&db, order)?;
    drop(db);

    let store = Store::open(&dir)?;
    assert_eq!(rows(store.db().raw(), order)?, before);
    Ok(())
}

#[test]
fn v14_open_checks_org_qualified_assignment_references() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let store = Store::open(&data)?;
    let db = store.db();
    db.fixture_batch(
        "INSERT INTO conversations (org_id,conversation_id,node,spec,spec_version,model,
            instructions_tokens,tools,created_by,created_at,sponsor_member_id)
         VALUES (:org_id,'conversation','node','spec','1','model',0,0,'owner','now','missing');",
    )?;
    drop(db);
    drop(store);
    let Err(error) = Store::open(&data) else {
        return Err("accepted a dangling sponsor reference".into());
    };
    assert!(
        error.contains("invalid organization member reference in conversations.sponsor_member_id")
    );
    Ok(())
}
