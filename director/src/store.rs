//! The Director's own state under one data directory: an SQLite database, a content-addressed object
//! store, and the local credential file. Nothing is shared with the Laravel web client.
//!
//! ```text
//! <data>/                    0700
//!   director.sqlite          0600, WAL
//!   objects/ab/<sha256>      bytes named by their hash, written once
//!   local-credential         0600, the token local clients use (access.rs)
//! ```

use crate::thread;
use crate::util::{self, sha256_hex};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS nodes (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, token_hash TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL, last_seen_at TEXT, revoked_at TEXT,
  revision INTEGER NOT NULL DEFAULT 0, protocol INTEGER, status TEXT, status_at TEXT);
CREATE TABLE IF NOT EXISTS jobs (
  job_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL REFERENCES nodes(id), spec TEXT NOT NULL,
  task TEXT NOT NULL, trials INTEGER NOT NULL, bundle_sha256 TEXT NOT NULL, grant_id TEXT,
  grant_sha256 TEXT, meta TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL,
  accepted_at TEXT, finished_at TEXT, run_id TEXT, terminal_row TEXT, error TEXT,
  revision INTEGER NOT NULL DEFAULT 0, intent_id TEXT, work_reason TEXT);
CREATE TABLE IF NOT EXISTS job_files (
  job_id TEXT NOT NULL REFERENCES jobs(job_id), path TEXT NOT NULL, sha256 TEXT NOT NULL,
  bytes INTEGER NOT NULL, mode INTEGER NOT NULL, PRIMARY KEY (job_id, path));
CREATE INDEX IF NOT EXISTS job_files_sha ON job_files(sha256);
CREATE TABLE IF NOT EXISTS runs (
  run_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL REFERENCES nodes(id), usage TEXT NOT NULL,
  row_line TEXT, transcript TEXT, verdict TEXT, received_at TEXT NOT NULL,
  revision INTEGER NOT NULL DEFAULT 0, job_id TEXT);
CREATE TABLE IF NOT EXISTS run_files (
  run_id TEXT NOT NULL REFERENCES runs(run_id), path TEXT NOT NULL, sha256 TEXT NOT NULL,
  bytes INTEGER NOT NULL, PRIMARY KEY (run_id, path));
