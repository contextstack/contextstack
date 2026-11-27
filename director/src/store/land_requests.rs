//! Add hosted land requests to the existing action approval flow.

use super::{OrgScope, migration_tx, teams, user_version};
use rusqlite::{Connection, Result, params};

pub(super) const VERSION: u32 = 12;

pub(super) fn migrate(db: &mut Connection, scope: &OrgScope) -> Result<()> {
    if user_version(db)? >= VERSION {
        return Ok(());
    }
    db.pragma_update(None, "foreign_keys", false)?;
    let result = migrate_locked(db, scope);
    let restore = db.pragma_update(None, "foreign_keys", true);
    result.and(restore)
}

fn migrate_locked(db: &mut Connection, scope: &OrgScope) -> Result<()> {
    let tx = migration_tx(db)?;
    if user_version(&tx)? != teams::IDENTITY_VERSION {
        return Ok(());
    }
    if !has_assignment(&tx)? {
        tx.execute_batch(
            "ALTER TABLE action_approvals ADD COLUMN assigned_member_id TEXT REFERENCES members(id);",
        )?;
        tx.execute(
            "UPDATE action_approvals SET assigned_member_id = ?1 WHERE +org_id = ?2 AND status != 'pending' AND job_id IS NOT NULL",
            params![scope.member_id, scope.org_id],
        )?;
    }
    if has_land_requests(&tx)? {
        tx.pragma_update(None, "user_version", VERSION)?;
        return tx.commit();
    }
    let before = approval_count(&tx)?;
    tx.execute_batch(
        "CREATE TABLE migration_v12 (
            org_id TEXT NOT NULL REFERENCES orgs(id), id TEXT PRIMARY KEY,
            job_id TEXT, run_id TEXT, request TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending', created_at TEXT NOT NULL,
            reviewer_model TEXT, verdict TEXT, review_reason TEXT, reviewed_at TEXT,
            decided_by TEXT, decided_at TEXT, outcome TEXT, claimed_at TEXT,
            revision INTEGER NOT NULL DEFAULT 0, conversation_id TEXT, node_id INTEGER,
            assigned_member_id TEXT REFERENCES members(id),
            FOREIGN KEY (org_id,job_id) REFERENCES jobs(org_id,job_id),
            FOREIGN KEY (org_id,conversation_id) REFERENCES conversations(org_id,conversation_id),
            FOREIGN KEY (org_id,node_id) REFERENCES nodes(org_id,id),
            CHECK ((job_id IS NOT NULL AND run_id IS NOT NULL AND conversation_id IS NULL AND node_id IS NULL)
                OR (job_id IS NULL AND run_id IS NULL AND conversation_id IS NOT NULL AND node_id IS NOT NULL)),
            UNIQUE(job_id,run_id,request),
            UNIQUE(org_id,id)
        );
        INSERT INTO migration_v12 (rowid,org_id,id,job_id,run_id,request,status,created_at,
            reviewer_model,verdict,review_reason,reviewed_at,decided_by,decided_at,outcome,claimed_at,revision,assigned_member_id)
        SELECT rowid,org_id,id,job_id,run_id,request,status,created_at,
            reviewer_model,verdict,review_reason,reviewed_at,decided_by,decided_at,outcome,claimed_at,revision,assigned_member_id
        FROM action_approvals;
        DROP TABLE action_approvals;
        ALTER TABLE migration_v12 RENAME TO action_approvals;
        CREATE INDEX action_approvals_job ON action_approvals(job_id);
        CREATE INDEX action_approvals_status ON action_approvals(status,created_at);
        CREATE INDEX action_approvals_org ON action_approvals(org_id);
        CREATE INDEX action_approvals_node ON action_approvals(node_id,status,claimed_at);
        CREATE INDEX action_approvals_conversation ON action_approvals(conversation_id,status);
        CREATE UNIQUE INDEX IF NOT EXISTS conversations_land_parent ON conversations(org_id,conversation_id);
        CREATE UNIQUE INDEX IF NOT EXISTS nodes_land_parent ON nodes(org_id,id);
        CREATE TABLE IF NOT EXISTS land_requests (
            org_id TEXT NOT NULL REFERENCES orgs(id),
            approval_id TEXT NOT NULL,
            requested_by_member_id TEXT NOT NULL,
            PRIMARY KEY (org_id,approval_id),
            FOREIGN KEY (org_id,approval_id) REFERENCES action_approvals(org_id,id),
            FOREIGN KEY (org_id,requested_by_member_id) REFERENCES members(org_id,id)
        );",
    )?;
    if approval_count(&tx)? != before {
        return Err(invalid(
            "row count changed while rebuilding action_approvals",
        ));
    }
    if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(invalid("v12 migration failed foreign_key_check"));
    }
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()
}

