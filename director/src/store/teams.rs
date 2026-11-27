//! Teams migrations and validation of the bootstrap graph.

use super::OrgScope;
use super::{LAST_LEGACY_VERSION, migration_tx, user_version};
use rusqlite::{Connection, OptionalExtension, Result, params};
use std::collections::BTreeSet;
use std::fmt::Write;

const TABLES: &str = "
    nodes credentials pairings ui_codes ui_sessions
    intents jobs runs job_files run_files lead_objects envelopes planning_jobs decisions lands
    cancels controls schedules firings schedule_changes live_runs run_events action_approvals
    task_needs_you owner_message_marks conversations conversation_files turns summaries projects
    decision_log decision_log_revisions backlog_items backlog_revisions project_heartbeats
    budgets job_budgets writes changes
";

const ACCESS: &[&str] = &["credentials", "pairings", "ui_codes", "ui_sessions"];
pub(super) const VERSION: u32 = 10;
pub(super) const IDENTITY_VERSION: u32 = 11;
pub(super) const INVITES_VERSION: u32 = 13;

const SCHEMA: &str = "
CREATE TABLE orgs (id TEXT NOT NULL PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE teams (
    id TEXT NOT NULL PRIMARY KEY, org_id TEXT NOT NULL REFERENCES orgs(id),
    name TEXT NOT NULL, UNIQUE (org_id,id));
CREATE TABLE members (
    id TEXT NOT NULL PRIMARY KEY, org_id TEXT NOT NULL REFERENCES orgs(id),
    display_name TEXT NOT NULL, role TEXT NOT NULL, status TEXT NOT NULL, UNIQUE (org_id,id));
CREATE TABLE team_members (
    org_id TEXT NOT NULL, team_id TEXT NOT NULL, member_id TEXT NOT NULL, role TEXT NOT NULL,
    PRIMARY KEY (team_id,member_id),
    FOREIGN KEY (org_id,team_id) REFERENCES teams(org_id,id),
    FOREIGN KEY (org_id,member_id) REFERENCES members(org_id,id));
CREATE INDEX team_members_org_member_team ON team_members(org_id,member_id,team_id);
CREATE TABLE org_settings (
    org_id TEXT NOT NULL REFERENCES orgs(id), key TEXT NOT NULL, value TEXT NOT NULL,
    PRIMARY KEY (org_id,key));
";

pub(super) fn migrate(conn: &mut Connection, owner_name: Option<&str>) -> Result<OrgScope> {
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = migrate_locked(conn, owner_name);
    let restore = conn.pragma_update(None, "foreign_keys", true);
    result.and_then(|scope| restore.map(|()| scope))
}

fn migrate_locked(conn: &mut Connection, owner_name: Option<&str>) -> Result<OrgScope> {
    let tx = migration_tx(conn)?;
    if user_version(&tx)? != LAST_LEGACY_VERSION {
        return bootstrap(&tx);
    }
    tx.execute_batch(SCHEMA)?;
    let (org, member) = seed(&tx, owner_name.unwrap_or("Owner"))?;
    let sequence: Option<i64> = tx
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'changes'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let mut parents = BTreeSet::new();
    for table in TABLES.split_whitespace() {
        rebuild(&tx, table, &org, &member, &mut parents)?;
    }
    if let Some(sequence) = sequence {
        tx.execute("DELETE FROM sqlite_sequence WHERE name = 'changes'", [])?;
        tx.execute(
            "INSERT INTO sqlite_sequence(name, seq) VALUES ('changes', ?1)",
            [sequence],
        )?;
    }
    for (table, column) in parents {
        tx.execute_batch(&format!(
            "CREATE UNIQUE INDEX {table}_org_parent_{column} ON {table}(org_id,{column})"
        ))?;
    }
    add_indexes(&tx)?;
    if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(invalid("v10 migration failed foreign_key_check"));
    }
    let scope = bootstrap(&tx)?;
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    Ok(scope)
}

pub(super) fn migrate_identity(db: &mut Connection) -> Result<OrgScope> {
    if user_version(db)? >= IDENTITY_VERSION {
        return bootstrap(db);
    }
    db.pragma_update(None, "foreign_keys", false)?;
    let result = migrate_identity_locked(db);
    let restore = db.pragma_update(None, "foreign_keys", true);
    result.and_then(|scope| restore.map(|()| scope))
}