CREATE TABLE IF NOT EXISTS envelopes (
  event_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL REFERENCES nodes(id),
  envelope_sha256 TEXT NOT NULL, ack TEXT NOT NULL, received_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS credentials (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL, token_hash TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL, last_used_at TEXT, revoked_at TEXT,
  conversation_id TEXT, intent_id TEXT, node TEXT);
CREATE TABLE IF NOT EXISTS lead_objects (
  credential_id TEXT NOT NULL REFERENCES credentials(id), sha256 TEXT NOT NULL,
  PRIMARY KEY (credential_id, sha256));
CREATE TABLE IF NOT EXISTS pairings (
  code_hash TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL, used_at TEXT, credential_id TEXT);
CREATE TABLE IF NOT EXISTS ui_codes (
  code_hash TEXT PRIMARY KEY, expires_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS ui_sessions (
  token_hash TEXT PRIMARY KEY, expires_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS intents (
  intent_id TEXT PRIMARY KEY, title TEXT, node TEXT NOT NULL, spec TEXT NOT NULL, task TEXT NOT NULL,
  trials INTEGER NOT NULL, grant_id TEXT, meta TEXT NOT NULL, bundle_sha256 TEXT NOT NULL,
  created_by TEXT NOT NULL, created_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
  project_id TEXT, conversation_id TEXT, lead_conversation_id TEXT,
  origin_kind TEXT NOT NULL DEFAULT 'api', origin_conversation_id TEXT, origin_turn_id TEXT);
CREATE TABLE IF NOT EXISTS planning_jobs (
  job_id TEXT PRIMARY KEY REFERENCES jobs(job_id), executor TEXT NOT NULL, deadline TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS decisions (
  decision_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE REFERENCES runs(run_id), decision TEXT NOT NULL,
  note TEXT, decided_by TEXT NOT NULL, decided_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS lands (
  land_id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(run_id), state TEXT NOT NULL,
  repo TEXT NOT NULL, lane TEXT NOT NULL, main TEXT NOT NULL, \"commit\" TEXT, reason TEXT,
  landed_by TEXT NOT NULL, landed_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
  attempts TEXT NOT NULL DEFAULT '[]', current INTEGER NOT NULL DEFAULT 1);
CREATE TABLE IF NOT EXISTS cancels (
  cancel_id TEXT PRIMARY KEY, job_id TEXT NOT NULL UNIQUE REFERENCES jobs(job_id), state TEXT NOT NULL,
  reason TEXT, requested_by TEXT NOT NULL, requested_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
  control_id TEXT);
CREATE TABLE IF NOT EXISTS writes (
  key TEXT PRIMARY KEY, request_sha256 TEXT NOT NULL, record_kind TEXT NOT NULL, record_id TEXT NOT NULL,
  credential_id TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS changes (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, record_id TEXT NOT NULL, at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS controls (
  control_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL REFERENCES nodes(id), kind TEXT NOT NULL, body TEXT NOT NULL,
  state TEXT NOT NULL, outcome TEXT, detail TEXT, result TEXT, issued_at TEXT NOT NULL, received_at TEXT,
  settled_at TEXT);
CREATE INDEX IF NOT EXISTS controls_open ON controls(node_id, state);
CREATE TABLE IF NOT EXISTS schedules (
  schedule_key TEXT PRIMARY KEY, node_id INTEGER NOT NULL REFERENCES nodes(id), schedule_id TEXT NOT NULL,
  schedule_revision INTEGER NOT NULL, body TEXT NOT NULL, reported_at TEXT NOT NULL, removed_at TEXT,
  revision INTEGER NOT NULL DEFAULT 0, UNIQUE (node_id, schedule_id));
CREATE TABLE IF NOT EXISTS firings (
  node_id INTEGER NOT NULL REFERENCES nodes(id), firing_id TEXT NOT NULL, schedule_id TEXT NOT NULL,
  body TEXT NOT NULL, recorded_at TEXT NOT NULL, reported_at TEXT NOT NULL, PRIMARY KEY (node_id, firing_id));
CREATE INDEX IF NOT EXISTS firings_schedule ON firings(node_id, schedule_id, recorded_at);
CREATE TABLE IF NOT EXISTS schedule_changes (
  change_id TEXT PRIMARY KEY, schedule_key TEXT NOT NULL REFERENCES schedules(schedule_key),
  control_id TEXT NOT NULL REFERENCES controls(control_id), expect_revision INTEGER NOT NULL, change TEXT NOT NULL,
  requested_by TEXT NOT NULL, requested_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS live_runs (
  run_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL REFERENCES nodes(id), job_id TEXT NOT NULL REFERENCES jobs(job_id),
  confirmed INTEGER NOT NULL, first_at TEXT NOT NULL, last_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS live_runs_job ON live_runs(job_id);
CREATE TABLE IF NOT EXISTS run_events (
  run_id TEXT NOT NULL REFERENCES live_runs(run_id), seq INTEGER NOT NULL, kind TEXT NOT NULL,
  partial INTEGER NOT NULL, payload TEXT NOT NULL, at TEXT, received_at TEXT NOT NULL, PRIMARY KEY (run_id, seq));
CREATE TABLE IF NOT EXISTS conversations (
  conversation_id TEXT PRIMARY KEY, title TEXT, node TEXT NOT NULL, spec TEXT NOT NULL, spec_version TEXT NOT NULL,
  model TEXT NOT NULL, instructions_tokens INTEGER NOT NULL, tools INTEGER NOT NULL, intent_id TEXT,
  held INTEGER NOT NULL DEFAULT 0, created_by TEXT NOT NULL, created_at TEXT NOT NULL,
  revision INTEGER NOT NULL DEFAULT 0, project_id TEXT,
  attention_question TEXT, attention_job_id TEXT, attention_set_at TEXT, attention_set_by TEXT,
  lead INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS conversation_files (
  conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id), path TEXT NOT NULL,
  sha256 TEXT NOT NULL, bytes INTEGER NOT NULL, mode INTEGER NOT NULL, PRIMARY KEY (conversation_id, path));
CREATE TABLE IF NOT EXISTS turns (
  turn_id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id),
  seq INTEGER NOT NULL, message TEXT NOT NULL, state TEXT NOT NULL, posted_by TEXT NOT NULL, posted_at TEXT NOT NULL,
  job_id TEXT, reply TEXT, detail TEXT, context TEXT, started_at TEXT, finished_at TEXT,
  revision INTEGER NOT NULL DEFAULT 0, UNIQUE (conversation_id, seq));
CREATE INDEX IF NOT EXISTS turns_job ON turns(job_id);
CREATE TABLE IF NOT EXISTS summaries (
  summary_id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id),
  through_seq INTEGER NOT NULL, based_on TEXT, redoes TEXT, job_id TEXT NOT NULL, state TEXT NOT NULL, text TEXT,
  detail TEXT, requested_by TEXT NOT NULL, created_at TEXT NOT NULL, finished_at TEXT,
  revision INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS summaries_job ON summaries(job_id);
CREATE TABLE IF NOT EXISTS task_needs_you (
  needs_you_id TEXT PRIMARY KEY, intent_id TEXT NOT NULL REFERENCES intents(intent_id),
  conversation_id TEXT NOT NULL, job_id TEXT, raised_at TEXT NOT NULL, raised_by TEXT NOT NULL,
  status TEXT NOT NULL, cleared_at TEXT, cleared_by TEXT);
CREATE INDEX IF NOT EXISTS task_needs_you_intent ON task_needs_you(intent_id);
CREATE TABLE IF NOT EXISTS owner_message_marks (
  message_id TEXT PRIMARY KEY, intent_id TEXT NOT NULL REFERENCES intents(intent_id),
  mark TEXT NOT NULL, actor TEXT NOT NULL, marked_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS owner_message_marks_intent ON owner_message_marks(intent_id);
CREATE TABLE IF NOT EXISTS projects (
  project_id TEXT PRIMARY KEY, name TEXT NOT NULL, created_by TEXT NOT NULL, created_at TEXT NOT NULL,
  revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS decision_log (
  entry_id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(project_id),
  number INTEGER NOT NULL CHECK (number > 0), body TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('active', 'superseded', 'retired')),
  superseded_by TEXT REFERENCES decision_log(entry_id),
  created_by TEXT NOT NULL, created_at TEXT NOT NULL, updated_by TEXT NOT NULL, updated_at TEXT NOT NULL,
  revision INTEGER NOT NULL DEFAULT 0, UNIQUE (project_id, number));
CREATE TABLE IF NOT EXISTS decision_log_revisions (
  entry_id TEXT NOT NULL REFERENCES decision_log(entry_id), revision INTEGER NOT NULL,
  snapshot TEXT NOT NULL, PRIMARY KEY (entry_id, revision));
CREATE TABLE IF NOT EXISTS budgets (
  scope TEXT NOT NULL, scope_id TEXT NOT NULL, usd_micros INTEGER NOT NULL, period TEXT NOT NULL,
  set_by TEXT NOT NULL, set_at TEXT NOT NULL, PRIMARY KEY (scope, scope_id));
CREATE TABLE IF NOT EXISTS job_budgets (
  job_id TEXT PRIMARY KEY REFERENCES jobs(job_id), project_id TEXT, conversation_id TEXT, state TEXT NOT NULL,
  ceiling_micros INTEGER, trials INTEGER NOT NULL, reserved_micros INTEGER NOT NULL DEFAULT 0,
  charged_micros INTEGER, charge_basis TEXT, reason TEXT, detail TEXT, created_at TEXT NOT NULL,
  reserved_at TEXT, settled_at TEXT, settled_by TEXT, corrected_at TEXT);
CREATE INDEX IF NOT EXISTS job_budgets_project ON job_budgets(project_id, state);
CREATE INDEX IF NOT EXISTS job_budgets_conversation ON job_budgets(conversation_id, state);
";

/// Version-1 indexes that depend on columns the v0 upgrade adds (or on the current `lands` shape).
const SCHEMA_INDEXES: &str = "
CREATE INDEX IF NOT EXISTS jobs_intent ON jobs(intent_id);
CREATE INDEX IF NOT EXISTS jobs_node_state ON jobs(node_id, state);
CREATE INDEX IF NOT EXISTS runs_job ON runs(job_id);
CREATE INDEX IF NOT EXISTS conversations_intent ON conversations(intent_id);
CREATE INDEX IF NOT EXISTS intents_lead_conversation ON intents(lead_conversation_id);
CREATE UNIQUE INDEX IF NOT EXISTS lands_one_current ON lands(run_id) WHERE current = 1;
";

/// Schema version this binary creates and upgrades through.
pub(crate) const SCHEMA_VERSION: u32 = 8;

/// Numbered migrations after the v0 baseline path. Each entry is the `user_version` after it runs.
type MigrationFn = fn(&Connection) -> rusqlite::Result<()>;

const MIGRATIONS: &[(u32, MigrationFn)] = &[
    (2, migrate_v1_to_v2),
    (3, migrate_v2_to_v3),
    (4, migrate_v3_to_v4),
    (5, |db| db.execute_batch(crate::action_approvals::SCHEMA)),
    (6, |db| {
        db.execute_batch(
            "ALTER TABLE action_approvals ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
        )
    }),
    (7, |db| db.execute_batch(crate::backlog::SCHEMA)),
    (8, |db| {
        add_columns(
            db,
            &[
                ("credentials", "created_by", "TEXT"),
                ("credentials", "revoked_by", "TEXT"),
                ("pairings", "issued_by", "TEXT"),
            ],
        )
    }),
];

/// v2: a turn says who started it: `person` (a posted message) or `wake` (a hosted lead's follow-up after a lane ended).
fn migrate_v1_to_v2(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("ALTER TABLE turns ADD COLUMN origin TEXT NOT NULL DEFAULT 'person'")
}

/// v3: reversible conversation archival.
fn migrate_v2_to_v3(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("ALTER TABLE conversations ADD COLUMN archived_at TEXT")
}

/// v4: work-trace origin, job reasons, needs-you history and owner-message marks.
fn migrate_v3_to_v4(conn: &Connection) -> rusqlite::Result<()> {
    for (table, column, definition) in [
        ("intents", "origin_kind", "TEXT NOT NULL DEFAULT 'api'"),
        ("intents", "origin_conversation_id", "TEXT"),
        ("intents", "origin_turn_id", "TEXT"),
        ("jobs", "work_reason", "TEXT"),
    ] {
        let present = conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
            ))?
            .exists(params![column])?;
        if !present {
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))?;
        }
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_needs_you (
          needs_you_id TEXT PRIMARY KEY, intent_id TEXT NOT NULL REFERENCES intents(intent_id),
          conversation_id TEXT NOT NULL, job_id TEXT, raised_at TEXT NOT NULL, raised_by TEXT NOT NULL,
          status TEXT NOT NULL, cleared_at TEXT, cleared_by TEXT);
        CREATE INDEX IF NOT EXISTS task_needs_you_intent ON task_needs_you(intent_id);
        CREATE TABLE IF NOT EXISTS owner_message_marks (
          message_id TEXT PRIMARY KEY, intent_id TEXT NOT NULL REFERENCES intents(intent_id),
          mark TEXT NOT NULL, actor TEXT NOT NULL, marked_at TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS owner_message_marks_intent ON owner_message_marks(intent_id);",
    )
}

/// Columns added after step 3's schema: `(table, column, definition)`, added when absent.
const ADDED: [(&str, &str, &str); 24] = [
    ("jobs", "revision", "INTEGER NOT NULL DEFAULT 0"),
    ("jobs", "intent_id", "TEXT"),
    ("jobs", "error", "TEXT"),
    ("runs", "revision", "INTEGER NOT NULL DEFAULT 0"),
    // Step 5: protocol 2.
    ("cancels", "control_id", "TEXT"),
    ("nodes", "revision", "INTEGER NOT NULL DEFAULT 0"),
    ("nodes", "protocol", "INTEGER"),
    ("nodes", "status", "TEXT"),
    ("nodes", "status_at", "TEXT"),
    // Step 7: projects.
    ("intents", "project_id", "TEXT"),
    ("intents", "conversation_id", "TEXT"),
    ("conversations", "project_id", "TEXT"),
    // Step 7 follow-ups: how a reservation closed, a late correction, and the job a run names.
    ("job_budgets", "settled_by", "TEXT"),
    ("job_budgets", "corrected_at", "TEXT"),
    ("runs", "job_id", "TEXT"),
    // Hosted lead: persist its conversation, optional intent and estate node binding.
    ("credentials", "conversation_id", "TEXT"),
    ("credentials", "intent_id", "TEXT"),
    ("credentials", "node", "TEXT"),
    // Hosted lead: queue keeps running; the owner sees one clear question.
    ("conversations", "attention_question", "TEXT"),
    ("conversations", "attention_job_id", "TEXT"),
    ("conversations", "attention_set_at", "TEXT"),
    ("conversations", "attention_set_by", "TEXT"),
    // Hosted lead: which conversation started a task lane (from the submitting credential only).
    ("intents", "lead_conversation_id", "TEXT"),
    // Hosted lead: stored conversation lead marker.
    ("conversations", "lead", "INTEGER NOT NULL DEFAULT 0"),
];

/// Numbers each object write's temp file.
static OBJECT_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A record a client sees, with its own revision (CLIENT-API.md §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Record {
    /// What was asked.
    Intent,
    /// One assignment of work to a node.
    Job,
    /// One execution attempt with evidence.
    Run,
    /// A decision receipt about one run's result.
    Decision,
    /// What happened when a task lane was landed on the machine.
    Land,
    /// A cancel request for one job.
    Cancel,
    /// A node and its status.
    Node,
    /// A schedule its node reported.
    Schedule,
    /// A request to change one schedule.
    ScheduleChange,
    /// A thread of turns.
    Conversation,
    /// One message of a conversation and its reply; each is a job.
    Turn,
    /// A summary of a conversation's older turns, made by a job.
    Summary,
    /// A project: intents and conversations grouped, with an optional budget.
    Project,
    /// A project decision and its rationale.
    DecisionLog,
    /// An item in a project backlog.
    BacklogItem,
    /// A request for authorization to perform an action.
    ActionApproval,
}

impl Record {
    /// Every kind.
    pub const ALL: [Self; 16] = [
        Self::Intent,
        Self::Job,
        Self::Run,
        Self::Decision,
        Self::Land,
        Self::Cancel,
        Self::Node,
        Self::Schedule,
        Self::ScheduleChange,
        Self::Conversation,
        Self::Turn,
        Self::Summary,
        Self::Project,
        Self::DecisionLog,
        Self::BacklogItem,
        Self::ActionApproval,
    ];

    /// The kind's name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Job => "job",
            Self::Run => "run",
            Self::Decision => "decision",
            Self::Land => "land",
            Self::Cancel => "cancel",
            Self::Node => "node",
            Self::Schedule => "schedule",
            Self::ScheduleChange => "schedule_change",
            Self::Conversation => "conversation",
            Self::Turn => "turn",
            Self::Summary => "summary",
            Self::Project => "project",
            Self::DecisionLog => "decision_log",
            Self::BacklogItem => "backlog_item",
            Self::ActionApproval => "action_approval",
        }
    }

    /// The kind a wire name names.
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    fn table(self) -> (&'static str, &'static str) {
        match self {
            Self::Intent => ("intents", "intent_id"),
            Self::Job => ("jobs", "job_id"),
            Self::Run => ("runs", "run_id"),
            Self::Decision => ("decisions", "decision_id"),
            Self::Land => ("lands", "land_id"),
            Self::Cancel => ("cancels", "cancel_id"),
            Self::Node => ("nodes", "name"),
            Self::Schedule => ("schedules", "schedule_key"),
            Self::ScheduleChange => ("schedule_changes", "change_id"),
            Self::Conversation => ("conversations", "conversation_id"),
            Self::Turn => ("turns", "turn_id"),
            Self::Summary => ("summaries", "summary_id"),
            Self::Project => ("projects", "project_id"),
            Self::DecisionLog => ("decision_log", "entry_id"),
            Self::BacklogItem => ("backlog_items", "item_id"),
            Self::ActionApproval => ("action_approvals", "id"),
        }
    }
}

/// Record that a record changed: the next revision of the whole installation becomes its revision, and the
/// change is queued for the event stream. Call it inside the transaction that made the change.
pub fn touch(db: &Connection, kind: Record, id: &str) -> rusqlite::Result<i64> {
    db.execute(
        "INSERT INTO changes (kind, record_id, at) VALUES (?1, ?2, ?3)",
        params![kind.name(), id, util::now_utc()],
    )?;
    let revision = db.last_insert_rowid();
    let (table, key) = kind.table();
    db.execute(
        &format!("UPDATE {table} SET revision = ?1 WHERE {key} = ?2"),
        params![revision, id],
    )?;
    Ok(revision)
}

/// The newest revision recorded.
pub fn revision(db: &Connection) -> rusqlite::Result<i64> {
    db.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| {
        r.get(0)
    })
}

fn backfill_conversation_lead(conn: &Connection, data_dir: &Path) -> rusqlite::Result<()> {
    let done = conn
        .prepare("SELECT 1 FROM meta WHERE key = 'backfill_conversation_lead'")?
        .exists([])?;
    if done {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT c.conversation_id, cf.sha256 FROM conversations c
         JOIN conversation_files cf ON cf.conversation_id = c.conversation_id AND cf.path = c.spec
         WHERE c.lead = 0",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, sha) in rows {
        let path = data_dir.join("objects").join(&sha[..2]).join(&sha);
        let lead = std::fs::read(&path).is_ok_and(|yaml| thread::is_lead_spec(&yaml));
        if lead {
            conn.execute(
                "UPDATE conversations SET lead = 1 WHERE conversation_id = ?1",
                params![id],
            )?;
        }
    }
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('backfill_conversation_lead', '1')",
        [],
    )?;
    Ok(())
}

/// A land used to be one row per run. Until the record is `landed`, a later report keeps that row
/// (so an idempotent replay still answers it) and stores the earlier attempt on the new current row.
fn lands_keep_earlier_attempts(conn: &Connection) -> rusqlite::Result<()> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'lands'",
        [],
        |r| r.get(0),
    )?;
    if sql.contains("run_id TEXT NOT NULL UNIQUE") {
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        let rebuilt = rebuild_lands(conn);
        let fk = conn.pragma_update(None, "foreign_keys", "ON");
        rebuilt?;
        fk?;
    }
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS lands_one_current ON lands(run_id) WHERE current = 1",
    )?;
    Ok(())
}