fn has_assignment(db: &Connection) -> Result<bool> {
    db.prepare(
        "SELECT 1 FROM pragma_table_info('action_approvals') WHERE name = 'assigned_member_id'",
    )?
    .exists([])
}

fn has_land_requests(db: &Connection) -> Result<bool> {
    db.prepare("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'land_requests'")?
        .exists([])
}

fn approval_count(db: &Connection) -> Result<i64> {
    db.query_row("SELECT count(*) FROM action_approvals", [], |row| {
        row.get(0)
    })
}

fn invalid(message: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{LAST_LEGACY_VERSION, Store, legacy_fixture};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn v11_fixture(dir: &std::path::Path) -> TestResult<Connection> {
        let mut db = legacy_fixture(dir, LAST_LEGACY_VERSION)?;
        let scope = teams::migrate(&mut db, None)?;
        teams::migrate_identity(&mut db)?;
        assert_eq!(user_version(&db)?, 11);
        db.execute(
            "INSERT INTO nodes (org_id,id,name,token_hash,created_at) VALUES (?1,1,'node','hash','now')",
            [&scope.org_id],
        )?;
        db.execute(
            "INSERT INTO jobs (org_id,job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at)
             VALUES (?1,'job',1,'spec','task',1,'sha','{}','done','now')",
            [&scope.org_id],
        )?;
        db.execute(
            "INSERT INTO action_approvals (rowid,org_id,id,job_id,run_id,request,created_at,status,revision)
             VALUES (42,?1,'approval','job','run','{}','now','approved',17)",
            [&scope.org_id],
        )?;
        Ok(db)
    }

    #[test]
    fn v11_approval_survives_upgrade() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("director");
        drop(v11_fixture(&dir)?);
        let store = Store::open(&dir)?;
        let db = store.db();
        assert_eq!(user_version(db.raw())?, crate::store::SCHEMA_VERSION);
        let approval = db.query_row(
            "SELECT rowid,status,revision,conversation_id FROM action_approvals
             WHERE +org_id = :org_id AND id='approval'",
            rusqlite::params![],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )?;
        assert_eq!(approval, (42_i64, "approved".to_owned(), 17_i64, None));
        assert_eq!(approval_count(db.raw())?, 1);
        assert!(!db.raw().prepare("PRAGMA foreign_key_check")?.exists([])?);
        Ok(())
    }

    #[test]
    fn main_v12_approval_assignment_survives_upgrade() -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("director");
        let mut db = v11_fixture(&dir)?;
        let scope = teams::bootstrap(&db)?;
        migrate(&mut db, &scope)?;
        assert_eq!(user_version(&db)?, VERSION);
        db.execute(
            "INSERT INTO members VALUES ('admin', ?1, 'Admin', 'admin', 'active')",
            [&scope.org_id],
        )?;
        db.execute(
            "UPDATE action_approvals SET assigned_member_id='admin' WHERE id='approval'",
            [],
        )?;
        db.execute(
            "INSERT INTO conversations (org_id,conversation_id,node,spec,spec_version,model,
                instructions_tokens,tools,created_by,created_at)
             VALUES (?1,'conv','node','spec','version','model',0,0,'actor','now')",
            [&scope.org_id],
        )?;
        db.execute(
            "INSERT INTO action_approvals (org_id,id,conversation_id,node_id,request,created_at,status)
             VALUES (?1,'land','conv',1,
                     json_object('kind','land','repo','app','lane','release','target_run_id','run'),
                     'now','needs_owner')",
            [&scope.org_id],
        )?;
        db.execute(
            "INSERT INTO land_requests VALUES (?1,'land','admin')",
            [&scope.org_id],
        )?;
        drop(db);

        let store = Store::open(&dir)?;
        let db = store.db();
        assert_eq!(user_version(db.raw())?, crate::store::SCHEMA_VERSION);
        assert!(has_land_requests(db.raw())?);
        let assigned: String = db.query_row(
            "SELECT assigned_member_id FROM action_approvals WHERE +org_id = :org_id AND id='approval'",
            params![],
            |row| row.get(0),
        )?;
        assert_eq!(assigned, "admin");
        assert_eq!(
            db.raw().query_row(
                "SELECT requested_by_member_id, status, json_extract(request,'$.target_run_id')
                 FROM land_requests JOIN action_approvals USING (org_id)
                 WHERE approval_id=id AND assigned_member_id =
                       (SELECT value FROM meta WHERE key='bootstrap_member_id')",
                [],
                |row| Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?
                )),
            )?,
            ("admin".into(), "needs_owner".into(), "run".into())
        );
        assert!(!db.raw().prepare("PRAGMA foreign_key_check")?.exists([])?);

        Ok(())
    }

    #[test]
    fn complete_v13_migrations_are_noops() -> TestResult {
        check_v13_migrations(false)
    }

    #[test]
    fn v13_without_land_requests_is_never_lowered() -> TestResult {
        check_v13_migrations(true)
    }

    fn check_v13_migrations(remove_land_requests: bool) -> TestResult {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("director");
        drop(Store::open(&dir)?);
        let mut db = Connection::open(dir.join("director.sqlite"))?;
        let scope = teams::bootstrap(&db)?;
        db.pragma_update(None, "user_version", 13)?;
        if remove_land_requests {
            db.execute_batch("DROP TABLE land_requests")?;
        }
        let schema_version = |db: &Connection| {
            db.pragma_query_value(None, "schema_version", |row| row.get::<_, u32>(0))
        };
        let before = schema_version(&db)?;
        migrate(&mut db, &scope)?;
        teams::migrate_invites(&mut db)?;
        assert_eq!(user_version(&db)?, 13);
        assert_eq!(schema_version(&db)?, before);
        drop(db);
        assert_eq!(
            user_version(Store::open(&dir)?.db().raw())?,
            crate::store::SCHEMA_VERSION
        );
        Ok(())
    }

    #[test]
    fn foreign_key_fault_rolls_back_schema_data_and_stamp() -> TestResult {
        let temp = tempfile::tempdir()?;
        let mut db = v11_fixture(&temp.path().join("director"))?;
        db.pragma_update(None, "foreign_keys", false)?;
        db.execute(
            "UPDATE action_approvals SET job_id='missing' WHERE id='approval'",
            [],
        )?;
        db.pragma_update(None, "foreign_keys", true)?;
        let dump = |db: &Connection| -> Result<Vec<String>> {
            db.prepare("SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")?
                .query_map([], |r| r.get(0))?
                .collect()
        };
        let before = dump(&db)?;
        let scope = teams::migrate_identity(&mut db)?;
        let error = migrate(&mut db, &scope)
            .err()
            .ok_or("broken reference must abort v12")?;
        assert!(
            error
                .to_string()
                .contains("v12 migration failed foreign_key_check")
        );
        assert_eq!(user_version(&db)?, 11);
        assert_eq!(dump(&db)?, before);
        assert!(db.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))?);
        let job: String = db.query_row(
            "SELECT job_id FROM action_approvals WHERE rowid=42",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(job, "missing");
        db.execute(
            "UPDATE action_approvals SET job_id='job' WHERE id='approval'",
            [],
        )?;
        migrate(&mut db, &scope)?;
        assert_eq!(user_version(&db)?, 12);
        Ok(())
    }
}