fn migrate_identity_locked(db: &mut Connection) -> Result<OrgScope> {
    let tx = migration_tx(db)?;
    let scope = bootstrap(&tx)?;
    // Another opener may have upgraded while this connection waited for the write lock.
    if user_version(&tx)? != VERSION {
        return Ok(scope);
    }
    for table in ["intents", "jobs", "conversations", "schedules"] {
        attribute_work(&tx, table, &scope)?;
    }
    tx.execute(
        "UPDATE nodes SET registered_by = ?1 WHERE +org_id = ?2 AND registered_by IS NULL",
        params![scope.member_id, scope.org_id],
    )?;
    super::add_columns(&tx, &[("ui_sessions", "id", "TEXT")])?;
    tx.execute_batch(
        "UPDATE ui_sessions SET id = 'ui_' || lower(hex(randomblob(16))) WHERE id IS NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS ui_sessions_id ON ui_sessions(id);",
    )?;
    if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(invalid("v11 migration failed foreign_key_check"));
    }
    tx.pragma_update(None, "user_version", IDENTITY_VERSION)?;
    tx.commit()?;
    Ok(scope)
}

fn attribute_work(db: &Connection, table: &str, scope: &OrgScope) -> Result<()> {
    if db
        .prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name='responsible_member_id'")?
        .exists([table])?
    {
        return Ok(());
    }
    let count: i64 = db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
    let ddl: String = db.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    let body = ddl
        .split_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .ok_or_else(|| invalid("unexpected v10 work table definition"))?;
    let indexes = table_indexes(db, table)?;
    let columns: String = db.query_row(
        "SELECT group_concat(printf('\"%w\"', name) ORDER BY cid) FROM pragma_table_info(?1)",
        [table],
        |row| row.get(0),
    )?;
    db.execute_batch(&format!(
        "CREATE TABLE migration_v11 (responsible_member_id TEXT, {body},
            FOREIGN KEY (org_id,responsible_member_id) REFERENCES members(org_id,id))"
    ))?;
    // Preserve rowids for lists that use them as a tie-breaker.
    db.execute(
        &format!(
            "INSERT INTO migration_v11 (rowid,responsible_member_id,{columns})
             SELECT rowid, CASE WHEN +org_id = ?1 THEN ?2 END, old.* FROM {table} old"
        ),
        params![scope.org_id, scope.member_id],
    )?;
    let copied: i64 = db.query_row("SELECT count(*) FROM migration_v11", [], |r| r.get(0))?;
    if copied != count {
        return Err(invalid(&format!(
            "row count changed while rebuilding {table}"
        )));
    }
    db.execute_batch(&format!(
        "DROP TABLE {table}; ALTER TABLE migration_v11 RENAME TO {table}"
    ))?;
    for index in indexes {
        db.execute_batch(&index)?;
    }
    Ok(())
}

fn invalid(message: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message.into())
}

fn seed(db: &Connection, owner_name: &str) -> Result<(String, String)> {
    const SETTINGS: &str = "('local_node','lead_node','chief_of_staff','chief_of_staff_model_note','pairing_failures')";
    let id = || crate::util::ulid().map_err(|e| invalid(&e));
    let (org, team, member) = (id()?, id()?, id()?);
    db.execute("INSERT INTO orgs VALUES (?1, 'Default')", [&org])?;
    db.execute(
        "INSERT INTO teams VALUES (?1, ?2, 'Default')",
        params![team, org],
    )?;
    db.execute(
        "INSERT INTO members VALUES (?1, ?2, ?3, 'owner', 'active')",
        params![member, org, owner_name],
    )?;
    db.execute(
        "INSERT INTO team_members VALUES (?1, ?2, ?3, 'owner')",
        params![org, team, member],
    )?;
    db.execute(
        "INSERT INTO meta VALUES ('default_org_id', ?1), ('default_team_id', ?2), ('bootstrap_member_id', ?3)",
        params![org, team, member],
    )?;
    db.execute(
        &format!(
            "INSERT INTO org_settings SELECT ?1, key, value FROM meta WHERE key IN {SETTINGS}"
        ),
        [&org],
    )?;
    db.execute(&format!("DELETE FROM meta WHERE key IN {SETTINGS}"), [])?;
    Ok((org, member))
}