fn rebuild_lands(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE lands_next (
           land_id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(run_id), state TEXT NOT NULL,
           repo TEXT NOT NULL, lane TEXT NOT NULL, main TEXT NOT NULL, \"commit\" TEXT, reason TEXT,
           landed_by TEXT NOT NULL, landed_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
           attempts TEXT NOT NULL DEFAULT '[]', current INTEGER NOT NULL DEFAULT 1);
         INSERT INTO lands_next (
           land_id, run_id, state, repo, lane, main, \"commit\", reason, landed_by, landed_at, revision)
         SELECT land_id, run_id, state, repo, lane, main, \"commit\", reason, landed_by, landed_at, revision
         FROM lands;
         DROP TABLE lands;
         ALTER TABLE lands_next RENAME TO lands;",
    )?;
    tx.commit()?;
    Ok(())
}

fn ensure_indexes(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS jobs_intent ON jobs(intent_id);
         CREATE INDEX IF NOT EXISTS jobs_node_state ON jobs(node_id, state);
         CREATE INDEX IF NOT EXISTS runs_job ON runs(job_id);
         CREATE INDEX IF NOT EXISTS conversations_intent ON conversations(intent_id);
         CREATE INDEX IF NOT EXISTS intents_lead_conversation ON intents(lead_conversation_id);",
    )
}

