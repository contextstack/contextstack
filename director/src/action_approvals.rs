//! Director-owned action requests. Node claims are consumed before execution.

use crate::access::Client;
use crate::policy::OWNER_ONLY_APPROVAL;
use crate::records::View;
use crate::server::{Call, Reply, State, internal, json, refusal};
use crate::store::ScopedConnection;
use crate::store::{Record, touch};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};

pub(crate) mod browser;
mod hard_lines;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS action_approvals (
    id TEXT PRIMARY KEY, job_id TEXT NOT NULL REFERENCES jobs(job_id), run_id TEXT NOT NULL,
    request TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', created_at TEXT NOT NULL,
    reviewer_model TEXT, verdict TEXT, review_reason TEXT, reviewed_at TEXT,
    decided_by TEXT, decided_at TEXT, outcome TEXT, claimed_at TEXT,
    UNIQUE(job_id, run_id, request)
);
CREATE INDEX IF NOT EXISTS action_approvals_job ON action_approvals(job_id);
CREATE INDEX IF NOT EXISTS action_approvals_status ON action_approvals(status,created_at);
";

pub(crate) const VIEW: View = View {
    select: "SELECT id, job_id, revision, conversation_id FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) AS action_approvals",
    key: "id",
    order: "revision DESC",
    shape: |_, row| {
        let mut value = json!({"id": row.get::<_, String>(0)?, "revision": row.get::<_, i64>(2)?});
        if let Some(conversation) = row.get::<_, Option<String>>(3)? {
            value["conversation_id"] = json!(conversation);
        } else {
            value["job_id"] = json!(row.get::<_, String>(1)?);
        }
        Ok(value)
    },
};

const RECORD: &str = "json_patch(json_remove(request,'$.owner_only'),json_object('id',id,'job_id',job_id,'run_id',run_id,'status',status,
    'conversation_id',conversation_id,'created_at',created_at,'revision',revision,'reviewer_model',reviewer_model,'verdict',verdict,'review_reason',review_reason,
    'decided_by',decided_by,'decided_at',decided_at,'outcome',json(outcome),'claimed_at',claimed_at))";
const CLIENT_RECORD: &str = "json_object('assignee_member_id',assigned_member_id,
    'intent_id',(SELECT j.intent_id FROM (SELECT * FROM jobs WHERE +org_id = :org_id) j
                 WHERE j.job_id=action_approvals.job_id))";
const LEAD_STATUS: &[&str] = &[
    "id",
    "job_id",
    "intent_id",
    "conversation_id",
    "run_id",
    "status",
    "revision",
    "verdict",
    "outcome",
    "kind",
    "commit",
    "base",
    "target_run_id",
];

fn client_record(
    db: &ScopedConnection,
    mut row: Value,
    client: &Client,
) -> rusqlite::Result<Value> {
    if row["kind"] == "browser_action"
        && let Some(fields) = row.as_object_mut()
    {
        fields.remove("execution");
        browser::add_warning(&mut row);
    }
    if client.kind == crate::access::Kind::Lead
        && let Some(fields) = row.as_object_mut()
    {
        fields.retain(|key, _| LEAD_STATUS.contains(&key.as_str()));
    }
    crate::projections::names(db, &mut row)?;
    let open = row["status"] == "needs_owner";
    crate::projections::approval_actions(db, &mut row, open);
    Ok(row)
}

