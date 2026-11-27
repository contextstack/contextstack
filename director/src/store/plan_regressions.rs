use super::{Store, scope_guard::rust_strings};
use rusqlite::Connection;

type PlanCase<'a> = (&'a str, &'a str, &'a str, &'a str, &'a [&'a str]);

fn seed(db: &Connection) -> rusqlite::Result<()> {
    // These cardinalities model a single-org installation with a long turn history.
    db.execute_batch(
        r#"
        BEGIN;
        CREATE TEMP TABLE fixture_ids (id INTEGER PRIMARY KEY);
        INSERT INTO fixture_ids
          WITH RECURSIVE ids(id) AS (VALUES (1) UNION ALL SELECT id + 1 FROM ids WHERE id < 200000)
          SELECT id FROM ids;
        INSERT INTO nodes (org_id,id,name,token_hash,created_at)
          SELECT id,1,'node','hash','now' FROM orgs;
        INSERT INTO intents (org_id,intent_id,node,spec,task,trials,meta,bundle_sha256,created_by,created_at)
          SELECT orgs.id,'intent-' || f.id,'node','spec','task',1,'{}','bundle','owner','now'
          FROM orgs, fixture_ids f WHERE f.id <= 200;
        INSERT INTO conversations (org_id,conversation_id,node,spec,spec_version,model,instructions_tokens,tools,created_by,created_at)
          SELECT orgs.id,'conv-' || f.id,'node','spec','1','model',1,0,'owner','now'
          FROM orgs, fixture_ids f WHERE f.id <= 2000;
        INSERT INTO jobs (org_id,job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at,intent_id,run_id)
          SELECT orgs.id,'job-' || f.id,1,'spec','task',1,'bundle','{"schedule_id":"nightly"}',
                 CASE WHEN f.id <= 5 THEN 'waiting' ELSE 'accepted' END,'now',
                 'intent-' || ((f.id - 1) % 200 + 1),'run-' || f.id
          FROM orgs, fixture_ids f WHERE f.id <= 500;
        INSERT INTO runs (org_id,run_id,node_id,usage,received_at,job_id)
          SELECT org_id,run_id,node_id,'{}','now',job_id FROM jobs;
        INSERT INTO job_budgets (org_id,job_id,state,trials,created_at)
          SELECT org_id,job_id,CASE WHEN state = 'waiting' THEN 'waiting' ELSE 'held' END,trials,created_at
          FROM jobs;
        INSERT INTO turns (org_id,turn_id,conversation_id,seq,message,state,posted_by,posted_at,job_id)
          SELECT orgs.id,'turn-' || f.id,'conv-' || ((f.id - 1) % 2000 + 1),
                 (f.id - 1) / 2000 + 1,'message',
                 CASE WHEN f.id <= 500 THEN 'running' ELSE 'done' END,'owner','now',
                 CASE WHEN f.id <= 500 THEN 'job-' || f.id END
          FROM orgs, fixture_ids f;
        INSERT INTO summaries (org_id,summary_id,conversation_id,through_seq,job_id,state,requested_by,created_at)
          SELECT org_id,'summary-' || job_id,'conv-1',50,job_id,'done','owner','now' FROM jobs;
        INSERT INTO task_needs_you (org_id,needs_you_id,intent_id,conversation_id,job_id,raised_at,raised_by,status)
          SELECT org_id,'needs-' || job_id,intent_id,'conv-1',job_id,'now','owner','open' FROM jobs;
        INSERT INTO action_approvals (org_id,id,job_id,run_id,request,created_at)
          SELECT org_id,'approval-' || job_id,job_id,run_id,'request','now' FROM jobs;
        DROP TABLE fixture_ids;
        COMMIT;"#,
    )
}

// Locate production SQL by its projection and a distinguishing fragment. Keep
// the org predicate out of the selectors so changing it still exercises EXPLAIN.
const CONVERSATIONS: &str = include_str!("../conversations.rs");
const TURNS: &str = include_str!("../turns.rs");
const SETTLE: &str = include_str!("../settle.rs");
const TRACE: &str = include_str!("../work_trace.rs");
const APPROVALS: &str = include_str!("../action_approvals.rs");
const RECORDS: &str = include_str!("../records.rs");
const SCOPES: &str = include_str!("../scopes.rs");
const RESERVATIONS: &str = include_str!("../reservations.rs");
const USAGE: &str = include_str!("../usage.rs");
const CASES: &[PlanCase<'_>] = &[
    (
        include_str!("../access.rs"),
        "SELECT id, kind, conversation_id, intent_id, node, member_id",
        "token_hash = ?1",
        "",
        &["sqlite_autoindex_credentials_2 (token_hash=?)"],
    ),
    (
        include_str!("../ui_auth.rs"),
        "SELECT id FROM credentials",
        "kind = 'local'",
        "",
        &["credentials_org_member (org_id=? AND member_id=?)"],
    ),
    (
        include_str!("../ui_auth.rs"),
        "SELECT id, member_id FROM ui_sessions",
        "token_hash = ?1",
        "",
        &["sqlite_autoindex_ui_sessions_1 (token_hash=?)"],
    ),
    (
        include_str!("../access.rs"),
        "SELECT display_name, role, status FROM members",
        "id = ?1",
        "",
        &["sqlite_autoindex_members_1 (id=?)"],
    ),
    (
        CONVERSATIONS,
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM turns",
        "state NOT IN ('queued', 'summarising')",
        "",
        &["sqlite_autoindex_turns_2 (conversation_id=?)"],
    ),
    (
        CONVERSATIONS,
        "SELECT turn_id, seq, ROW_NUMBER() OVER (ORDER BY seq) FROM turns",
        "state IN ('queued', 'summarising')",
        "",
        &["sqlite_autoindex_turns_2 (conversation_id=?)"],
    ),
    (
        TURNS,
        "SELECT turn_id, seq, message, state",
        "state = 'running'",
        "",
        &["sqlite_autoindex_turns_2 (conversation_id=?)"],
    ),
    (
        TURNS,
        "SELECT turn_id, seq, message, state",
        "state IN ('queued', 'summarising')",
        "",
        &["sqlite_autoindex_turns_2 (conversation_id=?)"],
    ),
    (
        TURNS,
        "SELECT job_id FROM turns",
        "state = 'running'",
        "",
        &["sqlite_autoindex_turns_2 (conversation_id=?)"],
    ),
    (
        TURNS,
        "SELECT turn_id FROM turns",
        "state = 'summarising'",
        "",
        &["sqlite_autoindex_turns_2 (conversation_id=?)"],
    ),
    (
        SETTLE,
        "SELECT turn_id, conversation_id FROM turns",
        "job_id = ?1",
        "",
        &["turns_job (job_id=?)"],
    ),
    (
        SETTLE,
        "SELECT summary_id, conversation_id FROM summaries",
        "job_id = ?1",
        "",
        &["summaries_job (job_id=?)"],
    ),
    (
        TRACE,
        "SELECT conversation_id, turn_id FROM turns",
        "job_id = ?1",
        "",
        &["turns_job (job_id=?)"],
    ),
    (
        TRACE,
        "SELECT COUNT(*) FROM jobs",
        "intent_id = ?1",
        "",
        &["jobs_intent (intent_id=?)"],
    ),
    (
        TRACE,
        "UPDATE task_needs_you",
        "intent_id = ?1",
        "",
        &["task_needs_you_intent (intent_id=?)"],
    ),
    (
        TRACE,
        "SELECT needs_you_id",
        "ORDER BY raised_at",
        "",
        &["task_needs_you_intent (intent_id=?)"],
    ),
    (
        include_str!("../jobs.rs"),
        "SELECT conversation_id, turn_id FROM turns",
        "job_id = ?1",
        "",
        &["turns_job (job_id=?)"],
    ),
    (
        APPROVALS,
        "SELECT id FROM action_approvals",
        "status IN ('pending','needs_owner')",
        "",
        &["action_approvals_job (job_id=?)"],
    ),
    (
        APPROVALS,
        "SELECT a.id,a.job_id,a.created_at",
        "a.verdict='no_go'",
        "",
        &[
            "jobs_intent (intent_id=?)",
            "action_approvals_job (job_id=?)",
        ],
    ),
    (
        include_str!("../live.rs"),
        "SELECT conversation_id, 'turn_id'",
        "UNION ALL",
        "",
        &["turns_job (job_id=?)", "summaries_job (job_id=?)"],
    ),
    (
        RECORDS,
        "SELECT j.job_id, n.name",
        "c.cancel_id",
        " ORDER BY j.rowid DESC LIMIT 500",
        &[
            "sqlite_autoindex_runs_1 (run_id=?)",
            "turns_job (job_id=?)",
            "summaries_job (job_id=?)",
        ],
    ),
    // The legacy RUNS fallbacks keyed by jobs.run_id have no dedicated
    // index. The job-ID and live-run lookups still use their primary keys.
    (
        RECORDS,
        "SELECT r.run_id, n.name",
        "r.transcript",
        " ORDER BY r.rowid DESC LIMIT 500",
        &[
            "sqlite_autoindex_jobs_1 (job_id=?)",
            "sqlite_autoindex_live_runs_1 (run_id=?)",
        ],
    ),
    (
        USAGE,
        "SELECT r.usage,",
        "json_type(j.meta, '$.schedule_id')",
        " AND n.name = ? ORDER BY r.received_at",
        &[
            "sqlite_autoindex_jobs_1 (job_id=?)",
            "sqlite_autoindex_live_runs_1 (run_id=?)",
            "sqlite_autoindex_job_budgets_1 (job_id=?)",
            "turns_job (job_id=?)",
            "summaries_job (job_id=?)",
        ],
    ),
    (
        SCOPES,
        "SELECT COALESCE(SUM(",
        "{col} = ?1",
        "",
        &[
            "sqlite_autoindex_jobs_1 (job_id=?)",
            "INTEGER PRIMARY KEY (rowid=?)",
        ],
    ),
    (
        SCOPES,
        "SELECT job_id FROM job_budgets",
        "state = 'waiting'",
        "",
        &[
            "sqlite_autoindex_jobs_1 (job_id=?)",
            "INTEGER PRIMARY KEY (rowid=?)",
        ],
    ),
    (
        RESERVATIONS,
        "SELECT b.project_id, b.conversation_id,",
        "json_extract(j.meta, '$.schedule_id')",
        "",
        &[
            "sqlite_autoindex_job_budgets_1 (job_id=?)",
            "sqlite_autoindex_jobs_1 (job_id=?)",
        ],
    ),
    (
        RESERVATIONS,
        "SELECT b.job_id, b.project_id, b.conversation_id,",
        "json_extract(j.meta, '$.schedule_id')",
        "",
        &[
            "sqlite_autoindex_job_budgets_1 (job_id=?)",
            "INTEGER PRIMARY KEY (rowid=?)",
        ],
    ),
];

#[test]
fn org_scope_preserves_selective_query_plans() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let store = Store::open(&temp.path().join("data"))?;
    let db = store.db();
    seed(db.raw())?;

    for &(source, prefix, fragment, suffix, indexes) in CASES {
        let strings = rust_strings(source)?;
        let matches: Vec<_> = strings
            .iter()
            .filter(|(_, sql)| sql.starts_with(prefix) && sql.contains(fragment))
            .collect();
        assert_eq!(matches.len(), 1, "{prefix} / {fragment}");
        let mut sql = matches[0]
            .1
            .replace("{OWNER_ONLY_APPROVAL}", crate::policy::OWNER_ONLY_APPROVAL);
        for (placeholder, prefix) in [
            ("{CONV}", "COALESCE(b.conversation_id,"),
            ("{PROJ}", "COALESCE(b.project_id,"),
            ("{col}", "(SELECT n.name || '/'"),
        ] {
            if sql.contains(placeholder) {
                let fragment = strings
                    .iter()
                    .find(|(_, literal)| literal.starts_with(prefix))
                    .unwrap_or_else(|| panic!("missing SQL fragment {prefix}"));
                sql = sql.replace(placeholder, &fragment.1);
            }
        }
        let mut stmt = db
            .raw()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}{suffix}"))?;
        let plan = stmt
            .raw_query()
            .mapped(|row| row.get::<_, String>(3))
            .collect::<rusqlite::Result<Vec<_>>>()?
            .join("\n");
        assert!(
            !plan.contains("_org (") && !plan.contains("(org_id=?)"),
            "{sql}\n{plan}"
        );
        for index in indexes {
            assert!(plan.contains(index), "missing {index}: {sql}\n{plan}");
        }
    }
    Ok(())
}