/// Begin a write transaction under WAL: immediate lock, so a deferred read-then-write does not fail with `SQLITE_BUSY`.
pub fn write_tx(conn: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}

fn add_columns(conn: &Connection, columns: &[(&str, &str, &str)]) -> rusqlite::Result<()> {
    for &(table, column, definition) in columns {
        let present = conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
            ))?
            .exists(params![column])?;
        if !present {
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))?;
        }
    }
    Ok(())
}

fn user_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
}

/// Pre–schema-version installs: improvised column adds, `lands` rebuild, backfills, then stamp v1.
fn migrate_v0_to_v1(conn: &Connection, data_dir: &Path) -> rusqlite::Result<()> {
    add_columns(conn, &ADDED)?;
    lands_keep_earlier_attempts(conn)?;
    backfill_conversation_lead(conn, data_dir)?;
    ensure_indexes(conn)?;
    conn.pragma_update(None, "user_version", 1)
}

fn apply_migrations(conn: &mut Connection) -> rusqlite::Result<()> {
    loop {
        let current = user_version(conn)?;
        if current >= SCHEMA_VERSION {
            break;
        }
        let next = current + 1;
        let Some((_, migrate)) = MIGRATIONS.iter().find(|(v, _)| *v == next) else {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "database schema version {current} has no migration to version {next}"
            )));
        };
        let tx = write_tx(conn)?;
        migrate(&tx)?;
        tx.pragma_update(None, "user_version", next)?;
        tx.commit()?;
    }
    Ok(())
}