fn client_detail(db: &ScopedConnection, id: &str, client: &Client) -> Reply {
    let row = db
        .query_row(
            &format!(
                "SELECT json_patch({RECORD},{CLIENT_RECORD}) FROM action_approvals
                  WHERE +org_id=:org_id AND id=?1 AND (?2 IS NULL OR (assigned_member_id=?2 AND NOT ({OWNER_ONLY_APPROVAL})))"
            ),
            params![id, crate::policy::approval_filter(client)],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .and_then(|row| {
            row.map(|raw| client_record(db, serde_json::from_str(&raw).unwrap_or_default(), client))
                .transpose()
        });
    match row {
        Ok(Some(row)) => json(200, &row),
        Ok(None) => missing(),
        Err(error) => internal(&error),
    }
}

// json_patch removes null fields. Clients can treat absent optional fields as null.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(18);
// Allow the gateway one second to return its response after its 10-second model budget.
const REVIEW_CALL_TIMEOUT: Duration = Duration::from_secs(11);
// Attempts that reached a model; fast gateway refusals do not count, the deadline still bounds them.
const MAX_REVIEW_ATTEMPTS: usize = 3;

fn bad(detail: &str) -> Reply {
    refusal(422, "invalid_request", detail, Value::Null)
}

fn missing() -> Reply {
    refusal(404, "not_found", "no such action approval", Value::Null)
}

fn conflict() -> Reply {
    refusal(
        409,
        "conflict",
        "the action approval is no longer in the required state",
        Value::Null,
    )
}

pub fn expire_reviews(db: &ScopedConnection) -> rusqlite::Result<()> {
    let mut stmt = db.prepare_cached(
        "SELECT id FROM action_approvals WHERE +org_id = :org_id AND status='pending' AND job_id IS NOT NULL AND created_at < ?1",
    )?;
    let expired = stmt
        .query_map([crate::util::utc_in(-20)], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let verdict = no_go("The action reviewer timed out.");
    for id in expired {
        finish_review(db, &id, &verdict)?;
    }
    browser::expire_jobs(db)?;
    expire_land_requests(db)
}

pub(crate) fn expire_land_requests(db: &ScopedConnection) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    expire_land_in(&tx)?;
    tx.commit()
}

fn expire_land_in(db: &ScopedConnection) -> rusqlite::Result<()> {
    expire_open(
        db,
        "SELECT id FROM action_approvals WHERE +org_id=:org_id AND status='needs_owner'
         AND json_extract(request,'$.kind')='browser_action'
         AND (COALESCE(json_extract(request,'$.expires_at'),'') <= ?1
              OR json_type(request,'$.values_summary') IS NOT 'array')",
        params![crate::util::now_utc()],
    )?;
    expire_open(
        db,
        "SELECT id FROM action_approvals WHERE +org_id = :org_id AND conversation_id IS NOT NULL
         AND ((status='pending' AND created_at < ?1)
              OR (status='needs_owner' AND reviewed_at < ?2)
              OR (status='approved' AND claimed_at IS NULL
                  AND COALESCE(decided_at,reviewed_at,created_at) < ?2))",
        params![crate::util::utc_in(-300), crate::util::utc_in(-86400)],
    )?;
    crate::lead_install::expire_claims(db)
}

/// Pending and `needs_owner` approvals for a job that has ended; clients drop them on the change events.
pub fn expire_for_job(db: &ScopedConnection, job: &str) -> rusqlite::Result<usize> {
    expire_open(
        db,
        "SELECT id FROM action_approvals
         WHERE +org_id = :org_id AND job_id=?1 AND status IN ('pending','needs_owner')
           AND json_type(request,'$.execution') IS NULL",
        params![job],
    )
}

/// At Director start: clear open approvals whose jobs are already `done` or `error`.
pub fn expire_ended_jobs(db: &ScopedConnection) -> rusqlite::Result<usize> {
    let tx = db.unchecked_transaction()?;
    let changed = expire_open(
        &tx,
        "SELECT a.id FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) a JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id=a.job_id
         WHERE a.status IN ('pending','needs_owner') AND j.state IN ('done','error')
           AND json_type(a.request,'$.execution') IS NULL",
        params![],
    )?;
    tx.commit()?;
    Ok(changed)
}

fn expire_open<P: IntoIterator<Item: rusqlite::ToSql>>(
    db: &ScopedConnection,
    select: &str,
    params: P,
) -> rusqlite::Result<usize> {
    let ids = db
        .prepare(select)?
        .query_map(params, |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut changed = 0;
    for id in ids {
        if mark_expired(db, &id)? {
            changed += 1;
        }
    }
    Ok(changed)
}

/// Updates in the caller's transaction (job terminal) or autocommit (unit tests / per-statement).
fn mark_expired(db: &ScopedConnection, id: &str) -> rusqlite::Result<bool> {
    let prior: Option<String> = db
        .query_row(
            "SELECT status FROM action_approvals WHERE +org_id = :org_id AND id=?1
             AND (status IN ('pending','needs_owner')
                  OR (conversation_id IS NOT NULL AND status='approved' AND claimed_at IS NULL))",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(prior) = prior else {
        return Ok(false);
    };
    let changed = db.execute(
        "UPDATE action_approvals SET status='expired',
        review_reason=CASE WHEN json_extract(request,'$.kind')='browser_action' THEN ?4
                           WHEN conversation_id IS NOT NULL THEN
                           CASE json_extract(request,'$.kind') WHEN 'install' THEN 'install request expired'
                             WHEN 'rollback' THEN 'rollback request expired' ELSE 'The land request expired.' END
                           ELSE COALESCE(NULLIF(review_reason,''),'job ended') END,
        outcome=json_object('status','expired','reason',
                            CASE WHEN json_extract(request,'$.kind')='browser_action' THEN ?4
                            WHEN conversation_id IS NOT NULL THEN CASE json_extract(request,'$.kind') WHEN 'install' THEN 'install request expired'
                             WHEN 'rollback' THEN 'rollback request expired' ELSE 'land request timed out' END ELSE 'job ended' END),
        decided_at=?2
        WHERE +org_id = :org_id AND id=?1 AND status=?3 AND claimed_at IS NULL",
        params![id, crate::util::now_utc(), prior, browser::EXPIRED],
    )? > 0;
    if changed {
        touch(db, Record::ActionApproval, id)?;
        if prior == "needs_owner" {
            touch_conversations(db, id)?;
        }
    }
    Ok(changed)
}

pub(crate) fn record(db: &ScopedConnection, id: &str) -> rusqlite::Result<Option<Value>> {
    db.query_row(
        &format!("SELECT {RECORD} FROM action_approvals WHERE +org_id = :org_id AND id=?1"),
        [id],
        |row| {
            let raw: String = row.get(0)?;
            Ok(serde_json::from_str(&raw).unwrap_or_default())
        },
    )
    .optional()
}

pub(crate) fn reply(db: &ScopedConnection, id: &str) -> Reply {
    match record(db, id) {
        Ok(Some(row)) => json(200, &row),
        Ok(None) => missing(),
        Err(error) => internal(&error),
    }
}

pub fn client(state: &State, call: &Call, client: &Client, parts: &[&str]) -> Reply {
    let mut db = state.store.db();
    match (call.method.as_str(), parts) {
        ("GET", ["browser-action-screenshots", sha]) => browser::screenshot(state, &db, sha),
        ("GET", ["action-approvals"]) => {
            let result = (|| {
                let mut sql = format!(
                    "SELECT json_patch({RECORD},{CLIENT_RECORD}) FROM action_approvals
                     WHERE +org_id = :org_id AND (?1 IS NULL OR status=?1) AND (?2 IS NULL OR job_id=?2)
                     AND (?3 IS NULL OR assigned_member_id=?3)"
                );
                let mut bind = vec![
                    call.query("status").map(String::from).into(),
                    call.query("job").map(String::from).into(),
                    crate::policy::approval_filter(client)
                        .map(String::from)
                        .into(),
                ];
                if crate::policy::approval_filter(client).is_some() {
                    let _ = write!(sql, " AND NOT ({OWNER_ONLY_APPROVAL})");
                }
                if client.kind == crate::access::Kind::Lead {
                    sql.push_str(" AND (conversation_id = ? OR (conversation_id IS NULL");
                    bind.push(client.conversation_id.clone().into());
                    crate::lead::push_job_scope(
                        client,
                        "action_approvals.job_id",
                        &mut sql,
                        &mut bind,
                    );
                    sql.push_str("))");
                }
                if let Some(conversation) = call.query("conversation") {
                    let _ = write!(
                        sql,
                        " AND id IN (SELECT a.id FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) a
                         LEFT JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id=a.job_id
                         LEFT JOIN (SELECT * FROM intents WHERE +org_id = :org_id) i ON i.intent_id=j.intent_id
                         JOIN (SELECT * FROM conversations WHERE +org_id = :org_id) c ON {CONVERSATION_LINK}
                         WHERE c.conversation_id = ?)"
                    );
                    bind.push(conversation.to_owned().into());
                }
                sql.push_str(
                    " ORDER BY status='needs_owner' DESC, created_at DESC, id DESC LIMIT 500",
                );
                db.prepare(&sql)?
                    .query_map(bind, |row| {
                        let raw: String = row.get(0)?;
                        client_record(&db, serde_json::from_str(&raw).unwrap_or_default(), client)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })();
            match result {
                Ok(rows) => json(200, &json!({"action_approvals":rows})),
                Err(error) => internal(&error),
            }
        }
        ("GET", ["action-approvals", id]) => client_detail(&db, id, client),
        ("POST", ["action-approvals", id, "clear"]) => {
            match record(&db, id) {
                Ok(Some(row)) if row["kind"] == "browser_action" => {
                    return browser::clear(&mut db, id, client);
                }
                Err(error) => return internal(&error),
                _ => {}
            }
            crate::lead_install::clear(&mut db, id, client.member_id(), false)
        }
        ("POST", ["action-approvals", id, operation @ ("decide" | "answer")]) => {
            decide_client(&mut db, call, client, id, operation)
        }
        _ => missing(),
    }
}

fn decide_client(
    db: &mut ScopedConnection,
    call: &Call,
    client: &Client,
    id: &str,
    operation: &str,
) -> Reply {
    let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
    if operation == "answer" && body["choice"] == "acknowledge" {
        return crate::lead_install::clear(db, id, client.member_id(), true);
    }
    let (status, actor) = if operation == "answer" {
        match body["choice"].as_str() {
            Some("go") => ("approved", client.member_id()),
            Some("no_go") => ("denied", client.member_id()),
            _ => return bad("choice must be go or no_go"),
        }
    } else {
        match body["decision"].as_str() {
            Some("approve") => ("approved", client.id.as_str()),
            Some("deny") => ("denied", client.id.as_str()),
            _ => return bad("decision must be approve or deny"),
        }
    };
    let tx = match crate::store::write_tx(db) {
        Ok(tx) => tx,
        Err(error) => return internal(&error),
    };
    if status == "approved" {
        match record(&tx, id) {
            Ok(Some(row)) if row["kind"] == "land" && row["commit"].is_null() => {
                return refusal(
                    409,
                    "conflict",
                    "The node could not pin a local commit and base. Request landing again after checking the lane.",
                    Value::Null,
                );
            }
            Err(error) => return internal(&error),
            _ => {}
        }
    }
    if let Err(error) = expire_land_in(&tx) {
        return internal(&error);
    }
    match decide(&tx, id, status, actor).and_then(|changed| tx.commit().map(|()| changed)) {
        Ok(true) => client_detail(db, id, client),
        Ok(false) => match record(db, id) {
            Ok(Some(_)) => conflict(),
            Ok(None) => missing(),
            Err(error) => internal(&error),
        },
        Err(error) => internal(&error),
    }
}

fn decide(db: &ScopedConnection, id: &str, status: &str, owner: &str) -> rusqlite::Result<bool> {
    let changed = db.execute(
        "UPDATE action_approvals SET status=?2,decided_by=?3,decided_at=?4,
        outcome=CASE WHEN ?2='denied' THEN json_object('status','denied') END
        WHERE +org_id = :org_id AND (id=?1 AND status='needs_owner')",
        params![id, status, owner, crate::util::now_utc()],
    )? > 0;
    if changed {
        if status == "approved" {
            browser::enqueue(db, id)?;
        }
        touch(db, Record::ActionApproval, id)?;
        touch_conversations(db, id)?;
    }
    Ok(changed)
}

fn strings(value: &Value) -> Option<Vec<&str>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().filter(|s| !s.contains('\0')))
        .collect()
}

fn loopback_port(need: &str) -> Option<u16> {
    let port = need.strip_prefix("network:loopback:")?;
    if port.starts_with('0') || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>()
        .ok()
        .filter(|port| !matches!(port, 0 | 8780 | 9200))
}

fn normalized(input: &Value) -> Option<Value> {
    let argv = strings(&input["argv"])?;
    let needs = strings(&input["needs"])?;
    let paths = input.get("write_paths").map_or(Some(Vec::new()), strings)?;
    let reason = input["reason"].as_str()?.trim();
    let model = input["requesting_model"].as_str()?;
    if argv.first().is_none_or(|exe| exe.is_empty())
        || needs.is_empty()
        || reason.is_empty()
        || model.is_empty()
        || needs.iter().any(|n| {
            !matches!(*n, "network" | "write_paths" | "delete") && loopback_port(n).is_none()
        })
        || (needs.contains(&"network") && needs.iter().any(|need| loopback_port(need).is_some()))
        || needs.contains(&"write_paths") == paths.is_empty()
        || paths.iter().any(|p| !Path::new(p).is_absolute())
    {
        return None;
    }
    let mut request = json!({"argv":argv,"needs":needs,"write_paths":paths,"reason":reason,"requesting_model":model});
    if let Some(digest) = input.get("script_sha256") {
        let digest = digest.as_str()?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        request["script_sha256"] = json!(digest);
    }
    Some(request)
}

fn sensitive(word: &str) -> bool {
    word.split(['/', '\\', '=', ':', ' ', '\t', '\n'])
        .any(|part| {
            let part = part.to_ascii_lowercase();
            part.contains("credential")
                || part.contains("token")
                || part.contains("secret")
                || Path::new(&part).extension().is_some_and(|ext| {
                    ["key", "pem", "p12", "pfx"]
                        .iter()
                        .any(|name| ext.eq_ignore_ascii_case(name))
                })
                || matches!(
                    part.as_str(),
                    ".ssh"
                        | ".gnupg"
                        | ".aws"
                        | ".kube"
                        | "keychains"
                        | ".netrc"
                        | ".npmrc"
                        | ".git"
                        | ".contextstack"
                        | ".env"
                        | ".envrc"
                        | "id_rsa"
                        | "id_ed25519"
                )
                || [".env.", ".env_", ".env-"]
                    .iter()
                    .any(|prefix| part.starts_with(prefix))
        })
}

fn inside(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    })
}