pub(super) fn bootstrap(db: &Connection) -> Result<OrgScope> {
    db.query_row(
        "SELECT o.id, m.id
         FROM orgs o
         JOIN teams t ON t.org_id = o.id
         JOIN members m ON m.org_id = o.id
         JOIN team_members tm ON tm.org_id = o.id AND tm.team_id = t.id AND tm.member_id = m.id
         WHERE o.id = (SELECT value FROM meta WHERE key = 'default_org_id')
           AND t.id = (SELECT value FROM meta WHERE key = 'default_team_id')
           AND m.id = (SELECT value FROM meta WHERE key = 'bootstrap_member_id')
           AND m.role = 'owner' AND m.status = 'active' AND tm.role = 'owner'",
        [],
        |r| {
            Ok(OrgScope {
                org_id: r.get(0)?,
                member_id: r.get(1)?,
                role: None,
            })
        },
    )
    .optional()?
    .ok_or_else(|| invalid("invalid Director bootstrap graph"))
}

fn rebuild(
    db: &Connection,
    table: &str,
    org: &str,
    member: &str,
    parents: &mut BTreeSet<(String, String)>,
) -> Result<()> {
    let ddl: String = db.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    // Keep the v9 columns and constraints, including columns added by earlier upgrades.
    let Some((body, "")) = ddl
        .split_once('(')
        .and_then(|(_, rest)| rest.rsplit_once(')'))
    else {
        return Err(invalid(&format!(
            "unexpected v9 table definition for {table}"
        )));
    };
    let mut body = body.to_owned();
    let mut constraints = String::new();
    let mut stmt = db.prepare("SELECT \"table\", \"from\", \"to\", seq, on_update, on_delete FROM pragma_foreign_key_list(?1)")?;
    let mut fks = stmt.query([table])?;
    // The v9 schema uses single-column inline references with NO ACTION.
    while let Some(row) = fks.next()? {
        let (parent, from, to): (String, String, String) = (row.get(0)?, row.get(1)?, row.get(2)?);
        let inline = format!(" REFERENCES {parent}({to})");
        if row.get::<_, u32>(3)? != 0
            || row.get::<_, String>(4)? != "NO ACTION"
            || row.get::<_, String>(5)? != "NO ACTION"
            || !body.contains(&inline)
        {
            return Err(invalid(&format!("unexpected v9 foreign key in {table}")));
        }
        body = body.replacen(&inline, "", 1);
        let _ = write!(
            constraints,
            ", FOREIGN KEY (org_id,{from}) REFERENCES {parent}(org_id,{to})"
        );
        parents.insert((parent, to));
    }

    match table {
        "writes" => {
            body = body.replace("key TEXT PRIMARY KEY", "key TEXT NOT NULL");
            constraints.push_str(", PRIMARY KEY (org_id,key)");
        }
        "budgets" => {
            body = body.replace(
                "PRIMARY KEY (scope, scope_id)",
                "PRIMARY KEY (org_id,scope,scope_id)",
            );
        }
        _ => {}
    }
    // Put new columns first, so the original table constraints can remain in place.
    let mut columns = "org_id TEXT NOT NULL REFERENCES orgs(id),".to_owned();
    let mut bindings = vec![org];
    let access = ACCESS.contains(&table);
    if access {
        columns.push_str("member_id TEXT NOT NULL,");
        constraints.push_str(", FOREIGN KEY (org_id,member_id) REFERENCES members(org_id,id)");
        bindings.push(member);
    }
    if table == "nodes" {
        columns.push_str("registered_by TEXT,");
        constraints.push_str(", FOREIGN KEY (org_id,registered_by) REFERENCES members(org_id,id)");
    }
    let indexes = table_indexes(db, table)?;
    let count: i64 = db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
    db.execute_batch(&format!(
        "CREATE TABLE migration_v10 ({columns}{body}{constraints})"
    ))?;
    let insert = if access {
        format!(
            "INSERT INTO migration_v10 SELECT ?1, ?2, old.* FROM {table} old ORDER BY old.rowid"
        )
    } else if table == "nodes" {
        format!(
            "INSERT INTO migration_v10 SELECT ?1, NULL, old.* FROM {table} old ORDER BY old.rowid"
        )
    } else {
        format!("INSERT INTO migration_v10 SELECT ?1, old.* FROM {table} old ORDER BY old.rowid")
    };
    db.execute(&insert, rusqlite::params_from_iter(bindings))?;
    let copied: i64 = db.query_row("SELECT count(*) FROM migration_v10", [], |r| r.get(0))?;
    if copied != count {
        return Err(invalid(&format!(
            "row count changed while rebuilding {table}"
        )));
    }
    db.execute_batch(&format!(
        "DROP TABLE {table}; ALTER TABLE migration_v10 RENAME TO {table}"
    ))?;
    for index in indexes {
        db.execute_batch(&index)?;
    }
    Ok(())
}

