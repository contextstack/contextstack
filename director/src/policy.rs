//! Effective permissions for authenticated Director clients.
use crate::access::{Client, Kind, Role};
use crate::server::{Call, Reply, State, internal, refusal};
use crate::store::{Record, ScopedConnection};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    ReadWork,
    ReadMemberDirectory,
    RenameSelf,
    StartWork,
    ControlWork,
    DecideRun,
    ApproveAction,
    ReadSourceBudgets,
    ManageSourceBudgets,
    ManageDirectorBudgets,
    ManageOwnDevices,
    ManageMembers,
    ManageMemberDevices,
    EditProjects,
    EditBacklog,
    EditDecisionLog,
    ManageSchedules,
    ManageOrgSettings,
    SponsorLead,
    Land,
}
use Capability::{
    ApproveAction, ControlWork, DecideRun, EditBacklog, EditDecisionLog, EditProjects, Land,
    ManageDirectorBudgets, ManageMemberDevices, ManageMembers, ManageOrgSettings, ManageOwnDevices,
    ManageSchedules, ManageSourceBudgets, ReadMemberDirectory, ReadSourceBudgets, ReadWork,
    RenameSelf, SponsorLead, StartWork,
};

const CAPABILITIES: &[(Capability, &str, Role, bool)] = &[
    (ReadWork, "read_work", Role::Viewer, true),
    (
        ReadMemberDirectory,
        "read_member_directory",
        Role::Viewer,
        false,
    ),
    (RenameSelf, "rename_self", Role::Viewer, false),
    (StartWork, "start_work", Role::Member, true),
    (ControlWork, "control_work", Role::Member, true),
    (DecideRun, "decide_run", Role::Member, true),
    (ApproveAction, "approve_action", Role::Admin, false),
    (ReadSourceBudgets, "read_source_budgets", Role::Admin, false),
    (
        ManageSourceBudgets,
        "manage_source_budgets",
        Role::Owner,
        false,
    ),
    (
        ManageDirectorBudgets,
        "manage_director_budgets",
        Role::Admin,
        false,
    ),
    (ManageOwnDevices, "manage_own_devices", Role::Viewer, false),
    (ManageMembers, "manage_members", Role::Admin, false),
    (
        ManageMemberDevices,
        "manage_member_devices",
        Role::Admin,
        false,
    ),
    (EditProjects, "edit_projects", Role::Admin, false),
    (EditBacklog, "edit_backlog", Role::Member, true),
    (EditDecisionLog, "edit_decision_log", Role::Member, true),
    (ManageSchedules, "manage_schedules", Role::Admin, false),
    (ManageOrgSettings, "manage_org_settings", Role::Owner, false),
    (SponsorLead, "sponsor_lead", Role::Member, false),
    (Land, "land", Role::Owner, false),
];

#[derive(Clone, Copy)]
pub enum Target<'a> {
    Org(&'a str),
    Member(&'a str, &'a str),
}

pub fn allows(client: &Client, cap: Capability, target: Target<'_>) -> bool {
    let (org, member) = match target {
        Target::Org(org) => (org, None),
        Target::Member(org, member) => (org, Some(member)),
    };
    let scope = &client.member.scope;
    let Some(role) = scope.role else { return false };
    if !client.member.active || org != scope.org_id {
        return false;
    }
    let Some((_, _, minimum, delegated)) = CAPABILITIES.iter().find(|(c, _, _, _)| *c == cap)
    else {
        return false;
    };
    if role < *minimum || (client.kind == Kind::Lead && !delegated) {
        return false;
    }
    if cap == Land {
        return client.kind == Kind::Local;
    }
    if let Some(member) = member {
        match cap {
            ManageOwnDevices => return member == client.member_id() || role == Role::Owner,
            ApproveAction | StartWork | ControlWork | DecideRun | SponsorLead => {
                let unrestricted = if cap == ApproveAction {
                    Role::Owner
                } else {
                    Role::Admin
                };
                return role >= unrestricted || member == client.member_id();
            }
            _ => {}
        }
    }
    true
}

pub fn projection(client: &Client) -> Vec<&'static str> {
    CAPABILITIES
        .iter()
        .filter_map(|(cap, name, _, _)| {
            allows(client, *cap, Target::Org(&client.member.scope.org_id)).then_some(*name)
        })
        .collect()
}

pub fn denied() -> Reply {
    refusal(
        403,
        "forbidden",
        "this credential cannot perform this operation on the target",
        Value::Null,
    )
}

#[derive(Clone, Copy)]
struct Operation<'a>(Capability, Option<(Record, &'a str)>);
use Record::{Conversation, Intent, Job, Run};