fn protected_path(word: &str, workspace: &Path, tmp: Option<&Path>) -> bool {
    let path = Path::new(word);
    // The trusted node's lane roots may live below the state directory.
    for root in [Some(workspace), tmp].into_iter().flatten() {
        if let Ok(relative) = path.strip_prefix(root) {
            return sensitive(&relative.to_string_lossy());
        }
    }
    sensitive(word)
}

fn system_executable(path: &Path) -> bool {
    [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/opt/homebrew/bin",
        "/opt/homebrew/Cellar",
    ]
    .iter()
    .any(|root| inside(path, Path::new(root)))
}

/// Hard lines: the action waits on a human (an owner or the assigned admin; browser actions only on
/// an owner) without a model review. See `hard_lines` and the
/// "Action approval hard lines" section of `director/README.md`. In order:
/// 1. Writing to a Git remote: any `git` or `gh` invocation that is not on the read-only list.
/// 2. A workspace that cannot be shown to lie inside the user's home.
/// 3. A credential, key, token or protected configuration path, by name anywhere in a path, or a
///    credential store under the home (after expanding `~`/`$HOME`, `..` and symlinks).
/// 4. A path outside the user's home (system executables excepted) or containing `..`.
fn hard_line_reason(request: &Value, context: &Value) -> Option<String> {
    if request["kind"] == "browser_action" {
        return Some(browser::REASON.into());
    }
    let argv = strings(&request["argv"]).unwrap_or_default();
    let paths = strings(&request["write_paths"]).unwrap_or_default();
    let resolved_executable = context["resolved_executable"].as_str();
    if let Some(reason) = hard_lines::remote_write(&argv, resolved_executable) {
        return Some(reason);
    }
    let (Some(home), Some(workspace)) = (context["home"].as_str(), context["workspace"].as_str())
    else {
        return Some("The workspace cannot be verified as lying inside the user's home.".into());
    };
    let (home, workspace) = (Path::new(home), Path::new(workspace));
    if !home.is_absolute() || home.parent().is_none() || !inside(workspace, home) {
        return Some("The workspace cannot be verified as lying inside the user's home.".into());
    }
    let tmp = context["job_tmp"].as_str().map(Path::new);
    let operands = strings(&context["resolved_operands"]).unwrap_or_default();
    let mut arguments = argv.iter().flat_map(|word| {
        [
            Some(*word),
            word.strip_prefix('-')
                .and_then(|flag| flag.split_once('=').map(|(_, value)| value)),
        ]
        .into_iter()
        .flatten()
    });
    if arguments
        .clone()
        .chain(paths.iter().copied())
        .chain(operands.iter().copied())
        .chain(resolved_executable)
        .any(|word| protected_path(word, workspace, tmp))
        || {
            let roots = [Some(workspace), tmp]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let listing = hard_lines::shallow_listing(&argv, resolved_executable);
            hard_lines::credential_store(
                argv.iter().chain(&operands).copied(),
                home,
                &roots,
                listing,
            ) || hard_lines::credential_store(
                paths.iter().copied().chain(resolved_executable),
                home,
                &roots,
                false,
            )
        }
    {
        return Some(
            "The action references a credential, key, token, or protected configuration path."
                .into(),
        );
    }
    if resolved_executable.is_some_and(|word| {
        let path = Path::new(word);
        !inside(path, home) && !system_executable(path)
    }) || paths.iter().chain(&operands).any(|word| {
        let path = Path::new(word);
        path.is_absolute() && !inside(path, home)
    }) || arguments.any(|word| {
        let path = Path::new(word);
        (path.is_absolute()
            && !inside(path, home)
            && !(argv.first() == Some(&word) && system_executable(path)))
            || path.components().any(|part| part == Component::ParentDir)
    }) {
        return Some("The action references a path outside the user's home.".into());
    }
    None
}

fn review_scope(request: &Value, context: &Value) -> &'static str {
    let needs = strings(&request["needs"]).unwrap_or_default();
    let paths = strings(&request["write_paths"]).unwrap_or_default();
    let roots = ["workspace", "job_tmp"].map(|key| context[key].as_str().map(Path::new));
    if needs.contains(&"network")
        || needs.contains(&"delete")
        || paths.iter().any(|path| {
            !roots
                .into_iter()
                .flatten()
                .any(|root| inside(Path::new(path), root))
        })
    {
        "broader_widening"
    } else {
        "routine_widening"
    }
}

struct Verdict {
    word: &'static str,
    reason: String,
}

fn no_go(reason: &str) -> Verdict {
    Verdict {
        word: "no_go",
        reason: reason.into(),
    }
}

/// End index (inclusive) of the balanced `{…}` object that begins at `start`, or none if it never closes.
fn balanced_object_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    if start >= bytes.len() || bytes[start] != b'{' {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Top-level JSON objects in document order, including those wrapped in fences or prose.
fn json_objects(raw: &str) -> Vec<Value> {
    let mut objects = Vec::new();
    let mut index = 0;
    let bytes = raw.as_bytes();
    while index < bytes.len() {
        if bytes[index] != b'{' {
            index += 1;
            continue;
        }
        let Some(end) = balanced_object_end(raw, index) else {
            break;
        };
        if let Ok(value) = serde_json::from_str::<Value>(&raw[index..=end]) {
            objects.push(value);
        }
        index = end + 1;
    }
    objects
}

fn truncate_reason(reason: &str) -> String {
    reason.chars().take(800).collect()
}

/// None when the reviewer's answer holds no well-formed verdict; the caller tries the next reviewer.
/// Accepts plain JSON, a fenced block, or leading/trailing prose: the first balanced top-level object
/// must carry `verdict` ∈ {`go`, `no_go`} and a non-empty `reason` (truncated at 800 characters). A later
/// object with a conflicting verdict makes the answer invalid.
fn parse_verdict(answer: &Value) -> Option<Verdict> {
    let raw = answer["message"]["content"].as_str()?;
    let objects = json_objects(raw);
    let first = objects.first()?;
    let reason = first["reason"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(truncate_reason)?;
    let word = match first["verdict"].as_str() {
        Some("go") => "go",
        Some("no_go") => "no_go",
        _ => return None,
    };
    for other in &objects[1..] {
        match other["verdict"].as_str() {
            Some(next) if matches!(next, "go" | "no_go") && next != word => return None,
            _ => {}
        }
    }
    Some(Verdict { word, reason })
}

fn select_reviewer(models: &Value, requester: &str) -> Vec<String> {
    let Some(models) = models["models"].as_array() else {
        return Vec::new();
    };
    let source = models
        .iter()
        .find(|row| row["name"] == requester)
        .and_then(|row| row["provider"].as_str());
    let mut candidates: Vec<_> = models
        .iter()
        .filter_map(|row| {
            let name = row["name"].as_str()?;
            let provider = row["provider"].as_str()?;
            (name != requester
                && row["available"] == true
                && row["candidate"] != true
                && row["executor"] != "cli"
                && row["cli_binary"].is_null())
            .then_some((
                name,
                (Some(provider) == source, row["cost_class"] != "free_local"),
            ))
        })
        .collect();
    // Stable: within a priority the gateway's catalog order stands (tiers differ per provider).
    candidates.sort_by_key(|(_, priority)| *priority);
    candidates
        .into_iter()
        .map(|(name, _)| name.to_owned())
        .collect()
}

async fn review(state: Arc<State>, id: String, request: Value, context: Value) {
    // Publish the pending request before a fast verdict can replace it in the same batch.
    state.hub.publish(&state.store);
    let verdict = review_request(
        &request,
        &context,
        model_review(&state, &id, &request, &context),
    )
    .await;
    let updated = finish_review(&state.store.db(), &id, &verdict);
    match updated {
        Ok(()) => state.hub.publish(&state.store),
        Err(error) => crate::logf!("action approval {id}: verdict could not be recorded: {error}"),
    }
}

fn update_approval(
    db: &ScopedConnection,
    id: &str,
    sql: &str,
    value: &str,
) -> rusqlite::Result<bool> {
    let tx = db.unchecked_transaction()?;
    let changed = tx.execute(sql, params![id, value])? == 1;
    if changed {
        touch(&tx, Record::ActionApproval, id)?;
    }
    tx.commit()?;
    Ok(changed)
}

fn select_model(db: &ScopedConnection, id: &str, model: &str) -> rusqlite::Result<bool> {
    update_approval(
        db,
        id,
        "UPDATE action_approvals SET reviewer_model=?2 WHERE +org_id = :org_id AND (id=?1 AND status='pending')",
        model,
    )
}

// Only bounded identifiers and structured budget reasons are safe to report.
fn error_identifier(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    })
}