fn open_database(path: &Path, dir: &Path) -> Result<Connection, String> {
    let mut conn =
        Connection::open(path).map_err(|e| format!("database {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("database {}: {e}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| format!("database {}: {e}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .and_then(|()| conn.pragma_update(None, "foreign_keys", "ON"))
        .and_then(|()| conn.execute_batch(SCHEMA))
        .map_err(|e| format!("database {}: {e}", path.display()))?;
    let ver = user_version(&conn).map_err(|e| format!("database {}: {e}", path.display()))?;
    if ver > SCHEMA_VERSION {
        return Err(format!(
            "database schema version {ver} is newer than this director supports ({SCHEMA_VERSION})"
        ));
    }
    if ver == 0 {
        migrate_v0_to_v1(&conn, dir).map_err(|e| format!("database {}: {e}", path.display()))?;
    } else {
        conn.execute_batch(SCHEMA_INDEXES)
            .map_err(|e| format!("database {}: {e}", path.display()))?;
    }
    apply_migrations(&mut conn).map_err(|e| format!("database {}: {e}", path.display()))?;
    Ok(conn)
}

/// The data directory, opened: the database behind a lock and the object store.
pub struct Store {
    db: Mutex<Connection>,
    /// The data directory.
    pub dir: PathBuf,
    /// This Director's installation ID, created once.
    pub installation: String,
}

impl Store {
    /// Open (creating on first use) the data directory, its database and installation ID.
    pub fn open(dir: &Path) -> Result<Self, String> {
        private_dir(dir)?;
        private_dir(&dir.join("objects"))?;
        let path = dir.join("director.sqlite");
        let conn = open_database(&path, dir)?;
        let installation = installation(&conn)?;
        Ok(Self {
            db: Mutex::new(conn),
            dir: dir.to_path_buf(),
            installation,
        })
    }

    /// The database, locked. One writer at a time keeps every receipt a single consistent step.
    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn object_path(&self, sha: &str) -> PathBuf {
        self.dir.join("objects").join(&sha[..2]).join(sha)
    }

    /// Whether an object is stored.
    pub fn has_object(&self, sha: &str) -> bool {
        util::is_sha256(sha) && self.object_path(sha).is_file()
    }

    /// An object's size; `None` when absent.
    pub fn object_len(&self, sha: &str) -> Option<u64> {
        if !util::is_sha256(sha) {
            return None;
        }
        fs::metadata(self.object_path(sha)).ok().map(|m| m.len())
    }

    /// An object's bytes; `None` when absent.
    pub fn read_object(&self, sha: &str) -> Option<Vec<u8>> {
        if !util::is_sha256(sha) {
            return None;
        }
        fs::read(self.object_path(sha)).ok()
    }

    /// Store bytes under their own hash. `Ok(true)` when newly stored; existing bytes are never replaced.
    pub fn put_object(&self, data: &[u8]) -> Result<bool, String> {
        let sha = sha256_hex(data);
        let path = self.object_path(&sha);
        if path.is_file() {
            return Ok(false);
        }
        let dir = path.parent().ok_or("object path has no parent")?;
        private_dir(dir)?;
        // Unique per write: two requests storing the same bytes at once must not share a temp file.
        let n = OBJECT_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{sha}.{}.{n}.tmp", std::process::id()));
        let written = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(data).and_then(|()| f.sync_all()))
            .and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(format!("object {sha}: {e}"));
        }
        Ok(true)
    }
}

/// The installation ID: `inst_<ulid>`, created on first open and never changed.
fn installation(conn: &Connection) -> Result<String, String> {
    let get = |conn: &Connection| {
        conn.query_row(
            "SELECT value FROM meta WHERE key = 'installation'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    };
    if let Some(id) = get(conn)? {
        return Ok(id);
    }
    let id = format!("inst_{}", util::ulid()?.to_lowercase());
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('installation', ?1)",
        params![id],
    )
    .map_err(|e| e.to_string())?;
    get(conn)?.ok_or_else(|| "installation ID was not stored".into())
}

/// Create `dir` (mode 0700) if needed and refuse one that other users can write or read.
pub fn private_dir(dir: &Path) -> Result<(), String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let mode = fs::metadata(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is open to other users (mode {:o}); chmod 700 it",
            dir.display(),
            mode & 0o777
        ));
    }
    Ok(())
}