fn table_indexes(db: &Connection, table: &str) -> Result<Vec<String>> {
    db.prepare("SELECT sql FROM sqlite_schema WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL ORDER BY name")?
        .query_map([table], |row| row.get(0))?
        .collect()
}

fn add_indexes(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE INDEX changes_org_seq ON changes(org_id,seq);
         CREATE INDEX job_files_org_sha ON job_files(org_id,sha256);
         CREATE INDEX run_files_org_sha ON run_files(org_id,sha256);
         CREATE INDEX jobs_org_node_state ON jobs(org_id,node_id,state);
         CREATE INDEX controls_org_node_state ON controls(org_id,node_id,state);",
    )?;
    for table in ACCESS {
        db.execute_batch(&format!(
            "CREATE INDEX {table}_org_member ON {table}(org_id,member_id)"
        ))?;
    }
    for table in TABLES.split_whitespace() {
        let indexed = db
            .prepare(
                "SELECT 1 FROM pragma_index_list(?1) AS l, pragma_index_info(l.name) AS i
             WHERE i.seqno = 0 AND i.name = 'org_id'",
            )?
            .exists([table])?;
        if !indexed {
            db.execute_batch(&format!("CREATE INDEX {table}_org ON {table}(org_id)"))?;
        }
    }
    Ok(())
}

pub(super) fn migrate_invites(db: &mut Connection) -> Result<()> {
    if user_version(db)? >= INVITES_VERSION {
        return Ok(());
    }
    let tx = migration_tx(db)?;
    if user_version(&tx)? == super::land_requests::VERSION {
        tx.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS invites (
                org_id TEXT NOT NULL, id TEXT NOT NULL, member_id TEXT NOT NULL,
                issuer_member_id TEXT NOT NULL, issued_by TEXT NOT NULL,
                code_hash TEXT NOT NULL, expires_at TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending','revoked','consumed')),
                PRIMARY KEY (org_id,id), UNIQUE (org_id,code_hash),
                FOREIGN KEY (org_id,member_id) REFERENCES members(org_id,id),
                FOREIGN KEY (org_id,issuer_member_id) REFERENCES members(org_id,id));
            CREATE UNIQUE INDEX IF NOT EXISTS invites_one_pending ON invites(org_id,member_id)
                WHERE status = 'pending';
            CREATE UNIQUE INDEX IF NOT EXISTS pairings_org_code ON pairings(org_id,code_hash);
            CREATE TABLE IF NOT EXISTS pairing_issuers (
                org_id TEXT NOT NULL, code_hash TEXT NOT NULL, issuer_member_id TEXT NOT NULL,
                PRIMARY KEY (org_id,code_hash),
                FOREIGN KEY (org_id,issuer_member_id) REFERENCES members(org_id,id),
                FOREIGN KEY (org_id,code_hash) REFERENCES pairings(org_id,code_hash));
            CREATE TABLE IF NOT EXISTS invite_throttle (
                org_id TEXT NOT NULL REFERENCES orgs(id), bucket TEXT NOT NULL,
                minute INTEGER NOT NULL, count INTEGER NOT NULL, PRIMARY KEY (org_id,bucket));
            CREATE TABLE IF NOT EXISTS member_writes (
                org_id TEXT NOT NULL, key TEXT NOT NULL, member_id TEXT NOT NULL,
                actor_member_id TEXT NOT NULL, response TEXT NOT NULL,
                PRIMARY KEY (org_id,key),
                FOREIGN KEY (org_id,key) REFERENCES writes(org_id,key),
                FOREIGN KEY (org_id,member_id) REFERENCES members(org_id,id),
                FOREIGN KEY (org_id,actor_member_id) REFERENCES members(org_id,id));
        ",
        )?;
        tx.pragma_update(None, "user_version", INVITES_VERSION)?;
    }
    tx.commit()
}

pub(super) const SPONSORSHIP_VERSION: u32 = 14;