fn reviewer_failure(status: u16, answer: &Value) -> (String, String) {
    let error = &answer["error"];
    if let Some(code) = error_identifier(&error["code"]).filter(|code| code.starts_with("gateway."))
    {
        let budget = &error["budget"];
        let detail = match (
            code,
            error_identifier(&budget["source"]),
            budget["reason"].as_str(),
        ) {
            (
                "gateway.policy_denied",
                Some(source),
                Some(reason @ ("off" | "exhausted" | "ceiling" | "invalid")),
            ) => {
                format!("{code}: {source} {reason}")
            }
            _ => code.to_owned(),
        };
        return (code.to_owned(), format!("refused ({detail})"));
    }
    if status == 200 && answer["status"] == "ok" {
        return (
            "model_mismatch".into(),
            "answered with a different model".into(),
        );
    }
    if status == 200 {
        return (
            "invalid_response".into(),
            "failed without a gateway error code".into(),
        );
    }
    (format!("http_{status}"), format!("failed (HTTP {status})"))
}

async fn model_review(state: &State, id: &str, request: &Value, context: &Value) -> Verdict {
    use tokio::time::{Instant, timeout_at};

    let deadline = Instant::now() + REVIEW_TIMEOUT;
    let gateway = state.settings.gateway.clone();
    let requester = request["requesting_model"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let catalog = tokio::task::spawn_blocking(move || {
        let (status, rows) = gateway.get("/v1/models?available_only=false").ok()?;
        (status == 200).then(|| select_reviewer(&rows, &requester))
    });
    let models = match timeout_at(deadline, catalog).await {
        Ok(Ok(Some(models))) if !models.is_empty() => models,
        Ok(Ok(Some(_))) => return no_go("No independent action reviewer is available."),
        Ok(_) => {
            return no_go("No action reviewer could run: the gateway model list was unavailable.");
        }
        Err(_) => return no_go("No action reviewer could run: the gateway model list timed out."),
    };

    let body = json!({
        "schema_version": "agent.gateway.model_call.v2",
        "run_id": id,
        "role": {"role_pack_name":"cs","role_pack_version":"0","role_name":"action-reviewer"},
        "messages": [
            {"role":"system","content":include_str!("../../cs/agents/action-reviewer.md")},
            {"role":"user","content":json!({"request":request,"lane":context,"scope":review_scope(request, context)}).to_string()}
        ],
        "budget": {"timeout_ms":10000,"max_output_tokens":512,"max_usd":"0.100000"}
    });
    let mut failures = Vec::new();
    let mut reached = 0;
    for (index, model) in models.into_iter().enumerate() {
        let now = Instant::now();
        if now >= deadline || reached >= MAX_REVIEW_ATTEMPTS {
            break;
        }
        let attempt = index + 1;
        let mut body = body.clone();
        body["step_id"] = json!(attempt);
        body["request_id"] = json!(format!("{id}-{attempt}"));
        body["capability"] = json!({"kind":"generic","model_tier_hint":model});
        let gateway = state.settings.gateway.clone();
        let call = tokio::task::spawn_blocking(move || gateway.model_call(&body));
        let (code, reason) = match timeout_at(deadline.min(now + REVIEW_CALL_TIMEOUT), call).await {
            Ok(Ok(Ok((200, answer)))) if answer["status"] == "ok" && answer["model"] == model => {
                if let Some(verdict) = parse_verdict(&answer) {
                    if !matches!(select_model(&state.store.db(), id, &model), Ok(true)) {
                        return no_go("The Director could not record the reviewer.");
                    }
                    return verdict;
                }
                reached += 1;
                ("invalid_verdict".into(), "returned no valid verdict".into())
            }
            Ok(Ok(Ok((status, answer)))) => {
                // A refusal before any model ran is cheap; only answers from a model use an attempt.
                if !answer["error"]["code"].as_str().is_some_and(|code| {
                    matches!(
                        code,
                        "gateway.policy_denied" | "gateway.provider_request_rejected"
                    )
                }) {
                    reached += 1;
                }
                reviewer_failure(status, &answer)
            }
            Ok(_) => (
                "gateway_unavailable".into(),
                "could not reach the gateway".into(),
            ),
            Err(_) => {
                reached += 1;
                ("timeout".into(), "timed out".into())
            }
        };
        crate::logf!("action approval {id}: reviewer attempt {attempt} failed ({code})");
        let name: String = model
            .chars()
            .filter(char::is_ascii_graphic)
            .take(80)
            .collect();
        failures.push(format!("{name} {reason}"));
    }
    if failures.is_empty() {
        return no_go("No action reviewer could run: the review timed out.");
    }
    no_go(&format!(
        "No action reviewer could run: {}.",
        failures.join("; ")
    ))
}

async fn review_request(
    request: &Value,
    context: &Value,
    reviewer: impl std::future::Future<Output = Verdict>,
) -> Verdict {
    match hard_line_reason(request, context) {
        Some(reason) => no_go(&reason),
        None => reviewer.await,
    }
}

fn finish_review(db: &ScopedConnection, id: &str, verdict: &Verdict) -> rusqlite::Result<()> {
    let tx = db.unchecked_transaction()?;
    let status = if verdict.word == "go" {
        "approved"
    } else {
        "needs_owner"
    };
    let changed = tx.execute(
        "UPDATE action_approvals SET status=?2,verdict=?3,review_reason=?4,reviewed_at=?5,
        outcome=CASE WHEN ?2='needs_owner' THEN json_object('status','waiting_on_owner') END
        WHERE +org_id = :org_id AND (id=?1 AND status='pending')",
        params![
            id,
            status,
            verdict.word,
            verdict.reason,
            crate::util::now_utc()
        ],
    )?;
    if changed > 0 {
        touch(&tx, Record::ActionApproval, id)?;
        if status == "needs_owner" {
            touch_conversations(&tx, id)?;
        }
    }
    tx.commit()
}

fn submit(
    state: &Arc<State>,
    db: &ScopedConnection,
    body: &Value,
    job: &str,
    run: &str,
    token: &str,
) -> Reply {
    let browser = body["kind"] == "browser_action";
    let request = if browser {
        browser::request(&body["capture"], state.settings.browser_action_ttl_seconds)
    } else {
        normalized(body)
    };
    let Some(mut request) = request else {
        if browser {
            return bad("a native host browser capture is required");
        }
        return bad(
            "valid argv, needs, write_paths, requesting_model and reason are required; name each localhost port as network:loopback:<port> and exclude reserved ports",
        );
    };
    if browser {
        let Some(image) = browser::verified_capture(body, token, job, run) else {
            return bad("invalid or expired native host browser capture");
        };
        match browser::declared(&state.store, db, job, &request, &body["capture"]) {
            Ok(Some(execution)) => request["execution"] = execution,
            Ok(None) => return bad("the job must declare this browser action"),
            Err(error) => return internal(&error),
        }
        if let Err(error) = state.store.put_browser_screenshot(
            request["screenshot_sha"].as_str().unwrap_or_default(),
            &image,
        ) {
            return internal(&error);
        }
    }
    let context = body["context"].clone();
    let id = match crate::util::ulid() {
        Ok(id) => id,
        Err(error) => return internal(&error),
    };
    let encoded = request.to_string();
    let inserted = (|| -> rusqlite::Result<_> {
        let tx = db.unchecked_transaction()?;
        let assignee = if browser {
            crate::sponsorship::active_owner(&tx, None)?
        } else {
            crate::sponsorship::job_assignee(&tx, job)?
        };
        let reason = browser.then_some(browser::REASON);
        let inserted = tx.execute(
            "INSERT INTO action_approvals (org_id,id,job_id,run_id,request,created_at,assigned_member_id,
                status,verdict,review_reason,reviewed_at,outcome)
             SELECT :org_id,?1,?2,?3,?4,?5,?6,
                CASE WHEN ?7 IS NULL THEN 'pending' ELSE 'needs_owner' END,
                CASE WHEN ?7 IS NOT NULL THEN 'no_go' END,?7,
                CASE WHEN ?7 IS NOT NULL THEN ?5 END,
                CASE WHEN ?7 IS NOT NULL THEN json_object('status','waiting_on_owner') END
             WHERE NOT EXISTS (
                SELECT 1 FROM action_approvals WHERE +org_id=:org_id AND job_id=?2 AND run_id=?3
                 AND json_remove(request,'$.followup_job_id')=json(?4))
             ON CONFLICT(job_id,run_id,request) DO NOTHING",
            params![id, job, run, encoded, crate::util::now_utc(), assignee, reason],
        )?;
        if inserted == 1 {
            touch(&tx, Record::ActionApproval, &id)?;
            if browser {
                touch_conversations(&tx, &id)?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    })();
    match inserted {
        Ok(1) => {
            let answer = reply(db, &id);
            if !browser {
                tokio::spawn(review(Arc::clone(state), id, request, context));
            }
            answer
        }
        Ok(_) => match db.query_row(
            "SELECT id FROM action_approvals WHERE +org_id=:org_id AND job_id=?1 AND run_id=?2
             AND json_remove(request,'$.followup_job_id')=json(?3)",
            params![job, run, encoded],
            |row| row.get::<_, String>(0),
        ) {
            Ok(id) => reply(db, &id),
            Err(error) => internal(&error),
        },
        Err(error) => internal(&error),
    }
}

pub fn node(state: &Arc<State>, call: &Call, node: i64, job: &str, parts: &[&str]) -> Reply {
    let body: Value = serde_json::from_slice(&call.body).unwrap_or_default();
    let Some(run) = body["run_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() < 128)
    else {
        return bad("run_id is required");
    };
    let db = state.store.db();
    let job_state = db
        .query_row(
            "SELECT state FROM jobs WHERE +org_id = :org_id AND job_id=?1 AND node_id=?2",
            params![job, node],
            |row| row.get::<_, String>(0),
        )
        .optional();
    match job_state {
        Ok(Some(status))
            if status == "accepted"
                || matches!(parts, [_, "outcome"])
                || (status == "waiting" && matches!(parts, [_, "claim"])) => {}
        Ok(Some(_)) => return conflict(),
        Ok(None) => return missing(),
        Err(error) => return internal(&error),
    }
    if parts.is_empty() {
        return submit(
            state,
            &db,
            &body,
            job,
            run,
            call.bearer().unwrap_or_default(),
        );
    }
    if parts == ["capture"] {
        return browser::preflight(&state.store, &db, job, &body);
    }
    let [id, op] = parts else {
        return missing();
    };
    let found = db
        .query_row(
            "SELECT status,request FROM action_approvals WHERE +org_id=:org_id AND id=?1
             AND ((job_id=?2 AND run_id=?3 AND json_type(request,'$.execution') IS NULL)
                OR (json_extract(request,'$.followup_job_id')=?2 AND ?2=?3)
                OR (?4='poll' AND job_id=?2 AND run_id=?3))",
            params![id, job, run, op],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional();
    let (status, request) = match found {
        Ok(Some(status)) => status,
        Ok(None) => return missing(),
        Err(error) => return internal(&error),
    };
    let request: Value = serde_json::from_str(&request).unwrap_or_default();
    match *op {
        "poll" => reply(&db, id),
        "claim" if request["kind"] == "browser_action" => {
            browser::claim_reply(&db, id, &body, &request, call.bearer())
        }
        "claim" if status == "approved" => match update_approval(
            &db,
            id,
            "UPDATE action_approvals
            SET claimed_at=?2,outcome=json_object('status','unknown')
            WHERE +org_id = :org_id AND (id=?1 AND status='approved' AND claimed_at IS NULL)",
            &crate::util::now_utc(),
        ) {
            Ok(true) => reply(&db, id),
            Ok(false) => conflict(),
            Err(error) => internal(&error),
        },
        "claim" => conflict(),
        "outcome" => {
            let browser = request["kind"] == "browser_action";
            let Some(outcome) = node_outcome(&body, browser) else {
                return bad(if browser && body.get("status").is_some() {
                    "invalid browser submission status"
                } else {
                    "exit_code is required"
                });
            };
            match record_outcome(&db, id, &outcome.to_string()) {
                Ok(true) => reply(&db, id),
                Ok(false) => conflict(),
                Err(error) => internal(&error),
            }
        }
        _ => missing(),
    }
}

fn node_outcome(body: &Value, browser: bool) -> Option<Value> {
    let unknown = || {
        json!({"status":"submission_unknown",
        "message":"The site may have received the submission. Check the site before trying again."})
    };
    if browser && body.get("status").is_some() {
        let mut outcome = match body["status"].as_str()? {
            "submitted" => json!({"status":"submitted",
                "message":"The site received the submission. Check the site before trying again."}),
            "submission_unknown" => unknown(),
            "refused" => json!({"status":"refused",
                "message":"The browser refused the action before sending the submission."}),
            _ => return None,
        };
        if outcome["status"] == "refused"
            && let Some(reason) = body["reason"].as_str().filter(|text| {
                text.len() <= 160 && text.bytes().all(|byte| (32..=126).contains(&byte))
            })
        {
            outcome["reason"] = json!(reason);
        }
        return Some(outcome);
    }
    let exit = body["exit_code"]
        .as_i64()
        .filter(|code| (-255..=255).contains(code))?;
    Some(if browser && exit != 0 {
        unknown()
    } else {
        json!({"status":if exit == 0 { "succeeded" } else { "failed" },"exit_code":exit})
    })
}

pub(crate) fn record_outcome(
    db: &ScopedConnection,
    id: &str,
    outcome: &str,
) -> rusqlite::Result<bool> {
    let tx = db.unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE action_approvals SET outcome=?2 WHERE +org_id = :org_id AND (id=?1 AND claimed_at IS NOT NULL
        AND json_extract(outcome,'$.status')='unknown')",
        params![id, outcome],
    )? > 0;
    let recorded = if changed {
        browser::finish_job(&tx, id, outcome)?;
        touch(&tx, Record::ActionApproval, id)?;
        true
    } else {
        tx.prepare(
            "SELECT 1 FROM action_approvals WHERE +org_id = :org_id AND id=?1 AND claimed_at IS NOT NULL AND outcome=?2",
        )?
        .exists(params![id, outcome])?
    };
    tx.commit()?;
    Ok(recorded)
}

pub(crate) fn browser_jobs(state: &State, node: i64) -> Reply {
    match browser::jobs(&state.store.db(), node) {
        Ok(jobs) => json(200, &json!({"jobs":jobs})),
        Err(error) => internal(&error),
    }
}

fn attention_choices(item: &mut Value) {
    item["options"] = json!([{"id":"go","label":"Go"},{"id":"no_go","label":"No-go"}]);
    item["default"] = json!("no_go");
    item["links"] = json!({"approval":item["action_approval_id"]});
    if item["job_id"].is_string() {
        item["links"]["job"] = item["job_id"].clone();
    }
}

pub fn needs_you(db: &ScopedConnection, intent: &str) -> rusqlite::Result<Vec<Value>> {
    let (visible, member) = crate::policy::approval_read_scope(db);
    let mut stmt = db.prepare(&format!(
        "SELECT a.id,a.job_id,a.created_at,a.reviewed_at,a.review_reason,
        a.status,a.decided_at,a.decided_by,a.assigned_member_id,a.request FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) a JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id=a.job_id
        WHERE j.intent_id=?1 AND a.verdict='no_go' AND ?2 AND (?3 IS NULL OR (a.assigned_member_id = ?3 AND NOT ({OWNER_ONLY_APPROVAL}))) ORDER BY a.created_at,a.id",
    ))?;
    stmt.query_map(params![intent, visible, member], |row| {
        let status: String = row.get(5)?;
        let mut item = json!({"id":row.get::<_,String>(0)?,"action_approval_id":row.get::<_,String>(0)?,
            "job_id":row.get::<_,String>(1)?,"intent_id":intent,"raised_at":row.get::<_,Option<String>>(3)?,
            "assignee_member_id":row.get::<_,Option<String>>(8)?,"reason":row.get::<_,String>(4)?,"status":if status=="needs_owner" {"open"} else {"cleared"},
            "cleared_at":row.get::<_,Option<String>>(6)?,"cleared_by":row.get::<_,Option<String>>(7)?});
        attention_choices(&mut item);
        if visible && member.is_none() {
            browser::project(&mut item, &serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default());
        }
        crate::projections::names(db, &mut item)?;
        crate::projections::approval_actions(db, &mut item, status == "needs_owner");
        Ok(item)
    })?.collect()
}

// A job can originate directly in a conversation or in an intent started by its lead.
pub(crate) const CONVERSATION_LINK: &str = "(c.conversation_id=a.conversation_id OR c.intent_id=j.intent_id OR c.conversation_id=i.conversation_id
    OR c.conversation_id=i.lead_conversation_id OR c.conversation_id=i.origin_conversation_id OR EXISTS (SELECT 1 FROM turns t
    WHERE +org_id = :org_id AND t.job_id=j.job_id AND t.conversation_id=c.conversation_id))";

pub(crate) fn touch_conversations(db: &ScopedConnection, approval: &str) -> rusqlite::Result<()> {
    let mut stmt = db.prepare(&format!(
        "SELECT c.conversation_id FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) a
        LEFT JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id=a.job_id LEFT JOIN (SELECT * FROM intents WHERE +org_id = :org_id) i ON i.intent_id=j.intent_id
        JOIN (SELECT * FROM conversations WHERE +org_id = :org_id) c ON {CONVERSATION_LINK} WHERE a.id=?1"
    ))?;
    for id in stmt.query_map([approval], |r| r.get::<_, String>(0))? {
        touch(db, Record::Conversation, &id?)?;
    }
    Ok(())
}

pub fn attention(db: &ScopedConnection, conversation: &str) -> rusqlite::Result<Value> {
    let (visible, member) = crate::policy::approval_read_scope(db);
    db.query_row(&format!("SELECT a.id,a.job_id,COALESCE(a.reviewed_at,a.created_at),
        CASE WHEN json_extract(a.outcome,'$.waiting_jobs') IS NOT NULL
             THEN json_extract(a.outcome,'$.reason') ELSE a.review_reason END,
        a.assigned_member_id, COALESCE(json_extract(a.request,'$.kind'),'action'),
        json_extract(a.outcome,'$.waiting_jobs') IS NOT NULL,a.request,j.intent_id FROM (SELECT * FROM action_approvals WHERE +org_id = :org_id) a
        LEFT JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id=a.job_id LEFT JOIN (SELECT * FROM intents WHERE +org_id = :org_id) i ON i.intent_id=j.intent_id
        JOIN (SELECT * FROM conversations WHERE +org_id = :org_id) c ON {CONVERSATION_LINK} WHERE c.conversation_id=?1 AND (a.status='needs_owner' OR (json_extract(a.outcome,'$.waiting_jobs') IS NOT NULL
          AND COALESCE(json_extract(a.outcome,'$.acknowledged'),0)=0)) AND ?2 AND (?3 IS NULL OR (a.assigned_member_id = ?3 AND NOT ({OWNER_ONLY_APPROVAL})))
        ORDER BY a.created_at,a.id LIMIT 1"), params![conversation, visible, member], |r| {
        let mut item = json!({"action_approval_id":r.get::<_,String>(0)?, "job_id":r.get::<_,Option<String>>(1)?,
            "set_at":r.get::<_,String>(2)?, "question":r.get::<_,String>(3)?,
            "source_kind":"action_approval","assignee_member_id":r.get::<_,Option<String>>(4)?, "kind":r.get::<_,String>(5)?});
        if let Some(intent) = r.get::<_, Option<String>>(8)? {
            item["intent_id"] = json!(intent);
        }
        attention_choices(&mut item);
        if visible && member.is_none() {
            browser::project(&mut item, &serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default());
        }
        if r.get::<_, bool>(6)? {
            item["options"] = json!([{"id":"acknowledge","label":"Acknowledge rollback is waiting"}]);
            item["default"] = Value::Null;
        }
        Ok(item)
    }).optional().map(Option::unwrap_or_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_reviewer_touches_revision() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmp, store) = store()?;
        let db = store.db();
        assert!(select_model(&db, "action", "reviewer")?);
        let revision: i64 = db.query_row(
            "SELECT revision FROM action_approvals WHERE id='action'",
            params![],
            |row| row.get(0),
        )?;
        assert!(revision > 0);
        Ok(())
    }

    fn action() -> Value {
        json!({
            "argv": ["/usr/bin/curl", "-q", "--", "https://docs.rs/serde"],
            "needs": ["network"], "write_paths": [],
            "requesting_model": "writer", "reason": "Read the documentation"
        })
    }

    #[test]
    fn loopback_needs_name_unreserved_ports() {
        let mut request = action();
        request["needs"] = json!(["network:loopback:3000", "network:loopback:65535"]);
        assert_eq!(normalized(&request).as_ref(), Some(&request));
        for invalid in [
            json!(["network:loopback"]),
            json!(["network:loopback:0"]),
            json!(["network:loopback:+3000"]),
            json!(["network:loopback:03000"]),
            json!(["network:loopback:65536"]),
            json!(["network:loopback:8780"]),
            json!(["network:loopback:9200"]),
            json!(["network", "network:loopback:3000"]),
        ] {
            request["needs"] = invalid;
            assert!(normalized(&request).is_none(), "{request}");
        }
    }

    fn lane() -> Value {
        json!({
            "home":"/workspace/agent-home","workspace":"/workspace/agent-home/lane",
            "job_tmp":"/workspace/agent-home/.contextstack/runs/run/tmp",
            "scripts":[{"path":"cs/tasks/regressions/trace-e2e/check","contents":"#!/bin/sh\nexit 0\n"}]
        })
    }

    fn regression() -> Result<Value, String> {
        normalized(&json!({
            "argv": ["cs/tasks/regressions/trace-e2e/check"],
            "needs": ["network:loopback:3000", "write_paths"],
            "write_paths": ["/workspace/agent-home/.contextstack/runs/run/tmp"],
            "requesting_model": "writer", "reason": "Verify the lane changes"
        }))
        .ok_or_else(|| "invalid regression request".to_owned())
    }

    fn response(verdict: &str) -> Verdict {
        parse_verdict(&json!({"message":{"content":json!({
            "verdict":verdict,"reason":"reviewed the goal"
        }).to_string()}}))
        .unwrap_or_else(|| no_go("The reviewer returned an invalid verdict."))
    }

    fn store() -> Result<(tempfile::TempDir, crate::store::Store), String> {
        let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let store = crate::store::Store::open(&temp.path().join("director"))?;
        store
            .db()
            .fixture_batch(
                "
            INSERT INTO nodes (org_id, id,name,token_hash,created_at) VALUES(:org_id, 1,'test','hash','now');
            INSERT INTO jobs (org_id, job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at)
              VALUES(:org_id, 'job',1,'s','t',1,'sha','{}','accepted','now');
            INSERT INTO action_approvals (org_id, id,job_id,run_id,request,created_at)
            VALUES(:org_id, 'action','job','run','{}','now');
        ",
            )
            .map_err(|e| e.to_string())?;
        Ok((temp, store))
    }

    #[test]
    fn existing_v5_approval_can_publish_changes() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("director");
        let old = crate::store::legacy_fixture(&dir, 5)?;
        old.execute_batch(
            "INSERT INTO nodes (id,name,token_hash,created_at) VALUES (1,'test','hash','now');
             INSERT INTO jobs (job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at)
               VALUES ('job',1,'s','t',1,'sha','{}','accepted','now');
             INSERT INTO action_approvals (id,job_id,run_id,request,created_at)
               VALUES ('action','job','run','{}','now');",
        )?;
        drop(old);

        let upgraded = crate::store::Store::open(&dir)?;
        let db = upgraded.db();
        assert_eq!(
            record(&db, "action")?.ok_or("missing approval")?["status"],
            "pending"
        );
        finish_review(&db, "action", &no_go("needs a decision"))?;
        let event_record = crate::records::one(&db, Record::ActionApproval, "action")?
            .ok_or("missing event record")?;
        assert_eq!(
            event_record,
            json!({"id":"action","job_id":"job","revision":1})
        );
        Ok(())
    }

    #[tokio::test]
    async fn review_go_and_no_go_record_the_reason() -> Result<(), String> {
        for (word, status) in [("go", "approved"), ("no_go", "needs_owner")] {
            let (_temp, store) = store()?;
            let request = regression()?;
            let context = lane();
            assert_eq!(review_scope(&request, &context), "routine_widening");
            let verdict = review_request(&request, &context, async {
                match select_model(&store.db(), "action", "test-reviewer") {
                    Ok(true) => response(word),
                    _ => no_go("test could not record reviewer"),
                }
            })
            .await;
            let db = store.db();
            finish_review(&db, "action", &verdict).map_err(|e| e.to_string())?;
            let row = record(&db, "action")
                .map_err(|e| e.to_string())?
                .ok_or("missing approval")?;
            assert_eq!(row["status"], status);
            assert_eq!(row["verdict"], word);
            assert_eq!(row["review_reason"], "reviewed the goal");
            assert_eq!(row["reviewer_model"], "test-reviewer");
        }
        Ok(())
    }

    #[tokio::test]
    async fn hard_lines_skip_review() {
        for argv in [
            json!(["git", "push", "--force"]),
            json!(["/bin/rm", "--", ".contextstack/credentials"]),
            json!(["/bin/rm", "--", "auth-token"]),
            json!(["/bin/rm", "--", "/outside"]),
            json!(["/bin/rm", "--", "../outside"]),
        ] {
            let mut request = action();
            request["argv"] = argv;
            let decision = review_request(&request, &lane(), async {
                panic!("hard line called the reviewer");
            })
            .await;
            assert_eq!(decision.word, "no_go");
        }
    }

    fn hard_line(argv: Value, context: &Value) -> Option<String> {
        let mut request = action();
        request["argv"] = argv;
        hard_line_reason(&request, context)
    }

    const PUSH: &str = "Git push needs a human decision (an owner or the assigned admin).";
    const CREDENTIAL: &str =
        "The action references a credential, key, token, or protected configuration path.";

    #[test]
    fn hard_line_verdicts_distinguish_commands_and_listing() {
        for (argv, owner) in [
            (json!(["grep", "git", "."]), false),
            (json!(["rg", "push", "src/"]), false),
            (json!(["echo", "git push later"]), false),
            (json!(["echo", "send-pack", "gh"]), false),
            (json!(["sh", "-c", "echo git push later"]), false),
            (json!(["sh", "-c", "echo ok > git"]), false),
            (json!(["env", "grep", "git", "."]), false),
            (
                json!(["sh", "-c", "echo 'git push later' # git push"]),
                false,
            ),
            (json!(["ls", "~"]), false),
            (json!(["ls", "-la", "~/.config"]), false),
            (
                json!(["find", "~", "-maxdepth", "1", "-name", "*.md"]),
                false,
            ),
            (json!(["git", "config", "user.name", "x"]), false),
            (
                json!(["git", "config", "--get", "remote.origin.url"]),
                false,
            ),
            (json!(["git", "status"]), false),
            (json!(["git", "log"]), false),
            (json!(["git", "diff"]), false),
            (json!(["git", "push"]), true),
            (json!(["env", "git", "push"]), true),
            (json!(["find", ".", "-exec", "git", "push", ";"]), true),
            (json!(["git", "publish"]), true),
            (json!(["git", "-c", "alias.x=push", "x"]), true),
            (json!(["send-pack", "origin"]), true),
            (json!(["git", "http-push", "origin"]), true),
            (json!(["gh", "pr", "merge", "12"]), true),
            (json!(["cat", "~/.git-credentials"]), true),
            (json!(["cp", "~/.config/gh/hosts.yml", "."]), true),
            (json!(["tar", "-c", "~"]), true),
            (json!(["cp", "-r", "~/.config", "."]), true),
            (json!(["rsync", "-a", "~", "."]), true),
            (json!(["open", "~/.config/gh/hosts.yml"]), true),
            (json!(["grep", "-r", "host", "~"]), true),
            (json!(["ls", "-R", "~"]), true),
            (json!(["find", "~", "-name", "*.md"]), true),
            (json!(["git", "config", "credential.helper", "store"]), true),
        ] {
            assert_eq!(hard_line(argv.clone(), &lane()).is_some(), owner, "{argv}");
        }
    }

    /// C2: forms that wrote to a remote without the old `push` word check noticing.
    #[test]
    fn remote_writes_are_hard_lines() {
        for argv in [
            json!(["git", "-c", "alias.x=push", "x"]),
            json!(["git", "-c", "Alias.Up=push --force", "up", "origin"]),
            json!(["git", "-c", "alias.x=!git pu''sh", "x"]),
            json!(["git", "--config-env=alias.x=PUSH_ALIAS", "x"]),
            json!(["git", "send-pack", "https://example.com/r.git", "main"]),
            json!([
                "/usr/bin/git",
                "http-push",
                "https://example.com/r.git",
                "main"
            ]),
            json!(["git", "remote-https", "origin", "https://example.com/r.git"]),
            json!(["git-send-pack", "https://example.com/r.git", "main"]),
            json!(["/usr/libexec/git-core/git-remote-https", "origin"]),
            json!(["sh", "-c", "cd lane && git send-pack origin main"]),
            json!(["sh", "-c", "git -c alias.x='!git pu\"\"sh' x"]),
            json!([
                "python3",
                "-c",
                "import subprocess; subprocess.run(['git','push'])"
            ]),
            json!([
                "git",
                "-c",
                "core.sshCommand=ssh -o ProxyCommand=evil",
                "fetch"
            ]),
            json!(["git", "submodule", "foreach", "git send-pack origin main"]),
            json!(["git", "rebase", "-x", "make", "main"]),
            json!(["git", "fetch", "--upload-pack=git-receive-pack", "origin"]),
            json!(["gh", "repo", "sync"]),
            json!(["gh", "pr", "merge", "12", "--squash"]),
            json!(["gh", "release", "create", "v1.0.0"]),
            json!(["gh", "api", "-X", "POST", "repos/o/r/issues"]),
            json!(["gh", "api", "--method=PUT", "repos/o/r/pulls/1/merge"]),
            json!(["gh", "api", "-XPATCH", "repos/o/r"]),
            json!([
                "gh",
                "api",
                "--method",
                "delete",
                "repos/o/r/git/refs/heads/x"
            ]),
            json!(["gh", "api", "repos/o/r/issues", "-f", "title=x"]),
            json!(["gh", "api", "graphql", "-F", "query=@mutation.graphql"]),
            json!(["sh", "-c", "gh pr merge 12"]),
        ] {
            let reason = hard_line(argv.clone(), &lane()).unwrap_or_default();
            assert!(
                reason.starts_with(PUSH.trim_end_matches('.')),
                "{argv}: {reason}"
            );
        }
    }

    #[test]
    fn read_only_git_and_gh_still_reach_the_reviewer() {
        for argv in [
            json!(["git", "fetch", "origin"]),
            json!([
                "/usr/bin/git",
                "-C",
                "/workspace/agent-home/lane",
                "log",
                "--oneline"
            ]),
            json!(["git", "-c", "alias.st=status --short", "st"]),
            json!(["git", "-c", "alias.log=push", "log"]),
            json!(["git", "clone", "https://example.com/r.git"]),
            json!(["git", "ls-remote", "origin"]),
            json!(["sh", "-c", "git fetch origin && git log -1"]),
            json!(["gh", "pr", "view", "12"]),
            json!(["gh", "pr", "checks", "12", "--repo", "o/r"]),
            json!([
                "gh",
                "api",
                "repos/o/r/pulls",
                "--paginate",
                "-q",
                ".[].title"
            ]),
            json!(["gh", "api", "-X", "GET", "repos/o/r"]),
            json!(["gh", "run", "view", "42", "--log"]),
            json!(["/usr/bin/curl", "-q", "--", "https://docs.rs/serde"]),
        ] {
            assert_eq!(hard_line(argv.clone(), &lane()), None, "{argv}");
        }
    }

    /// C3: credential stores the component-name check did not know about, and `~`, `$HOME`,
    /// `..` and shell-string forms of them.
    #[test]
    fn credential_stores_are_hard_lines() {
        let home = "/workspace/agent-home";
        for argv in [
            json!(["/bin/cat", format!("{home}/.config/gh/hosts.yml")]),
            json!(["/bin/cat", "~/.docker/config.json"]),
            json!(["/bin/ls", "~/.kube"]),
            json!(["/bin/cat", "~agent-home/.kube/config"]),
            json!(["sh", "-c", "cat $HOME/.kube/config"]),
            json!([
                "sh",
                "-c",
                "cat \"${HOME}\"/.config/gcloud/configurations/config_default"
            ]),
            json!([
                "/bin/cat",
                format!("{home}/Library/Keychains/login.keychain-db")
            ]),
            json!(["/bin/cat", format!("{home}/.codex/auth.json")]),
            json!(["/bin/cat", format!("{home}/.claude/.credentials.json")]),
            json!(["/bin/cat", format!("{home}/.gemini/oauth_creds.json")]),
            json!([
                "/bin/cat",
                format!("{home}/.local/share/opencode/opencode.db")
            ]),
            json!([
                "/bin/cat",
                format!("{home}/.contextstack/credentials/director")
            ]),
            json!(["/bin/cat", format!("{home}/lane/../.config/gh/hosts.yml")]),
            json!([
                "sh",
                "-c",
                format!("cat {home}/lane/../.docker/config.json")
            ]),
            json!(["/usr/bin/tar", "czf", "out.tgz", "~/.config"]),
            json!(["/usr/bin/tar", "czf", "out.tgz", "--directory=~", "."]),
            json!(["sh", "-c", "d=gh; cat ~/.config/$d/hosts.yml"]),
            json!(["sh", "-c", "cat ~/.config/g[h]/hosts.yml"]),
            json!(["sh", "-c", "cat ~/.c*/gh/hosts.yml"]),
            json!(["sh", "-c", "cat /workspace/$USER/.config/gh/hosts.yml"]),
            json!(["sh", "-c", "cd ~ && cat .docker/config.json"]),
        ] {
            assert_eq!(
                hard_line(argv.clone(), &lane()).as_deref(),
                Some(CREDENTIAL),
                "{argv}"
            );
        }
        // A workspace symlink the node resolved onto a credential store.
        let mut context = lane();
        context["resolved_operands"] = json!([format!("{home}/.kube/config")]);
        assert_eq!(
            hard_line(json!(["/bin/cat", "kube/config"]), &context).as_deref(),
            Some(CREDENTIAL)
        );
        // A lane whose workspace lies inside a store directory may still use its own files.
        let mut context = lane();
        context["workspace"] = json!(format!("{home}/.config/gcloud/lane"));
        assert_eq!(hard_line(json!(["/bin/cat", "README.md"]), &context), None);
        assert_eq!(
            hard_line(
                json!(["/bin/cat", "~/.config/gcloud/credentials.db"]),
                &context
            )
            .as_deref(),
            Some(CREDENTIAL)
        );
        // Node-resolved operands keep the listing verdict only for containing directories.
        let mut context = lane();
        context["resolved_operands"] = json!([format!("{home}/.config")]);
        assert_eq!(hard_line(json!(["ls", "~/.config"]), &context), None);
        context["resolved_operands"] = json!([format!("{home}/.config/gh")]);
        assert_eq!(
            hard_line(json!(["ls", "notes"]), &context).as_deref(),
            Some(CREDENTIAL)
        );
        // A write path that contains a credential store.
        let mut request = action();
        request["argv"] = json!(["ls", "~/.config"]);
        request["write_paths"] = json!([format!("{home}/.config")]);
        assert_eq!(
            hard_line_reason(&request, &lane()).as_deref(),
            Some(CREDENTIAL)
        );
    }

    #[test]
    fn symlinked_credential_store_is_a_hard_line() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().canonicalize()?.join("home");
        let workspace = home.join("lane");
        std::fs::create_dir_all(home.join(".config/gh"))?;
        std::fs::create_dir_all(&workspace)?;
        std::os::unix::fs::symlink(home.join(".config/gh"), workspace.join("notes"))?;
        let context = json!({"home": home, "workspace": workspace});
        for argv in [
            json!(["/bin/cat", "notes/hosts.yml"]),
            json!(["/bin/cat", workspace.join("notes/hosts.yml")]),
            json!(["sh", "-c", "cat notes/hosts.yml"]),
        ] {
            assert_eq!(
                hard_line(argv.clone(), &context).as_deref(),
                Some(CREDENTIAL),
                "{argv}"
            );
        }
        assert_eq!(hard_line(json!(["/bin/cat", "README.md"]), &context), None);
        Ok(())
    }

    #[test]
    fn reviewer_uses_independent_gateway_model() {
        let models = json!({"models":[
            {"name":"writer","provider":"writer-source","available":true,"cost_class":"free_local","tier":9},
            {"name":"same-provider","provider":"writer-source","available":true,"cost_class":"free_local","tier":8},
            {"name":"paid","provider":"other","available":true,"cost_class":"api","tier":9},
            {"name":"local-low","provider":"other","available":true,"cost_class":"free_local","tier":1},
            {"name":"local-high","provider":"other","available":true,"cost_class":"free_local","tier":2},
            {"name":"local-tie","provider":"other","available":true,"cost_class":"free_local","tier":2},
            {"name":"cli","provider":"other","executor":"cli","available":true},
            {"name":"offline","provider":"other","available":false},
            {"name":"candidate","provider":"other","available":true,"candidate":true}
        ]});
        assert_eq!(
            select_reviewer(&models, "writer"),
            [
                "local-low",
                "local-high",
                "local-tie",
                "paid",
                "same-provider"
            ],
        );
    }

    fn content(raw: &str) -> Value {
        json!({"message": {"content": raw}})
    }

    #[test]
    fn parse_verdict_accepts_plain_fenced_and_prose() -> Result<(), Box<dyn std::error::Error>> {
        let plain =
            parse_verdict(&content(r#"{"verdict":"go","reason":"looks safe"}"#)).ok_or("plain")?;
        assert_eq!((plain.word, plain.reason.as_str()), ("go", "looks safe"));

        let fenced = parse_verdict(&content(
            "```json\n{\n  \"verdict\": \"no_go\",\n  \"reason\": \"touches keys\"\n}\n```",
        ))
        .ok_or("fenced")?;
        assert_eq!(
            (fenced.word, fenced.reason.as_str()),
            ("no_go", "touches keys")
        );

        let prose = parse_verdict(&content(
            "Here is my review:\n{\"verdict\":\"go\",\"reason\":\"bounded write\"}\nThanks.",
        ))
        .ok_or("prose")?;
        assert_eq!((prose.word, prose.reason.as_str()), ("go", "bounded write"));
        Ok(())
    }

    #[test]
    fn parse_verdict_rejects_conflicting_objects() {
        assert!(
            parse_verdict(&content(
                r#"{"verdict":"go","reason":"ok"} {"verdict":"no_go","reason":"not ok"}"#,
            ))
            .is_none()
        );
    }

    #[test]
    fn parse_verdict_truncates_long_reason() -> Result<(), Box<dyn std::error::Error>> {
        let long = "x".repeat(850);
        let verdict = parse_verdict(&content(&format!(
            r#"{{"verdict":"go","reason":"{long}"}}"#
        )))
        .ok_or("long reason")?;
        assert_eq!(verdict.word, "go");
        assert_eq!(verdict.reason.chars().count(), 800);
        assert!(verdict.reason.chars().all(|c| c == 'x'));
        Ok(())
    }

    #[test]
    fn pending_approval_expires_when_job_ends() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, store) = store()?;
        let db = store.db();
        assert_eq!(expire_for_job(&db, "job")?, 1);
        let row = record(&db, "action")?.ok_or("missing")?;
        assert_eq!(row["status"], "expired");
        assert_eq!(row["review_reason"], "job ended");
        assert_eq!(
            row["outcome"],
            json!({"status": "expired", "reason": "job ended"})
        );
        assert_eq!(expire_for_job(&db, "job")?, 0);
        Ok(())
    }

    #[test]
    fn startup_sweep_expires_approvals_for_ended_jobs() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, store) = store()?;
        let db = store.db();
        db.fixture_batch(
            "
            UPDATE jobs SET state='done' WHERE job_id='job';
            INSERT INTO jobs (org_id, job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at)
              VALUES(:org_id, 'running',1,'s','t',1,'sha','{}','accepted','now');
            INSERT INTO action_approvals (org_id, id,job_id,run_id,request,status,created_at,verdict,review_reason)
              VALUES(:org_id, 'owner','job','run2','{}','needs_owner','now','no_go','review said no');
            INSERT INTO action_approvals (org_id, id,job_id,run_id,request,created_at)
            VALUES(:org_id, 'live','running','run3','{}','now');
        ",
        )?;
        assert_eq!(expire_ended_jobs(&db)?, 2);
        assert_eq!(
            record(&db, "action")?.ok_or("pending")?["status"],
            "expired"
        );
        let owner = record(&db, "owner")?.ok_or("owner")?;
        assert_eq!(owner["status"], "expired");
        assert_eq!(owner["review_reason"], "review said no");
        assert_eq!(
            owner["outcome"],
            json!({"status": "expired", "reason": "job ended"})
        );
        assert_eq!(record(&db, "live")?.ok_or("live")?["status"], "pending");
        Ok(())
    }
}