/// Write `contents` to `path` with mode 0600, replacing it atomically.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let done = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(contents).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&tmp, path));
    if let Err(e) = done {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Res = Result<(), Box<dyn std::error::Error>>;

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct ColumnInfo {
        table: String,
        name: String,
        ty: String,
        notnull: i32,
        dflt: Option<String>,
    }

    fn column_set(conn: &Connection) -> rusqlite::Result<Vec<ColumnInfo>> {
        let mut tables = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for name in rows {
            tables.push(name?);
        }
        let mut cols = Vec::new();
        for table in tables {
            let mut info = conn.prepare(&format!(
                "SELECT name, type, \"notnull\", dflt_value FROM pragma_table_info('{table}')"
            ))?;
            let rows = info.query_map([], |r| {
                Ok(ColumnInfo {
                    table: table.clone(),
                    name: r.get(0)?,
                    ty: r.get(1)?,
                    notnull: r.get(2)?,
                    dflt: r.get(3)?,
                })
            })?;
            for c in rows {
                cols.push(c?);
            }
        }
        cols.sort();
        Ok(cols)
    }

    fn index_ddl(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = conn.prepare(
            "SELECT name, COALESCE(sql, '') FROM sqlite_master WHERE type = 'index' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    type SchemaSnapshot = (Vec<ColumnInfo>, Vec<(String, String)>);

    fn schema_snapshot(conn: &Connection) -> rusqlite::Result<SchemaSnapshot> {
        Ok((column_set(conn)?, index_ddl(conn)?))
    }

    fn seed_v0_fixture(dir: &Path) -> Res {
        private_dir(dir)?;
        let db = Connection::open(dir.join("director.sqlite"))?;
        db.execute_batch(
            r#"CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
           CREATE TABLE nodes (
             id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, token_hash TEXT NOT NULL UNIQUE,
             created_at TEXT NOT NULL, last_seen_at TEXT, revoked_at TEXT);
           CREATE TABLE jobs (
             job_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL, spec TEXT NOT NULL, task TEXT NOT NULL,
             trials INTEGER NOT NULL, bundle_sha256 TEXT NOT NULL, grant_id TEXT, grant_sha256 TEXT,
             meta TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL, accepted_at TEXT,
             finished_at TEXT, run_id TEXT, terminal_row TEXT);
           CREATE TABLE runs (
             run_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL, usage TEXT NOT NULL, row_line TEXT,
             transcript TEXT, verdict TEXT, received_at TEXT NOT NULL);
           CREATE TABLE lands (
             land_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE, state TEXT NOT NULL, repo TEXT NOT NULL,
             lane TEXT NOT NULL, main TEXT NOT NULL, "commit" TEXT, reason TEXT, landed_by TEXT NOT NULL,
             landed_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0);
           CREATE TABLE conversations (
             conversation_id TEXT PRIMARY KEY, title TEXT, node TEXT NOT NULL, spec TEXT NOT NULL,
             spec_version TEXT NOT NULL, model TEXT NOT NULL, instructions_tokens INTEGER NOT NULL,
             tools INTEGER NOT NULL, intent_id TEXT, held INTEGER NOT NULL DEFAULT 0,
             created_by TEXT NOT NULL, created_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0);"#,
        )?;
        Ok(())
    }

    #[test]
    fn schema_version_1_new_matches_upgraded_v0_fixture() -> Res {
        let fresh_dir = tempfile::tempdir()?.path().join("fresh");
        let upgraded_dir = tempfile::tempdir()?.path().join("upgraded");
        seed_v0_fixture(&upgraded_dir)?;
        let fresh = Store::open(&fresh_dir)?;
        let upgraded = Store::open(&upgraded_dir)?;
        assert_eq!(user_version(&fresh.db())?, SCHEMA_VERSION);
        assert_eq!(user_version(&upgraded.db())?, SCHEMA_VERSION);
        assert_eq!(
            schema_snapshot(&fresh.db())?,
            schema_snapshot(&upgraded.db())?,
            "fresh and upgraded schemas match"
        );
        Store::open(&fresh_dir)?;
        Store::open(&upgraded_dir)?;
        Ok(())
    }

    #[test]
    fn v2_upgrades_to_v3_preserving_conversations() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        let store = Store::open(&dir)?;
        store.db().execute_batch(
            "DROP TABLE action_approvals;
            ALTER TABLE conversations DROP COLUMN archived_at;
            INSERT INTO conversations (conversation_id, node, spec, spec_version, model,
                instructions_tokens, tools, created_by, created_at)
            VALUES ('existing', 'n', 's', 'v', 'm', 0, 0, 'owner', 'then');
            PRAGMA user_version = 2;",
        )?;
        drop(store);
        let upgraded = Store::open(&dir)?;
        assert_eq!(user_version(&upgraded.db())?, SCHEMA_VERSION);
        let archived: Option<String> = upgraded.db().query_row(
            "SELECT archived_at FROM conversations WHERE conversation_id = 'existing'",
            [],
            |r| r.get(0),
        )?;
        assert!(archived.is_none());
        upgraded
            .db()
            .execute("UPDATE conversations SET archived_at = 'now'", [])?;
        drop(upgraded);
        let reopened = Store::open(&dir)?;
        let archived: String = reopened.db().query_row(
            "SELECT archived_at FROM conversations WHERE conversation_id = 'existing'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(archived, "now");
        Ok(())
    }

    #[test]
    fn v3_upgrades_to_v4_preserving_intents_and_jobs() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        private_dir(&dir)?;
        let path = dir.join("director.sqlite");
        let db = Connection::open(&path)?;
        db.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('installation', 'inst_test');
             CREATE TABLE nodes (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, token_hash TEXT NOT NULL,
               created_at TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active', last_seen TEXT);
             INSERT INTO nodes VALUES (1, 'n', 'h', 't', 'active', NULL);
             CREATE TABLE intents (intent_id TEXT PRIMARY KEY, node TEXT NOT NULL, spec TEXT NOT NULL,
               task TEXT NOT NULL, trials INTEGER NOT NULL, meta TEXT NOT NULL, bundle_sha256 TEXT NOT NULL,
               created_by TEXT NOT NULL, created_at TEXT NOT NULL, lead_conversation_id TEXT, title TEXT);
             INSERT INTO intents (intent_id, node, spec, task, trials, meta, bundle_sha256, created_by, created_at)
             VALUES ('keep-intent', 'n', 's', 'tasks/t', 1, '{}', 'b', 'owner', 'then');
             CREATE TABLE jobs (job_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL, spec TEXT NOT NULL,
               task TEXT NOT NULL, trials INTEGER NOT NULL, bundle_sha256 TEXT NOT NULL, grant_id TEXT,
               grant_sha256 TEXT, meta TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL,
               accepted_at TEXT, finished_at TEXT, run_id TEXT, terminal_row TEXT, intent_id TEXT,
               revision INTEGER NOT NULL DEFAULT 0);
             INSERT INTO jobs (job_id, node_id, spec, task, trials, bundle_sha256, meta, state, created_at, intent_id)
             VALUES ('keep-job', 1, 's', 'tasks/t', 1, 'b', '{}', 'offered', 'then', 'keep-intent');
             CREATE TABLE conversations (
               conversation_id TEXT PRIMARY KEY, title TEXT, node TEXT NOT NULL, spec TEXT NOT NULL,
               spec_version TEXT NOT NULL, model TEXT NOT NULL, instructions_tokens INTEGER NOT NULL,
               tools INTEGER NOT NULL, intent_id TEXT, held INTEGER NOT NULL DEFAULT 0,
               created_by TEXT NOT NULL, created_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
               archived_at TEXT);
             PRAGMA user_version = 3;",
        )?;
        drop(db);
        let store = Store::open(&dir)?;
        assert_eq!(user_version(&store.db())?, SCHEMA_VERSION);
        let origin: String = store.db().query_row(
            "SELECT origin_kind FROM intents WHERE intent_id = 'keep-intent'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(origin, "api");
        let job: String = store.db().query_row(
            "SELECT job_id FROM jobs WHERE job_id = 'keep-job'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(job, "keep-job");
        store.db().execute_batch(
            "SELECT 1 FROM task_needs_you LIMIT 0;
             SELECT 1 FROM owner_message_marks LIMIT 0;",
        )?;
        Ok(())
    }

    #[test]
    fn schema_version_newer_than_binary_is_refused() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        private_dir(&dir)?;
        let path = dir.join("director.sqlite");
        let db = Connection::open(&path)?;
        db.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        db.pragma_update(None, "user_version", SCHEMA_VERSION + 1)?;
        drop(db);
        let Err(err) = Store::open(&dir) else {
            panic!("expected open to fail for too-new schema");
        };
        assert!(
            err.contains(&format!("schema version {}", SCHEMA_VERSION + 1))
                && err.contains(&format!("supports ({SCHEMA_VERSION})")),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[test]
    fn existing_credentials_gain_lead_bindings_without_losing_tokens() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        private_dir(&dir)?;
        let db = Connection::open(dir.join("director.sqlite"))?;
        db.execute_batch(
            "CREATE TABLE credentials (
               id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
               token_hash TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL,
               last_used_at TEXT, revoked_at TEXT);
             INSERT INTO credentials (id, kind, name, token_hash, created_at)
             VALUES ('cred_existing', 'device', 'existing', 'kept-hash', 'now');",
        )?;
        drop(db);
        let store = Store::open(&dir)?;
        let row: (String, Option<String>, Option<String>, Option<String>) = store.db().query_row(
            "SELECT token_hash, conversation_id, intent_id, node FROM credentials WHERE id = 'cred_existing'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        assert_eq!(row, ("kept-hash".into(), None, None, None));
        drop(store);
        Store::open(&dir)?;
        Ok(())
    }

    #[test]
    fn open_is_private_and_keeps_its_installation() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        let first = Store::open(&dir)?.installation;
        assert!(first.starts_with("inst_") && util::plain_name(&first, 128));
        assert_eq!(Store::open(&dir)?.installation, first, "created once");
        let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode(&dir)?, 0o700);
        assert_eq!(mode(&dir.join("director.sqlite"))?, 0o600);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
        assert!(Store::open(&dir).is_err(), "a readable data dir is refused");
        Ok(())
    }

    #[test]
    fn a_step3_database_gains_revisions() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        private_dir(&dir)?;
        let old = Connection::open(dir.join("director.sqlite"))?;
        old.execute_batch(
            "CREATE TABLE jobs (job_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL, spec TEXT NOT NULL,
               task TEXT NOT NULL, trials INTEGER NOT NULL, bundle_sha256 TEXT NOT NULL, grant_id TEXT,
               grant_sha256 TEXT, meta TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL,
               accepted_at TEXT, finished_at TEXT, run_id TEXT, terminal_row TEXT);
             INSERT INTO jobs VALUES ('j1', 1, 'spec.yaml', 'tasks/t', 1, 'x', NULL, NULL, '{}', 'offered',
               '2026-11-10T00:00:00Z', NULL, NULL, NULL, NULL);",
        )?;
        drop(old);
        let store = Store::open(&dir)?;
        let db = store.db();
        assert_eq!(revision(&db)?, 0);
        assert_eq!(touch(&db, Record::Job, "j1")?, 1);
        assert_eq!(
            touch(&db, Record::Job, "j1")?,
            2,
            "every change a higher revision"
        );
        let (rev, intent): (i64, Option<String>) =
            db.query_row("SELECT revision, intent_id FROM jobs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        assert_eq!((rev, intent), (2, None));
        Ok(())
    }

    #[test]
    fn conversation_lead_marker_backfills_from_spec() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        private_dir(&dir)?;
        let spec_yaml = b"id: hosted-lead\nmodel: claude-code-sonnet\n";
        let sha = sha256_hex(spec_yaml);
        let shard = dir.join("objects").join(&sha[..2]);
        private_dir(&shard)?;
        fs::write(shard.join(&sha), spec_yaml)?;
        let path = dir.join("director.sqlite");
        let old = Connection::open(&path)?;
        old.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE conversations (
               conversation_id TEXT PRIMARY KEY, title TEXT, node TEXT NOT NULL, spec TEXT NOT NULL,
               spec_version TEXT NOT NULL, model TEXT NOT NULL, instructions_tokens INTEGER NOT NULL,
               tools INTEGER NOT NULL, intent_id TEXT, held INTEGER NOT NULL DEFAULT 0,
               created_by TEXT NOT NULL, created_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
               lead INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE conversation_files (
               conversation_id TEXT NOT NULL, path TEXT NOT NULL, sha256 TEXT NOT NULL,
               bytes INTEGER NOT NULL, mode INTEGER NOT NULL, PRIMARY KEY (conversation_id, path));
             INSERT INTO conversations VALUES ('con_old', NULL, 'n1', 'spec.yaml', 'x', 'claude-code-sonnet',
               0, 0, NULL, 0, 'cred_x', 'now', 0, 0);",
        )?;
        old.execute(
            "INSERT INTO conversation_files VALUES ('con_old', 'spec.yaml', ?1, ?2, 420)",
            params![sha, i64::try_from(spec_yaml.len()).unwrap_or(i64::MAX)],
        )?;
        drop(old);
        let store = Store::open(&dir)?;
        let lead: i64 = store.db().query_row(
            "SELECT lead FROM conversations WHERE conversation_id = 'con_old'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(lead, 1, "hosted-lead spec backfills lead marker");
        Ok(())
    }

    #[test]
    fn an_old_land_table_keeps_room_for_an_earlier_attempt() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        private_dir(&dir)?;
        let old = Connection::open(dir.join("director.sqlite"))?;
        old.execute_batch(
            "CREATE TABLE nodes (
               id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, token_hash TEXT NOT NULL UNIQUE,
               created_at TEXT NOT NULL, last_seen_at TEXT, revoked_at TEXT);
             INSERT INTO nodes (id, name, token_hash, created_at) VALUES (1, 'n', 't', 'now');
             CREATE TABLE runs (
               run_id TEXT PRIMARY KEY, node_id INTEGER NOT NULL REFERENCES nodes(id), usage TEXT NOT NULL,
               row_line TEXT, transcript TEXT, verdict TEXT, received_at TEXT NOT NULL);
             INSERT INTO runs (run_id, node_id, usage, received_at) VALUES ('run_1', 1, '{}', 'now');
             CREATE TABLE lands (
               land_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE REFERENCES runs(run_id), state TEXT NOT NULL,
               repo TEXT NOT NULL, lane TEXT NOT NULL, main TEXT NOT NULL, \"commit\" TEXT, reason TEXT,
               landed_by TEXT NOT NULL, landed_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0);
             INSERT INTO lands (land_id, run_id, state, repo, lane, main, reason, landed_by, landed_at)
             VALUES ('lnd_old', 'run_1', 'failed', 'r', 'lane/x', 'main', 'check failed', 'owner', 't1');",
        )?;
        drop(old);
        let store = Store::open(&dir)?;
        let db = store.db();
        let (state, attempts, current): (String, String, i64) = db.query_row(
            "SELECT state, attempts, current FROM lands WHERE land_id = 'lnd_old'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(
            (state.as_str(), attempts.as_str(), current),
            ("failed", "[]", 1)
        );
        db.execute("UPDATE lands SET current = 0 WHERE land_id = 'lnd_old'", [])?;
        db.execute(
            "INSERT INTO lands (land_id, run_id, state, repo, lane, main, \"commit\", reason, landed_by, landed_at, attempts, current)
             VALUES ('lnd_new', 'run_1', 'landed', 'r', 'lane/x', 'main', 'abc', NULL, 'owner', 't2', ?1, 1)",
            params!["[{\"land_id\":\"lnd_old\"}]"],
        )?;
        let n: i64 = db.query_row(
            "SELECT COUNT(*) FROM lands WHERE run_id = 'run_1'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(n, 2);
        drop(db);
        drop(store);
        let again = Store::open(&dir)?;
        let n: i64 = again.db().query_row(
            "SELECT COUNT(*) FROM lands WHERE run_id = 'run_1'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(n, 2, "a second open keeps both rows");
        Ok(())
    }

    #[test]
    fn objects_are_content_addressed() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let sha = sha256_hex(b"evidence");
        assert!(!store.has_object(&sha));
        assert!(store.put_object(b"evidence")?);
        assert!(!store.put_object(b"evidence")?, "a repeat stores nothing");
        assert_eq!(store.read_object(&sha).as_deref(), Some(&b"evidence"[..]));
        assert!(store.read_object("../../etc/passwd").is_none());
        Ok(())
    }

    #[test]
    fn concurrent_immediate_read_write_transactions_both_commit() -> Res {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("director.sqlite");
        let setup = Connection::open(&path)?;
        setup.execute_batch(
            "CREATE TABLE slots (id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
             INSERT INTO slots (v) VALUES (0);",
        )?;
        setup.pragma_update(None, "journal_mode", "WAL")?;
        setup.busy_timeout(std::time::Duration::from_secs(10))?;
        drop(setup);
        // A holds a write transaction; B's write_tx waits for it (busy_timeout) instead of failing at once.
        let mut a = Connection::open(&path)?;
        a.busy_timeout(std::time::Duration::from_secs(10))?;
        let tx = write_tx(&mut a)?;
        tx.execute("UPDATE slots SET v = v + 1 WHERE id = 1", [])?;
        let b_path = path.clone();
        let b = std::thread::spawn(move || -> rusqlite::Result<()> {
            let mut db = Connection::open(&b_path)?;
            db.busy_timeout(std::time::Duration::from_secs(10))?;
            let tx = write_tx(&mut db)?;
            let v: i32 = tx.query_row("SELECT v FROM slots WHERE id = 1", [], |r| r.get(0))?;
            tx.execute("UPDATE slots SET v = ?1 WHERE id = 1", params![v + 10])?;
            tx.commit()
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        tx.commit()?;
        b.join().map_err(|_| "thread b panicked")??;
        let db = Connection::open(&path)?;
        let v: i32 = db.query_row("SELECT v FROM slots WHERE id = 1", [], |r| r.get(0))?;
        assert_eq!(v, 11);
        Ok(())
    }

    #[test]
    fn the_same_object_stored_at_once() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = std::sync::Arc::new(Store::open(&tmp.path().join("data"))?);
        let writers: Vec<_> = (0..8)
            .map(|_| {
                let s = std::sync::Arc::clone(&store);
                std::thread::spawn(move || s.put_object(b"same bytes"))
            })
            .collect();
        for w in writers {
            assert!(w.join().map_err(|_| "a writer panicked")?.is_ok());
        }
        assert!(store.has_object(&sha256_hex(b"same bytes")));
        Ok(())
    }
}