pub(super) fn migrate_sponsorship(db: &mut Connection) -> Result<()> {
    if user_version(db)? >= SPONSORSHIP_VERSION {
        return Ok(());
    }
    let tx = migration_tx(db)?;
    if user_version(&tx)? == INVITES_VERSION {
        super::add_columns(
            &tx,
            &[
                ("conversations", "sponsor_member_id", "TEXT"),
                ("conversations", "attention_question", "TEXT"),
                ("conversations", "attention_job_id", "TEXT"),
                ("conversations", "attention_set_at", "TEXT"),
                ("conversations", "attention_set_by", "TEXT"),
                ("conversations", "attention_assignee_member_id", "TEXT"),
                ("task_needs_you", "assignee_member_id", "TEXT"),
                ("backlog_items", "assignee_member_id", "TEXT"),
            ],
        )?;
        tx.execute_batch(
            "UPDATE conversations SET sponsor_member_id=COALESCE(responsible_member_id,
                (SELECT value FROM meta WHERE key='bootstrap_member_id' AND conversations.org_id=(SELECT value FROM meta WHERE key='default_org_id')));
             UPDATE conversations SET attention_assignee_member_id=
                (SELECT value FROM meta WHERE key='bootstrap_member_id' AND conversations.org_id=(SELECT value FROM meta WHERE key='default_org_id'))
                WHERE attention_question IS NOT NULL;
             UPDATE task_needs_you SET assignee_member_id=
                (SELECT value FROM meta WHERE key='bootstrap_member_id' AND task_needs_you.org_id=(SELECT value FROM meta WHERE key='default_org_id'))
                WHERE status='open';
             UPDATE backlog_items SET assignee_member_id=
                (SELECT value FROM meta WHERE key='bootstrap_member_id' AND backlog_items.org_id=(SELECT value FROM meta WHERE key='default_org_id'))
                WHERE status IN ('next','now','blocked') AND EXISTS (
                    SELECT 1 FROM json_each(data, '$.blocked_on') b WHERE b.value ->> 'kind' = 'owner');
             UPDATE action_approvals SET assigned_member_id=
                (SELECT value FROM meta WHERE key='bootstrap_member_id' AND action_approvals.org_id=(SELECT value FROM meta WHERE key='default_org_id'))
                WHERE assigned_member_id IS NULL AND status IN ('pending','needs_owner');
             CREATE TABLE IF NOT EXISTS sponsorship_changes (
                org_id TEXT NOT NULL, conversation_id TEXT NOT NULL, revision INTEGER NOT NULL,
                previous_member_id TEXT NOT NULL, sponsor_member_id TEXT NOT NULL,
                actor_member_id TEXT NOT NULL, actor_credential_id TEXT NOT NULL,
                changed_at TEXT NOT NULL, PRIMARY KEY (org_id, conversation_id, revision));
             CREATE TABLE IF NOT EXISTS assignment_changes (
                revision INTEGER PRIMARY KEY AUTOINCREMENT, org_id TEXT NOT NULL,
                record_kind TEXT NOT NULL, record_id TEXT NOT NULL,
                previous_member_id TEXT NOT NULL, assignee_member_id TEXT NOT NULL,
                actor_member_id TEXT NOT NULL, actor_credential_id TEXT NOT NULL, changed_at TEXT NOT NULL);",
        )?;
        tx.pragma_update(None, "user_version", SPONSORSHIP_VERSION)?;
    }
    tx.commit()
}

/// Check the references introduced after the org-qualified table rebuild.
pub(super) fn check_assignment_members(db: &Connection) -> std::result::Result<(), String> {
    for (table, columns) in [
        (
            "conversations",
            &["sponsor_member_id", "attention_assignee_member_id"][..],
        ),
        ("task_needs_you", &["assignee_member_id"]),
        ("backlog_items", &["assignee_member_id"]),
        ("action_approvals", &["assigned_member_id"]),
        ("land_requests", &["requested_by_member_id"]),
        (
            "sponsorship_changes",
            &["previous_member_id", "sponsor_member_id", "actor_member_id"],
        ),
        (
            "assignment_changes",
            &[
                "previous_member_id",
                "assignee_member_id",
                "actor_member_id",
            ],
        ),
    ] {
        if table == "land_requests"
            && !db
                .prepare("SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1")
                .and_then(|mut stmt| stmt.exists([table]))
                .map_err(|e| e.to_string())?
        {
            // Historical v13 databases could omit this table.
            continue;
        }
        for column in columns {
            let invalid: bool = db.query_row(&format!(
                "SELECT EXISTS (SELECT 1 FROM {table} r WHERE r.{column} IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM members m WHERE m.org_id=r.org_id AND m.id=r.{column}))"
            ), [], |r| r.get(0)).map_err(|e| e.to_string())?;
            if invalid {
                return Err(format!(
                    "invalid organization member reference in {table}.{column}"
                ));
            }
        }
    }
    Ok(())
}