#[allow(clippy::too_many_lines)]
fn route<'a>(method: &str, path: &[&'a str]) -> Option<Operation<'a>> {
    let op = match (method, path) {
        (
            "GET",
            [
                "me" | "lead-node" | "chief-of-staff" | "nodes" | "agents" | "models" | "repos"
                | "events" | "intents" | "jobs" | "runs" | "conversations" | "projects" | "usage"
                | "schedules",
            ]
            | [
                "nodes" | "agents" | "jobs" | "runs" | "intents" | "conversations" | "turns"
                | "summaries" | "decisions" | "cancels" | "objects" | "projects"
                | "schedule-changes",
                _,
            ]
            | ["intents", _, "work-trace"]
            | ["conversations", _, "thread"]
            | ["schedules", _, _]
            | ["runs", _, "decision" | "events"]
            | ["projects", _, "heartbeat" | "backlog.md"]
            | ["projects", _, "backlog" | "decision_log", ..]
            | ["jobs" | "runs", _, "files", _, ..],
        ) => Operation(ReadWork, None),
        ("POST", ["intents" | "conversations"]) | ("PUT", ["objects", _]) => {
            Operation(StartWork, None)
        }
        // The local-owner note handler checks idempotency before looking up the conversation.
        ("POST", ["conversations", _, "notes"]) => Operation(StartWork, None),
        ("GET" | "POST", ["conversations", id, "peer-messages"]) => {
            Operation(StartWork, Some((Conversation, id)))
        }
        ("POST", ["intents", id, "jobs"]) => Operation(StartWork, Some((Intent, id))),
        (
            "POST",
            [
                "conversations",
                id,
                "turns" | "wake-turns" | "land" | "install-release" | "rollback",
            ]
            | ["conversations", id, "summaries", _, "redo"],
        ) => Operation(StartWork, Some((Conversation, id))),
        ("GET", ["conversations", id, "smoke"]) => Operation(ReadWork, Some((Conversation, id))),
        ("POST", ["jobs", id, "cancel"]) => Operation(ControlWork, Some((Job, id))),
        ("PUT", ["intents", id, "owner-messages", _, "mark"]) => {
            Operation(ControlWork, Some((Intent, id)))
        }
        ("PATCH" | "DELETE", ["conversations", id])
        | (
            "POST",
            [
                "conversations",
                id,
                "archive" | "unarchive" | "stop" | "resume",
            ],
        )
        | ("PUT" | "DELETE", ["conversations", id, "attention"]) => {
            Operation(ControlWork, Some((Conversation, id)))
        }
        ("POST", ["runs", id, "decision"]) => Operation(DecideRun, Some((Run, id))),
        ("GET", ["action-approvals"] | ["action-approvals" | "browser-action-screenshots", _])
        | ("POST", ["action-approvals", _, "decide" | "answer" | "clear"]) => {
            Operation(ApproveAction, None)
        }
        ("GET", ["source-budgets"]) => Operation(ReadSourceBudgets, None),
        ("PUT", ["source-budgets"]) | ("PUT" | "DELETE", ["source-budgets", _]) => {
            Operation(ManageSourceBudgets, None)
        }
        ("PUT" | "DELETE", ["projects" | "conversations", _, "budget"]) => {
            Operation(ManageDirectorBudgets, None)
        }
        ("POST", ["projects"]) => Operation(EditProjects, None),
        (
            "POST",
            ["projects", _, "backlog"]
            | ["projects", _, "backlog", "reorder"]
            | ["projects", _, "backlog", _, "update" | "answer"],
        ) => Operation(EditBacklog, None),
        (
            "POST",
            ["projects", _, "decision_log"]
            | ["projects", _, "decision_log", "import"]
            | [
                "projects",
                _,
                "decision_log",
                _,
                "revise" | "supersede" | "retire",
            ],
        ) => Operation(EditDecisionLog, None),
        ("POST", ["schedules"] | ["schedules", _, _, "changes"])
        | ("DELETE" | "PATCH", ["schedules", _, _]) => Operation(ManageSchedules, None),
        ("PUT", ["projects", project, "heartbeat"]) => {
            Operation(ManageSchedules, Some((Record::Project, project)))
        }
        ("GET" | "PUT", ["peer-message-allowlist" | "advertised-url"])
        | ("PUT", ["lead-node" | "chief-of-staff"]) => Operation(ManageOrgSettings, None),
        ("POST", ["credentials", "lead"]) => Operation(SponsorLead, None),
        ("GET", ["members", "directory"]) => Operation(ReadMemberDirectory, None),
        ("GET", ["members" | "invites"])
        | ("POST", ["invites"])
        | ("PATCH", ["members", _])
        | ("DELETE", ["invites", _]) => Operation(ManageMembers, None),
        ("GET", ["credentials"]) | ("POST", ["pairings"]) | ("DELETE", ["credentials", _]) => {
            Operation(ManageOwnDevices, None)
        }
        _ => return None,
    };
    Some(op)
}

pub fn responsible(
    db: &ScopedConnection,
    kind: Record,
    id: &str,
) -> rusqlite::Result<Option<String>> {
    let sql = match kind {
        Intent => {
            "SELECT COALESCE(responsible_member_id, :member_id) FROM intents WHERE +org_id = :org_id AND intent_id = ?1"
        }
        Conversation => {
            "SELECT COALESCE(responsible_member_id, :member_id) FROM conversations WHERE +org_id = :org_id AND conversation_id = ?1"
        }
        Job => {
            "SELECT COALESCE(responsible_member_id, :member_id) FROM jobs WHERE +org_id = :org_id AND job_id = ?1"
        }
        Run => {
            return match crate::records::job_for_run(db, id)? {
                Some(job) => responsible(db, Job, &job),
                None => Ok(None),
            };
        }
        _ => return Ok(None),
    };
    db.query_row(sql, params![id], |row| row.get(0)).optional()
}

fn scoped(
    db: &ScopedConnection,
    client: &Client,
    operation: Operation<'_>,
) -> rusqlite::Result<bool> {
    let Operation(cap, resource) = operation;
    let org = &db.scope().org_id;
    if !allows(client, cap, Target::Org(org)) {
        // Existing project heartbeats are scoped self-wakes for hosted leads.
        return Ok(cap == ManageSchedules
            && client.kind == Kind::Lead
            && resource.is_some_and(|(kind, _)| kind == Record::Project)
            && allows(client, StartWork, Target::Org(org)));
    }
    if client.member.scope.role != Some(Role::Member) {
        return Ok(true);
    }
    let Some((kind, id)) = resource else {
        return Ok(true);
    };
    let member = if kind == Conversation
        && client.kind == Kind::Lead
        && client.conversation_id.as_deref() == Some(id)
    {
        crate::sponsorship::sponsor(db, id)?
    } else {
        responsible(db, kind, id)?
    };
    Ok(member.is_some_and(|member| allows(client, cap, Target::Member(org, &member))))
}

#[allow(clippy::result_large_err)]
fn require_responsible(db: &ScopedConnection, kind: Record, id: &str) -> Result<String, Reply> {
    responsible(db, kind, id)
        .map_err(|e| internal(&e))?
        .ok_or_else(crate::client_api::not_found)
}

fn bundle_object_visible(
    db: &ScopedConnection,
    client: &Client,
    sha: &str,
) -> rusqlite::Result<bool> {
    db.prepare_cached(
        "SELECT 1 FROM (SELECT credential_id FROM lead_objects WHERE +org_id = :org_id AND sha256 = ?1) o
         JOIN (SELECT id FROM credentials WHERE +org_id = :org_id AND member_id = ?2) c
           ON c.id = o.credential_id
         UNION ALL SELECT 1 FROM job_files WHERE +org_id = :org_id AND sha256 = ?1
         UNION ALL SELECT 1 FROM run_files WHERE +org_id = :org_id AND sha256 = ?1
         UNION ALL SELECT 1 FROM conversation_files WHERE +org_id = :org_id AND sha256 = ?1 LIMIT 1",
    )?.exists(params![sha, client.member_id()])
}

#[allow(clippy::result_large_err)]
fn check_links(
    state: &State,
    db: &ScopedConnection,
    client: &Client,
    parts: &[&str],
    body: &Value,
) -> Result<(), Reply> {
    if matches!(
        parts,
        ["intents", ..] | ["conversations"] | ["credentials", "lead"]
    ) {
        if let Some(job) = body["from_job"].as_str()
            && !(matches!(parts, ["conversations"]) && body.get("model").is_some())
        {
            if client.kind == Kind::Lead
                && !crate::lead::visible(db, client, Job, job).map_err(|e| internal(&e))?
            {
                return Err(crate::lead::denied());
            }
            require_responsible(db, Job, job)?;
        }
        if matches!(parts, ["conversations"] | ["credentials", "lead"])
            && let Some(intent) = body["intent_id"].as_str()
        {
            let member = require_responsible(db, Intent, intent)?;
            if !allows(
                client,
                ControlWork,
                Target::Member(&db.scope().org_id, &member),
            ) {
                return Err(denied());
            }
        }
        if body["origin"]["kind"] == "conversation_turn"
            && let Some(id) = body["origin"]["conversation_id"].as_str()
        {
            let member = require_responsible(db, Conversation, id)?;
            if !allows(
                client,
                ControlWork,
                Target::Member(&db.scope().org_id, &member),
            ) {
                return Err(denied());
            }
            if !db.prepare_cached(
                "SELECT 1 FROM turns WHERE +org_id = :org_id AND conversation_id = ?1 AND turn_id = ?2",
            ).and_then(|mut stmt| stmt.exists(params![id, body["origin"]["turn_id"].as_str()]))
                .map_err(|e| internal(&e))?
            {
                return Err(crate::client_api::not_found());
            }
        }
        if client.kind != Kind::Lead
            && let Some(files) = body["files"].as_array()
        {
            for sha in files.iter().filter_map(|file| file["sha256"].as_str()) {
                if state.store.has_object(sha)
                    && !bundle_object_visible(db, client, sha).map_err(|e| internal(&e))?
                {
                    return Err(crate::client_api::not_found());
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn check_backlog_update(
    client: &Client,
    old: &Value,
    input: &Map<String, Value>,
) -> Result<(), crate::work::No> {
    if can_resolve_owner_blocker(client, old["assignee_member_id"].as_str()) {
        return Ok(());
    }
    let mut owner_blockers = old["blocked_on"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|blocker| blocker["kind"] == "owner")
        .peekable();
    let hides_blocker = owner_blockers.peek().is_some()
        && !matches!(old["status"].as_str(), Some("done" | "dropped"))
        && matches!(
            input.get("status").and_then(Value::as_str),
            Some("done" | "dropped")
        );
    let removes_blocker = input.get("blocked_on").is_some_and(|blockers| {
        owner_blockers.any(|blocker| {
            !blockers
                .as_array()
                .is_some_and(|list| list.contains(blocker))
        })
    });
    let changes_question = input
        .get("needs_you")
        .is_some_and(|question| !old["needs_you"].is_null() && question != &old["needs_you"]);
    let changes_decision = changes_question || input.contains_key("needs_you_answer");
    if hides_blocker
        || removes_blocker
        || (changes_decision
            && (client.kind == Kind::Lead || old["assignee_member_id"].is_string()))
    {
        return Err(crate::work::No(
            403,
            "forbidden",
            "only the assigned member or an owner can resolve an owner blocker".into(),
            Value::Null,
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
pub fn guard(state: &State, call: &Call, client: &Client, parts: &[&str]) -> Result<(), Reply> {
    guard_on(state, &state.store.db(), call, client, parts)
}

#[allow(clippy::result_large_err)]
fn guard_on(
    state: &State,
    db: &ScopedConnection,
    call: &Call,
    client: &Client,
    parts: &[&str],
) -> Result<(), Reply> {
    let body = if call.method == "GET" || matches!(parts, ["objects", _]) {
        Value::Null
    } else {
        serde_json::from_slice(&call.body).unwrap_or_default()
    };
    authorize(db, call, client, parts, &body)?;
    if call.method != "GET" {
        check_links(state, db, client, parts, &body)?;
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn authorize(
    db: &ScopedConnection,
    call: &Call,
    client: &Client,
    parts: &[&str],
    body: &Value,
) -> Result<(), Reply> {
    if let Some(no) = crate::lead::guard(db, call, client, parts) {
        return Err(no);
    }
    let Some(mut operation) = route(&call.method, parts) else {
        return Err(crate::client_api::not_found());
    };
    let approval = operation.0 == ApproveAction;
    if approval && call.method == "GET" && client.kind == Kind::Lead {
        operation.0 = ReadWork;
    }
    let org = &db.scope().org_id;
    let is_owner = client.member.scope.role == Some(Role::Owner);
    if matches!(parts, ["members", id] if *id == client.member_id())
        && operation.0 == ManageMembers
        && crate::members::rename_only(body)
    {
        operation.0 = RenameSelf;
    }
    guard_sensitive_approvals(db, client, &call.method, parts, is_owner)?;
    let adoption = matches!(
        (call.method.as_str(), parts),
        ("PATCH", ["conversations", _])
    ) && body.get("sponsor_member_id").is_some();
    if adoption && !allows(client, ManageMembers, Target::Org(org)) {
        return Err(denied());
    }
    if operation.0 == SponsorLead {
        operation.1 = body["conversation_id"]
            .as_str()
            .map(|id| (Conversation, id));
    }
    if let Some((kind @ (Conversation | Intent | Job | Run), id)) = operation.1 {
        require_responsible(db, kind, id)?;
    }
    if operation.0 == SponsorLead
        && let Some((_, id)) = operation.1
    {
        let sponsor = crate::sponsorship::sponsor(db, id)
            .map_err(|e| internal(&e))?
            .ok_or_else(crate::client_api::not_found)?;
        if !allows(client, SponsorLead, Target::Member(org, &sponsor))
            || !eligible_sponsor(db, &sponsor).map_err(|e| internal(&e))?
        {
            return Err(denied());
        }
    } else if !scoped(db, client, operation).map_err(|e| internal(&e))? {
        return Err(denied());
    }
    if approval
        && let ["action-approvals", id, ..] = parts
        && !approval_visible(db, client, id).map_err(|e| internal(&e))?
    {
        return Err(crate::client_api::not_found());
    }
    if call.method == "POST"
        && parts == ["pairings"]
        && body["member_id"]
            .as_str()
            .is_some_and(|id| id != client.member_id())
        && !allows(client, ManageMemberDevices, Target::Org(org))
    {
        return Err(denied());
    }
    guard_payload(db, call, client, parts, body, operation.0, adoption)
}

pub(crate) fn can_resolve_owner_blocker(client: &Client, assignee: Option<&str>) -> bool {
    client.kind != Kind::Lead
        && (client.member.scope.role == Some(Role::Owner) || assignee == Some(client.member_id()))
}

#[allow(clippy::result_large_err)]
fn guard_payload(
    db: &ScopedConnection,
    call: &Call,
    client: &Client,
    parts: &[&str],
    body: &Value,
    capability: Capability,
    adoption: bool,
) -> Result<(), Reply> {
    let org = &db.scope().org_id;
    if let ("POST", ["projects", project, "backlog", item, "answer"]) =
        (call.method.as_str(), parts)
    {
        let assignee =
            crate::backlog::owner_blocker_assignee(db, project, item).map_err(|e| internal(&e))?;
        if client.kind == Kind::Lead
            || assignee.is_some_and(|id| !can_resolve_owner_blocker(client, Some(&id)))
        {
            return Err(denied());
        }
    }
    if client.member.scope.role != Some(Role::Owner) {
        let nested_budget = matches!(capability, EditProjects | ManageSchedules)
            || matches!(parts, ["conversations", ..]);
        if nested_budget
            && (!body["budget"].is_null() || !body["set"]["budget"].is_null())
            && !allows(client, ManageDirectorBudgets, Target::Org(org))
        {
            return Err(denied());
        }
        if call.method == "PATCH"
            && !adoption
            && let ["conversations", id] = parts
            && crate::chief_of_staff::read(db).map_err(|e| internal(&e))?["conversation_id"]
                .as_str()
                == Some(id)
            && !allows(client, ManageOrgSettings, Target::Org(org))
        {
            return Err(denied());
        }
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn guard_sensitive_approvals(
    db: &ScopedConnection,
    client: &Client,
    method: &str,
    parts: &[&str],
    is_owner: bool,
) -> Result<(), Reply> {
    if let ("POST", ["conversations", id, "land" | "install-release" | "rollback"]) =
        (method, parts)
        && !can_control_conversation(db, client, id).map_err(|e| internal(&e))?
    {
        return Err(denied());
    }
    let human_owner = is_owner && client.kind != Kind::Lead;
    if matches!((method, parts), ("GET", ["browser-action-screenshots", _])) && !human_owner {
        return Err(denied());
    }
    if matches!((method, parts), ("POST", ["action-approvals", _, "clear"])) && !human_owner {
        return Err(denied());
    }
    if let ("POST", ["action-approvals", id, "decide" | "answer"]) = (method, parts)
        && !human_owner
        && db
            .prepare(&format!(
                "SELECT 1 FROM action_approvals WHERE +org_id = :org_id AND id = ?1
                 AND ({OWNER_ONLY_APPROVAL})",
            ))
            .and_then(|mut stmt| stmt.exists(params![id]))
            .map_err(|e| internal(&e))?
    {
        return Err(denied());
    }
    Ok(())
}

// Hard-line requests are not listed: an assigned admin may decide them, as may any owner.
pub const OWNER_ONLY_APPROVAL: &str =
    "COALESCE(json_extract(request,'$.kind') IN ('land','install','rollback','browser_action'),0)";

pub fn approval_filter(client: &Client) -> Option<&str> {
    (client.kind != Kind::Lead && client.member.scope.role != Some(Role::Owner))
        .then(|| client.member_id())
}

pub fn approval_visible(
    db: &ScopedConnection,
    client: &Client,
    id: &str,
) -> rusqlite::Result<bool> {
    let org = &db.scope().org_id;
    if client.kind == Kind::Lead {
        if !allows(client, ReadWork, Target::Org(org)) {
            return Ok(false);
        }
        let target = db
            .query_row(
                "SELECT job_id, conversation_id FROM action_approvals WHERE +org_id = :org_id AND id = ?1",
                params![id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        return match target {
            Some((_, Some(conversation))) => {
                crate::lead::visible(db, client, Conversation, &conversation)
            }
            Some((Some(job), None)) => crate::lead::visible(db, client, Job, &job),
            _ => Ok(false),
        };
    }
    if !allows(client, ApproveAction, Target::Org(org)) {
        return Ok(false);
    }
    if approval_filter(client).is_none() {
        return Ok(true);
    }
    let member: Option<String> = db.query_row(
        &format!("SELECT assigned_member_id FROM action_approvals WHERE +org_id = :org_id AND id = ?1 AND assigned_member_id IS NOT NULL AND NOT ({OWNER_ONLY_APPROVAL})"),
        params![id], |row| row.get(0),
    ).optional()?;
    Ok(member.is_some_and(|member| allows(client, ApproveAction, Target::Member(org, &member))))
}

struct WriteRequest {
    state: std::sync::Arc<State>,
    call: Call,
    client: Client,
    refusal: Option<Reply>,
}

thread_local! {
    // Client handlers are synchronous and never hold the request context across an await.
    static WRITE_REQUEST: std::cell::RefCell<Option<WriteRequest>> =
        const { std::cell::RefCell::new(None) };
}

pub fn dispatch_mutation(
    state: &std::sync::Arc<State>,
    call: &Call,
    client: &Client,
    dispatch: impl FnOnce() -> Reply,
) -> Reply {
    WRITE_REQUEST.with(|current| {
        let _restore = WriteContext(current.replace(Some(WriteRequest {
            state: state.clone(),
            call: call.clone(),
            client: client.clone(),
            refusal: None,
        })));
        let reply = dispatch();
        current
            .borrow_mut()
            .as_mut()
            .and_then(|request| request.refusal.take())
            .unwrap_or(reply)
    })
}

struct WriteContext(Option<WriteRequest>);

impl Drop for WriteContext {
    fn drop(&mut self) {
        WRITE_REQUEST.with(|current| current.replace(self.0.take()));
    }
}

pub fn has_write_request() -> bool {
    WRITE_REQUEST.with_borrow(Option::is_some)
}

/// Called after BEGIN IMMEDIATE on the same connection that will perform the write.
pub fn recheck_write(db: &ScopedConnection) -> rusqlite::Result<()> {
    WRITE_REQUEST.with_borrow_mut(|current| {
        let Some(request) = current else {
            return Ok(());
        };
        let refusal = if let Some(fresh) = request.client.reload_member(db)? {
            let parts: Vec<_> = request
                .call
                .path
                .strip_prefix(crate::client_api::PREFIX)
                .unwrap_or_default()
                .split('/')
                .collect();
            guard_on(&request.state, db, &request.call, &fresh, &parts).err()
        } else {
            Some(refusal(
                401,
                "unauthenticated",
                "a client credential is required",
                Value::Null,
            ))
        };
        if let Some(refusal) = refusal {
            request.refusal = Some(refusal);
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_AUTH),
                Some("client authorization changed before the write".into()),
            ));
        }
        Ok(())
    })
}

thread_local! {
    // Dispatch and record rendering are synchronous. The guard restores the previous identity.
    static READ_CLIENT: std::cell::RefCell<Option<Client>> = const { std::cell::RefCell::new(None) };
}

pub struct ReadContext(Option<Client>);

impl Drop for ReadContext {
    fn drop(&mut self) {
        READ_CLIENT.with(|client| client.replace(self.0.take()));
    }
}

pub fn enter_read_context(client: &Client) -> ReadContext {
    ReadContext(READ_CLIENT.with(|current| current.replace(Some(client.clone()))))
}

pub fn projected_action(
    db: &ScopedConnection,
    method: &str,
    parts: &[&str],
    adoption: bool,
) -> bool {
    READ_CLIENT.with_borrow(|current| {
        let Some(client) = current.as_ref() else { return false };
        let body = if adoption {
            serde_json::json!({"sponsor_member_id": client.member_id()})
        } else {
            Value::Null
        };
        let call = Call {
            method: method.into(),
            path: String::new(),
            query: String::new(),
            headers: hyper::HeaderMap::new(),
            body: bytes::Bytes::new(),
        };
        authorize(db, &call, client, parts, &body).is_ok()
            && match (method, parts) {
                ("POST", ["action-approvals", id, "clear"]) => {
                    crate::lead_install::can_clear(db, id).unwrap_or(false)
                        || crate::action_approvals::browser::can_clear(db, id).unwrap_or(false)
                }
                _ => true,
            }
            && (!adoption || matches!(parts, ["conversations", id]
                if crate::sponsorship::can_adopt(db, client, id)))
            && (!matches!(parts, ["conversations", _, "archive"])
                || matches!(parts, ["conversations", id, "archive"] if crate::chief_of_staff::protect(db, id).is_ok()))
    })
}

pub fn approval_read_scope(db: &ScopedConnection) -> (bool, Option<String>) {
    READ_CLIENT.with_borrow(|client| {
        client.as_ref().map_or((false, None), |client| {
            (
                allows(client, ApproveAction, Target::Org(&db.scope().org_id)),
                approval_filter(client).map(String::from),
            )
        })
    })
}

pub fn attention_visible(assignee: Option<&str>, raised_by: Option<&str>) -> bool {
    READ_CLIENT.with_borrow(|client| {
        client.as_ref().is_none_or(|client| {
            if client.kind == Kind::Lead {
                return raised_by == Some(client.id.as_str());
            }
            client.member.scope.role == Some(Role::Owner) || assignee == Some(client.member_id())
        })
    })
}

pub(crate) fn eligible_role(role: Role, capability: Capability) -> bool {
    CAPABILITIES
        .iter()
        .any(|(cap, _, minimum, _)| *cap == capability && role >= *minimum)
}

#[allow(clippy::result_large_err)]
pub fn stream_target(
    state: &State,
    client: &Client,
    conversation: Option<&str>,
) -> Result<(), Reply> {
    let db = state.store.db();
    if !allows(client, ReadWork, Target::Org(&db.scope().org_id)) {
        return Err(denied());
    }
    if client.member.scope.role != Some(Role::Owner)
        && let Some(id) = conversation
        && responsible(&db, Conversation, id)
            .map_err(|e| internal(&e))?
            .is_none()
    {
        return Err(crate::client_api::not_found());
    }
    Ok(())
}

pub fn can_control_conversation(
    db: &ScopedConnection,
    client: &Client,
    id: &str,
) -> rusqlite::Result<bool> {
    scoped(db, client, Operation(ControlWork, Some((Conversation, id))))
}

pub fn node_land_route(method: &str, path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    matches!(
        (method, parts.as_slice()),
        ("GET", ["land-requests"])
            | (
                "POST",
                [
                    "land-requests",
                    _,
                    "inspect" | "unavailable" | "claim" | "outcome"
                ]
            )
    )
}

pub fn node_browser_action_route(method: &str, path: &str) -> bool {
    (method, path) == ("GET", "browser-action-jobs")
}

pub fn node_release_route(method: &str, path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    matches!(
        (method, parts.as_slice()),
        ("GET", ["release-requests"])
            | (
                "POST",
                [
                    "release-requests",
                    _,
                    "inspect" | "claim" | "outcome" | "unavailable"
                ]
            )
    )
}

pub fn ui_route(method: &str, path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    matches!(
        (method, parts.as_slice()),
        (
            "GET",
            ["nodes" | "jobs" | "conversations" | "models" | "events"]
                | ["conversations", _, "thread"]
        ) | ("POST", ["conversations"] | ["conversations", _, "turns"])
    )
}

pub fn ui_session_route(method: &str, path: &str) -> Option<Capability> {
    match (method, path) {
        ("GET", "/ui/session") => Some(ReadWork),
        ("POST", "/ui/logout") => Some(ManageOwnDevices),
        _ => None,
    }
}

pub fn local_route(path: &[&str]) -> Option<Capability> {
    match path {
        ["node", "register" | "revoke", _] => Some(ManageOrgSettings),
        ["job", "create"] => Some(StartWork),
        ["pair", _] | ["credential", "list"] | ["credential", "revoke", _] | ["ui-link"] => {
            Some(ManageOwnDevices)
        }
        ["installation"] => Some(ReadWork),
        _ => None,
    }
}

pub fn eligible_sponsor(db: &ScopedConnection, id: &str) -> rusqlite::Result<bool> {
    let Some(member) = crate::access::Member::load(db, id.to_owned())? else {
        return Ok(false);
    };
    Ok(member.active
        && member
            .scope
            .role
            .is_some_and(|role| eligible_role(role, SponsorLead)))
}

#[cfg(test)]
#[test]
fn disabled_after_authorization_cannot_start_work()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (_temp, state, owner, _) = crate::events::tests::fixture()?;
    let member_id = "race-member";
    let (_, token) = {
        let db = state.store.db();
        db.execute(
            "INSERT INTO members (org_id,id,display_name,role,status)
             VALUES (:org_id,?1,'Race member','member','active')",
            [member_id],
        )?;
        crate::access::mint_device(&db, member_id, "race-device")?
    };
    let client = crate::access::client(&state.store, &token)?.ok_or("missing device")?;
    let bytes = b"work bundle submitted after disable";
    let sha = crate::util::sha256_hex(bytes);
    let call = Call {
        method: "PUT".into(),
        path: format!("/api/v1/client/objects/{sha}"),
        query: String::new(),
        headers: hyper::HeaderMap::new(),
        body: bytes.as_slice().into(),
    };
    assert!(guard(&state, &call, &client, &["objects", &sha]).is_ok());

    let disable = Call {
        method: "PATCH".into(),
        path: format!("/api/v1/client/members/{member_id}"),
        query: String::new(),
        headers: [(
            hyper::header::HeaderName::from_static("idempotency-key"),
            hyper::header::HeaderValue::from_static("disable-during-write"),
        )]
        .into_iter()
        .collect(),
        body: br#"{"status":"disabled"}"#.as_slice().into(),
    };
    assert_eq!(
        crate::client_api::handle_as(&state, &disable, &owner).status(),
        200
    );
    // The request still holds the previously authenticated principal.
    assert_eq!(
        crate::client_api::handle_as(&state, &call, &client).status(),
        401
    );
    assert!(!state.store.has_object(&sha));
    let db = state.store.db();
    assert!(
        !db.prepare("SELECT 1 FROM lead_objects WHERE +org_id=:org_id AND sha256=?1")?
            .exists([sha])?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_reads_require_an_explicit_context() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = crate::store::Store::open(&temp.path().join("director"))?;
        assert_eq!(approval_read_scope(&store.db()), (false, None));
        Ok(())
    }

    #[test]
    fn capability_matrix_and_projection() {
        let viewer = [ReadWork, ManageOwnDevices, ReadMemberDirectory, RenameSelf];
        let member = [
            StartWork,
            ControlWork,
            DecideRun,
            EditBacklog,
            EditDecisionLog,
            SponsorLead,
        ];
        let admin = [
            ApproveAction,
            ReadSourceBudgets,
            ManageDirectorBudgets,
            EditProjects,
            ManageSchedules,
            ManageMembers,
            ManageMemberDevices,
        ];
        let owner = [ManageSourceBudgets, ManageOrgSettings];
        let delegated = [
            StartWork,
            ControlWork,
            DecideRun,
            EditBacklog,
            EditDecisionLog,
        ];
        for role in [Role::Viewer, Role::Member, Role::Admin, Role::Owner] {
            for kind in [
                Kind::Local,
                Kind::Device,
                Kind::Browser {
                    session_id: "session".into(),
                },
                Kind::Lead,
            ] {
                let client = Client {
                    id: "actor".into(),
                    kind,
                    member: crate::access::Member {
                        scope: crate::store::OrgScope {
                            org_id: "org".into(),
                            member_id: "self".into(),
                            role: Some(role),
                        },
                        display_name: "Member".into(),
                        active: true,
                    },
                    conversation_id: None,
                    intent_id: None,
                    node: None,
                };
                let projected = projection(&client);
                for &(cap, name, _, _) in CAPABILITIES {
                    let expected = if client.kind == Kind::Lead {
                        cap == ReadWork || (role >= Role::Member && delegated.contains(&cap))
                    } else {
                        viewer.contains(&cap)
                            || (role >= Role::Member && member.contains(&cap))
                            || (role >= Role::Admin && admin.contains(&cap))
                            || (role == Role::Owner && owner.contains(&cap))
                            || (role == Role::Owner && cap == Land && client.kind == Kind::Local)
                    };
                    let actual = allows(&client, cap, Target::Org("org"));
                    assert_eq!(actual, expected, "{name}, {role:?}, {:?}", client.kind);
                    assert_eq!(projected.contains(&name), actual, "{name} projection");
                    assert_eq!(
                        allows(&client, cap, Target::Member("org", "self")),
                        expected
                    );
                    assert!(!allows(&client, cap, Target::Org("foreign")));
                    let other = expected
                        && match cap {
                            ApproveAction | ManageOwnDevices => role == Role::Owner,
                            StartWork | ControlWork | DecideRun | SponsorLead => {
                                role >= Role::Admin
                            }
                            _ => true,
                        };
                    assert_eq!(allows(&client, cap, Target::Member("org", "other")), other);
                }
            }
        }
    }

    #[test]
    fn client_operations_are_classified() {
        for (method, cap, paths) in [
            (
                "GET",
                ReadWork,
                "me lead-node chief-of-staff nodes nodes/id agents agents/id models repos events intents intents/id intents/id/work-trace jobs jobs/id jobs/id/files/path runs runs/id runs/id/files/path runs/id/decision runs/id/events decisions/id cancels/id objects/sha conversations conversations/id conversations/id/thread conversations/id/smoke turns/id summaries/id projects projects/id projects/id/backlog projects/id/backlog/id projects/id/backlog.md projects/id/decision_log projects/id/decision_log/id projects/id/heartbeat usage schedules schedules/node/id schedule-changes/id",
            ),
            (
                "POST",
                StartWork,
                "intents intents/id/jobs conversations conversations/id/turns conversations/id/wake-turns conversations/id/peer-messages conversations/id/land conversations/id/install-release conversations/id/rollback conversations/id/notes conversations/id/summaries/id/redo",
            ),
            ("PUT", StartWork, "objects/sha"),
            (
                "POST",
                ControlWork,
                "jobs/id/cancel conversations/id/archive conversations/id/unarchive conversations/id/stop conversations/id/resume",
            ),
            ("PATCH", ControlWork, "conversations/id"),
            (
                "DELETE",
                ControlWork,
                "conversations/id conversations/id/attention",
            ),
            (
                "PUT",
                ControlWork,
                "conversations/id/attention intents/id/owner-messages/id/mark",
            ),
            ("POST", DecideRun, "runs/id/decision"),
            ("GET", ApproveAction, "action-approvals action-approvals/id"),
            (
                "POST",
                ApproveAction,
                "action-approvals/id/decide action-approvals/id/answer action-approvals/id/clear",
            ),
            ("GET", ReadSourceBudgets, "source-budgets"),
            (
                "PUT",
                ManageSourceBudgets,
                "source-budgets source-budgets/source",
            ),
            ("DELETE", ManageSourceBudgets, "source-budgets/source"),
            (
                "PUT",
                ManageDirectorBudgets,
                "conversations/id/budget projects/id/budget",
            ),
            (
                "DELETE",
                ManageDirectorBudgets,
                "conversations/id/budget projects/id/budget",
            ),
            ("POST", EditProjects, "projects"),
            (
                "POST",
                EditBacklog,
                "projects/id/backlog projects/id/backlog/reorder projects/id/backlog/id/update projects/id/backlog/id/answer",
            ),
            (
                "POST",
                EditDecisionLog,
                "projects/id/decision_log projects/id/decision_log/import projects/id/decision_log/id/revise projects/id/decision_log/id/supersede projects/id/decision_log/id/retire",
            ),
            (
                "POST",
                ManageSchedules,
                "schedules schedules/node/id/changes",
            ),
            ("DELETE", ManageSchedules, "schedules/node/id"),
            ("PATCH", ManageSchedules, "schedules/node/id"),
            ("PUT", ManageSchedules, "projects/id/heartbeat"),
            ("GET", StartWork, "conversations/id/peer-messages"),
            ("GET", ManageOrgSettings, "peer-message-allowlist"),
            (
                "PUT",
                ManageOrgSettings,
                "lead-node chief-of-staff peer-message-allowlist",
            ),
            ("POST", SponsorLead, "credentials/lead"),
            ("GET", ManageMembers, "members invites"),
            ("GET", ReadMemberDirectory, "members/directory"),
            ("GET", ManageOrgSettings, "advertised-url"),
            ("PUT", ManageOrgSettings, "advertised-url"),
            ("POST", ManageMembers, "invites"),
            ("PATCH", ManageMembers, "members/id"),
            ("DELETE", ManageMembers, "invites/id"),
            ("GET", ManageOwnDevices, "credentials"),
            ("POST", ManageOwnDevices, "pairings"),
            ("DELETE", ManageOwnDevices, "credentials/id"),
        ] {
            for path in paths.split_whitespace() {
                let segments: Vec<_> = path.split('/').collect();
                assert!(
                    route(method, &segments).is_some_and(|op| op.0 == cap),
                    "{method} {path}"
                );
            }
        }
        assert!(route("POST", &["runs", "id", "land"]).is_none());
        assert!(route("POST", &["unknown"]).is_none());
    }
}
