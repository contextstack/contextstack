"""Effective role enforcement against real device credentials."""
import hashlib
import json
import sqlite3
import subprocess
import sys
import time
from contextlib import ExitStack, closing
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from threading import Barrier
from urllib.parse import parse_qs, urlsplit

from cases.budgets import project
from cases.client import accept, finish, lead, owner, submitted
from cases.conversations import create_body, created, post, thread, turn
from cases.decision_log import CONTENT
from cases.protocol2 import create_schedule
from harness.client import Client, new_key
from harness.run import case
from harness.targets import RustTarget
from harness.wire import request


def device(ctx, role):
    token, credential = ctx.target.pair("policy-" + role)
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        org, = db.execute("SELECT org_id FROM credentials WHERE id = ?", (credential,)).fetchone()
        member = "policy-" + credential
        db.execute("INSERT INTO members VALUES (?, ?, ?, ?, 'active')", (member, org, role, role))
        db.execute("UPDATE credentials SET member_id = ? WHERE id = ?", (member, credential))
    return Client(ctx.target, token, credential)


@case("client", "policy", "effective-role-settings", ("rust",))
def effective_role_settings(ctx):
    local = owner(ctx)
    node = ctx.node2()
    before = local.call("GET", "lead-node").body
    expected = set()
    role_additions = {
        "viewer": "read_work manage_own_devices read_member_directory rename_self",
        "member": "start_work control_work decide_run edit_backlog edit_decision_log sponsor_lead",
        "admin": "approve_action read_source_budgets manage_director_budgets edit_projects manage_schedules manage_members manage_member_devices",
        "owner": "manage_source_budgets manage_org_settings",
    }
    for role, additions in role_additions.items():
        expected.update(additions.split())
        actor = device(ctx, role)
        me = actor.call("GET", "me").json
        ctx.expect(me["role"] == role and set(me["capabilities"]) == expected, "full device capabilities", me)
        ctx.expect(actor.call("GET", "nodes").status == 200, "org work is visible")
        denied = actor.call("PUT", "lead-node", body={"node": node.name})
        ctx.expect(denied.status == (200 if role == "owner" else 403), "settings are owner-only", denied)
        if role != "owner":
            ctx.expect(local.call("GET", "lead-node").body == before, "refusal preserves settings")
    ctx.expect(set(local.call("GET", "me").json["capabilities"]) == expected | {"land"},
               "full local owner capabilities")
    ctx.expect(local.call("PUT", "lead-node", body={"node": node.name}).status == 200, "owner can edit")


@case("client", "policy", "work-ownership-and-reconnect", ("rust",))
def work_ownership_and_reconnect(ctx):
    local, node = owner(ctx), ctx.node2()
    member, other, viewer, admin = (device(ctx, role) for role in ("member", "member", "viewer", "admin"))
    conversation = created(ctx, member, node)
    cid = conversation["conversation_id"]
    path = "conversations/" + cid
    key = new_key()
    before = local.call("GET", path + "/thread").json
    body = {"message": "Run a turn"}
    for actor in (other, viewer):
        actual = actor.call("GET", path + "/thread").json
        ctx.expect(actual == {**before, "conversation": {**before["conversation"],
                   "actions": {"control": False, "adopt": False, "archive": False}}}, "org-wide thread reads")
        ctx.expect(actor.call("POST", path + "/turns", body=body, key=key).status == 403,
                   "other members cannot drive the thread")
        ctx.expect(actor.call("POST", path + "/archive", key=new_key()).status == 403,
                   "other members cannot archive the thread")
    ctx.expect(local.call("GET", path + "/thread").json == before, "refusals preserve the thread")
    turn = post(ctx, member, conversation, body["message"], key=key)
    ctx.expect(admin.call("POST", path + "/turns", body=body, key=new_key()).status == 201,
               "admin drives another member's conversation")
    minted = member.call("POST", "credentials/lead", body={"name": "own", "conversation_id": cid})
    ctx.expect(minted.status == 201, "member can sponsor own conversation", minted)
    def lead_ids():
        with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
            return db.execute("SELECT id FROM credentials WHERE kind = 'lead' ORDER BY id").fetchall()

    before_mint = lead_ids()
    ctx.expect(other.call("POST", "credentials/lead", body={"name": "other", "conversation_id": cid}).status == 403,
               "other member cannot sponsor")
    ctx.expect(viewer.call("POST", "credentials/lead", body={"name": "viewer", "conversation_id": cid}).status == 403,
               "viewer cannot sponsor")
    ctx.expect(lead_ids() == before_mint, "mint refusals leave credentials unchanged")
    token = Client(ctx.target, minted.json["token"], minted.json["credential_id"])
    ctx.expect(set(token.call("GET", "me").json["capabilities"]) ==
               set("read_work start_work control_work decide_run edit_backlog edit_decision_log".split()),
               "full member-sponsored lead capabilities")
    ctx.expect(other.call("DELETE", "credentials/" + token.credential_id).status == 404, "foreign lead revoke")
    ctx.expect(member.call("DELETE", "credentials/" + token.credential_id).status == 200, "minter revokes own lead")
    ctx.expect(token.call("GET", "me").status == 401, "revoked lead cannot authenticate")

    key = new_key()
    member.upload_bundle()
    bundle = member.submit(node.name, key=key)
    ctx.expect(bundle.status == 201, "member can start new work", bundle)
    other.upload_bundle()
    collision = other.submit(node.name, key=key)
    ctx.expect(collision.status == 409 and "record" not in collision.json, "replay collision hides receipt", collision)
    ctx.expect(member.submit(node.name, key=key).header("idempotent-replayed") == "true", "own replay")
    job = bundle.json["jobs"][0]
    jid = job["job_id"]
    iid = bundle.json["intent"]["intent_id"]
    ctx.expect(viewer.call("GET", "jobs/" + jid).status == 200, "viewer reads job")
    file = next(f for f in job["files"] if f["path"] == "spec.yaml")
    ctx.expect(viewer.call("GET", "objects/" + file["sha256"]).status == 200, "viewer reads bundle")
    ctx.expect(other.call("POST", f"jobs/{jid}/cancel", body={}, key=new_key()).status == 403, "foreign cancel")
    ctx.expect(viewer.call("POST", f"jobs/{jid}/cancel", body={}, key=new_key()).status == 403, "viewer cancel")
    ctx.expect(local.call("GET", "jobs/" + jid).json == job, "cancel refusal preserves job")
    for method, route, payload in (
        ("PATCH", path, {"title": "Blocked"}),
        ("DELETE", path, {}),
        ("POST", path + "/stop", {}),
        ("POST", path + "/resume", {}),
        ("PUT", path + "/attention", {"question": "Blocked"}),
        ("DELETE", path + "/attention", {}),
        ("POST", path + "/summaries/missing/redo", {}),
        ("PUT", f"intents/{iid}/owner-messages/{turn['turn_id']}/mark", {"mark": "answer"}),
        ("POST", f"intents/{iid}/jobs", other.intent(node.name)),
        ("POST", "intents", other.intent(node.name, files=None, from_job=turn["job_id"], task="tasks/turn")),
        ("POST", "conversations", create_body(other, node, intent_id=iid)),
        ("POST", "credentials/lead",
         {"name": "linked", "conversation_id": created(ctx, other, node)["conversation_id"], "intent_id": iid}),
        ("POST", "intents", other.intent(node.name, origin={
            "kind": "conversation_turn", "conversation_id": cid, "turn_id": turn["turn_id"],
        })),
    ):
        result = other.call(method, route, body=payload, key=new_key())
        ctx.expect(result.status == 403, f"foreign work refused at {method} {route}", result)
    ctx.expect(lead_ids() == before_mint, "foreign intent binding leaves credentials unchanged")
    ctx.expect(viewer.submit(node.name).status == 403, "viewer cannot submit")
    ctx.expect(viewer.call("PUT", "objects/" + hashlib.sha256(b"test").hexdigest(), raw=b"test").status == 403,
               "viewer cannot upload")
    cancel_job = submitted(ctx, member, node)["jobs"][0]["job_id"]
    ctx.expect(admin.call("POST", f"jobs/{cancel_job}/cancel", body={}, key=new_key()).status == 202,
               "admin cancels another member's job")
    accept(ctx, node, jid)
    rid = finish(ctx, node, jid)["envelope"]["run_id"]
    decision = "runs/" + rid + "/decision"
    body = {"decision": "approve", "land": {"state": "landed"}}
    ctx.expect(other.call("POST", decision, body=body, key=new_key()).status == 403, "foreign decision")
    ctx.expect(viewer.call("POST", decision, body=body, key=new_key()).status == 403, "viewer decision")
    ctx.expect(local.call("GET", decision).status == 404, "refusal leaves no decision")
    decided = admin.call("POST", decision, body=body, key=new_key())
    ctx.expect(decided.status == 201, "admin decides another member's run", decided)
    ctx.expect(not local.call("GET", "runs/" + rid).json.get("land"), "approval cannot land")
    ctx.expect(local.call("POST", "runs/" + rid + "/land", body=body).status == 404, "no remote land route")

    stream = viewer.stream_for(cid)
    db_path = ctx.target.data / "director.sqlite"
    try:
        ctx.expect(stream.status == 200 and stream.next()[0] == "hello", "viewer stream opens")
        with sqlite3.connect(db_path) as db:
            db.execute("UPDATE members SET role = 'member' WHERE id = ?", ("policy-" + viewer.credential_id,))
        member.call("PATCH", path, body={"title": "New title"}, key=new_key())
        ctx.expect(stream.next(timeout=5)[0] == "revoked" and stream.next(timeout=5) is None and stream.ended,
                   "role change closes old stream")
    finally:
        stream.close()
    stream = viewer.stream_for(cid)
    try:
        ctx.expect(stream.status == 200 and stream.next()[0] == "hello", "reconnect uses new role")
    finally:
        stream.close()
    stream = viewer.stream_for("foreign-conversation")
    try:
        ctx.expect(stream.status == 404, "unknown conversation cannot open a scoped stream")
    finally:
        stream.close()


@case("client", "policy", "project-budget-and-approval-authority", ("rust",))
def project_budget_and_approval_authority(ctx):
    local, node = owner(ctx), ctx.node2()
    admin, other, member, viewer = (device(ctx, role) for role in ("admin", "admin", "member", "viewer"))
    ctx.expect("land" in local.call("GET", "me").json["capabilities"], "host owner can land")
    paired_owner = device(ctx, "owner")
    ctx.expect("land" not in paired_owner.call("GET", "me").json["capabilities"], "paired owner cannot land")
    before = local.call("GET", "source-budgets").body
    ctx.expect(admin.call("GET", "source-budgets").body == before, "admin can read source budgets")
    ctx.expect(member.call("GET", "source-budgets").status == 403, "member cannot read source budgets")
    ctx.expect(admin.call("PUT", "source-budgets", body={}).status == 403, "only owner can write source budgets")
    ctx.expect(local.call("GET", "source-budgets").body == before, "source limits survive refusal")
    ctx.expect(local.call("PUT", "source-budgets/openai", body={"usd": 12, "period": "day"}).status == 200,
               "owner can raise a source limit")
    ctx.expect(admin.call("DELETE", "source-budgets/openai").status == 403, "admin cannot delete source budget")
    ctx.expect(admin.call("PUT", "chief-of-staff", body={"enabled": True}, key=new_key()).status == 403,
               "admin cannot configure chief of staff")
    chief = local.call("PUT", "chief-of-staff", body={"enabled": True}, key=new_key())
    ctx.expect(chief.status == 200, "owner enables chief of staff", chief)
    chief_id = chief.json["chief_of_staff"]["conversation_id"]
    ctx.expect(admin.call("PATCH", f"conversations/{chief_id}", body={"title": "Blocked"}, key=new_key()).status == 403,
               "admin cannot rename chief of staff")
    before = local.call("GET", "projects").body
    ctx.expect(member.call("POST", "projects", body={"name": "blocked", "budget": {"usd": 1}}, key=new_key()).status == 403,
               "member cannot create projects")
    ctx.expect(local.call("GET", "projects").body == before, "project refusal leaves rows alone")
    pid = project(ctx, admin, "Policy project", {"usd": 10})["project_id"]
    path = "projects/" + pid
    before = local.call("GET", path).body
    ctx.expect(member.call("PUT", path + "/budget", body={"usd": 25}).status == 403, "member cannot set caps")
    ctx.expect(local.call("GET", path).body == before, "cap refusal preserves project")
    ctx.expect(admin.call("PUT", path + "/budget", body={"usd": 25}).status == 200, "admin edits Director budget")
    schedule = {"node": node.name, "name": "Nightly", "cron": "0 0 * * *", "timezone": "UTC",
                "spec": "spec.yaml", "task": "tasks/t", "budget": {"usd": 5}}
    before = local.call("GET", "schedules").body
    key = new_key()
    ctx.expect(member.call("POST", "schedules", body=schedule, key=key).status == 403, "member cannot create schedule")
    ctx.expect(local.call("GET", "schedules").body == before, "schedule refusal preserves list")
    ctx.expect(admin.call("POST", "schedules", body=schedule, key=key).status == 202, "admin schedules with budget")
    body = create_body(member, node, budget={"usd": 5})
    key = new_key()
    ctx.expect(member.call("POST", "conversations", body=body, key=key).status == 403, "inline cap denied")
    del body["budget"]
    made = member.call("POST", "conversations", body=body, key=key)
    ctx.expect(made.status == 201, "refusal saves no replay")
    budget = f"conversations/{made.json['conversation']['conversation_id']}/budget"
    ctx.expect(admin.call("PUT", budget, body={"usd": 5}).status == 200, "admin sets conversation budget")
    ctx.expect(admin.call("DELETE", budget).status == 200, "admin clears conversation budget")
    for stem, body, field in (("backlog", {"title": "Make a change"}, "items"),
                              ("decision_log", CONTENT, "entries")):
        route = path + "/" + stem
        key = new_key()
        ctx.expect(viewer.call("POST", route, body=body, key=key).status == 403, "viewer edit refused")
        ctx.expect(local.call("GET", route).json[field] == [], "refusal leaves project records alone")
        ctx.expect(member.call("POST", route, body=body, key=key).status == 201, "member edits shared records")

    job = submitted(ctx, admin, node)["jobs"][0]["job_id"]
    accept(ctx, node, job)
    request = {"run_id": "policy-action", "argv": ["/usr/bin/curl", "https://example.test"],
                "context": {"home": "/home/alice", "workspace": "/home/alice/work"}, "needs": ["network"],
               "requesting_model": "requester", "reason": "publish work"}
    sent = node.call("POST", f"jobs/{job}/action-approvals", body=request)
    ctx.expect(sent.status == 200, "node submits its approval request", sent)
    path = "action-approvals/" + sent.json["id"]
    for _ in range(80):
        row = local.call("GET", path).json
        if row["status"] == "needs_owner":
            break
        time.sleep(0.025)
    ctx.expect(row["status"] == "needs_owner", "approval escalates")
    ctx.expect(other.call("GET", "action-approvals").json["action_approvals"] == [], "other admin inbox excludes item")
    bound = lead(ctx, admin, created(ctx, admin, node))
    response = bound.call("POST", path + "/answer", body={"choice": "no_go"})
    ctx.expect(response.status in (403, 404), "assigned admin's lead cannot answer", response)
    ctx.expect(local.call("GET", path).json == row, "lead refusal leaves request unchanged")
    for actor, status in ((other, 404), (member, 403), (viewer, 403)):
        ctx.expect(actor.call("POST", path + "/decide", body={"decision": "deny"}).status == status,
                   "unassigned decision refused")
        ctx.expect(actor.call("POST", path + "/answer", body={"choice": "no_go"}).status == status,
                   "needs-you answer has the same approval authority")
    ctx.expect(local.call("GET", path).json == row, "refusals leave request unchanged")
    decision = admin.call("POST", path + "/answer", body={"choice": "no_go"})
    ctx.expect(decision.status == 200 and decision.json["status"] == "denied"
               and decision.json["decided_by"] == "policy-" + admin.credential_id,
               "assigned admin answers and records the member", decision)


@case("client", "policy", "approval-visibility", ("rust",))
def approval_visibility(ctx):
    local, node = owner(ctx), ctx.node2()
    admin, other = (device(ctx, "admin") for _ in range(2))
    owner_inbox = {row["id"] for row in local.call("GET", "action-approvals").json["action_approvals"]}
    with ExitStack() as stack:
        streams = [actor.stream() for actor in (local, admin, other)]
        for stream in streams:
            stack.callback(stream.close)
            ctx.expect(stream.next()[0] == "hello", "stream ready")
        items = []
        for idx, actor in enumerate((admin, other)):
            bundle = submitted(ctx, actor, node)
            iid, jid = bundle["intent"]["intent_id"], bundle["jobs"][0]["job_id"]
            cid = created(ctx, actor, node, intent_id=iid)["conversation_id"]
            accept(ctx, node, jid)
            sent = node.call("POST", f"jobs/{jid}/action-approvals", body={
                "run_id": f"policy-visibility-{idx}", "argv": ["/usr/bin/curl", "https://example.test"],
                "context": {"home": "/home/alice", "workspace": "/home/alice/work"},
                "needs": ["network"], "requesting_model": "requester", "reason": "publish work",
            })
            ctx.expect(sent.status == 200, "approval submitted", sent)
            aid = sent.json["id"]
            for _ in range(80):
                if local.call("GET", "action-approvals/" + aid).json["status"] == "needs_owner":
                    break
                time.sleep(0.025)
            ctx.expect(local.call("GET", "action-approvals/" + aid).json["status"] == "needs_owner", "escalated")
            items.append((actor, iid, cid, aid))
        marker = created(ctx, local, node)["conversation_id"]
        for actor, stream in zip((local, admin, other), streams):
            expected = {aid for assignee, _, _, aid in items if actor in (local, assignee)}
            listed = actor.call("GET", "action-approvals").json["action_approvals"]
            ctx.expect({row["id"] for row in listed} == (expected | owner_inbox if actor == local else expected),
                       "approval list is scoped", listed)
            for _, iid, cid, aid in items:
                allowed = aid in expected
                filtered = actor.call("GET", f"action-approvals?conversation={cid}&status=needs_owner")
                ctx.expect(filtered.status == 200
                           and [row["id"] for row in filtered.json["action_approvals"]] == ([aid] if allowed else []),
                           "conversation filter limits visible job approvals to the linked conversation", filtered)
                detail = actor.call("GET", "action-approvals/" + aid)
                ctx.expect(detail.status == (200 if allowed else 404), "approval detail is scoped", detail)
                trace = actor.call("GET", f"intents/{iid}/work-trace").json["work_trace"]
                ctx.expect(any(item.get("action_approval_id") == aid for item in trace["needs_you"]) == allowed,
                           "needs_you is scoped", trace)
                conv = actor.call("GET", f"conversations/{cid}").json["conversation"]
                ctx.expect(((conv.get("attention") or {}).get("action_approval_id") == aid) == allowed,
                           "conversation attention is scoped", conv)
            end, seen = stream.until(lambda name, data: name == "change" and data["id"] == marker)
            ctx.expect(end is not None, "stream reaches marker")
            ctx.expect({data["id"] for name, data, _ in seen
                        if name == "change" and data["kind"] == "action_approval"} == expected,
                       "approval SSE events are scoped", seen)
            for _, _, cid, aid in items:
                ctx.expect(any(name == "change" and data["id"] == cid
                               and (data["record"].get("attention") or {}).get("action_approval_id") == aid
                               for name, data, _ in seen) == (aid in expected),
                           "conversation SSE attention is scoped", seen)


@case("client", "policy", "land-request-authority", ("rust",))
def land_request_authority(ctx):
    local, node = owner(ctx), ctx.node2()
    member, other, admin, viewer = (device(ctx, role) for role in ("member", "member", "admin", "viewer"))
    own = created(ctx, member, node)
    pid = project(ctx, local, "Land request authority")["project_id"]
    foreign = created(ctx, other, node, project_id=pid)["conversation_id"]
    hosted = lead(ctx, member, own)
    cid = own["conversation_id"]
    path = f"conversations/{cid}/land"
    foreign_path = f"conversations/{foreign}/land"
    payload = {"repo": "fixture", "lane": "policy-land-authority"}
    job = submitted(ctx, other, node, project_id=pid, meta=payload)["jobs"][0]["job_id"]
    accept(ctx, node, job)
    finish(ctx, node, job, run_id="policy-land-authority", row_fields={"lane": payload})
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE conversations SET lead = 1 WHERE conversation_id = ?", (foreign,))

    refused = hosted.call("POST", foreign_path, body=payload)
    ctx.expect(refused.status in (403, 404), "lead cannot request landing on another member's conversation", refused)
    refused = viewer.call("POST", foreign_path, body=payload)
    ctx.expect(refused.status == 403, "viewer cannot request landing", refused)

    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE members SET role = 'viewer' WHERE id = ?", ("policy-" + member.credential_id,))
    refused = hosted.call("POST", path, body=payload)
    ctx.expect(refused.status == 403, "lead uses the sponsor's live role", refused)

    allowed = other.call("POST", foreign_path, body=payload)
    ctx.expect(allowed.status == 200 and allowed.json["kind"] == "land",
               "the same lane is eligible when its conversation's member requests the land", allowed)
    aid = allowed.json["id"]
    inspected = node.call("POST", f"land-requests/{aid}/inspect",
                          body={"commit": "1" * 40, "base": "2" * 40, "protected": False, "tracing": False})
    ctx.expect(inspected.status == 200 and inspected.json["status"] == "needs_owner",
               "the node escalates the land approval", inspected)
    # Assign the approval to the admin so normal approval visibility would allow a decision.
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE action_approvals SET assigned_member_id = ? WHERE id = ?",
                   ("policy-" + admin.credential_id, aid))
    approval = f"action-approvals/{aid}"
    ctx.expect(admin.call("GET", approval).status == 404, "assignment does not expose an owner-only fixture")
    refused = admin.call("POST", approval + "/decide", body={"decision": "deny"})
    ctx.expect(refused.status == 403, "admin cannot decide a land approval", refused)
    refused = admin.call("POST", approval + "/answer", body={"choice": "no_go"})
    ctx.expect(refused.status == 403, "admin cannot answer a land approval", refused)
    admin_id = admin.call("GET", "me").json["member"]["id"]
    owner_id = local.call("GET", "me").json["member"]["id"]
    ctx.expect(local.call("PATCH", "members/" + admin_id, body={"role": "member"},
                          key=new_key()).status == 200, "demote the assigned admin")
    ctx.expect(local.call("GET", approval).json["assignee_member_id"] == owner_id,
               "land approval reroutes to the owner")
    for op in ("inspect", "unavailable", "claim", "outcome"):
        refused = request(ctx.target.url, "POST", f"/api/v1/nodes/me/land-requests/{aid}/{op}",
                          headers={"Authorization": "Bearer " + local.token}, body={})
        ctx.expect(refused.status == 401, "client credentials cannot use the land node protocol", refused)
    chosen = local.call("POST", approval + "/decide", body={"decision": "deny"})
    ctx.expect(chosen.status == 200 and chosen.json["status"] == "denied", "owner can decide", chosen)


@case("client", "policy", "hosted-release-approval", ("rust",))
def hosted_release_approval(ctx):
    local, node, other_node = owner(ctx), ctx.node2(), ctx.node2()
    admin = device(ctx, "admin")
    local.call("PUT", "lead-node", body={"node": node.name})
    conv = local.call("POST", "conversations", body={"node": node.name, "spec": "hosted-lead",
                      "model": "claude-code-sonnet"}, key=new_key()).json["conversation"]
    hosted = lead(ctx, local, conv)
    path = "conversations/" + conv["conversation_id"]
    response = hosted.call("POST", path + "/install-release", body={"bundle": "release-one"})
    ctx.expect(response.status == 200 and response.json["kind"] == "install"
               and response.json["status"] == "pending", "lead requests node inspection", response)
    approval = response.json["id"]
    ctx.expect(response.json.get("bundle") == "release-one", "approval includes the staged bundle name", response)
    node_path = "release-requests/" + approval
    client_path = "action-approvals/" + approval
    pin = {"commit": "1" * 40, "target_hash": "2" * 64, "schema_current": 13, "schema_target": 14,
           "release_notes_diff": "+Update the node"}
    ctx.expect(other_node.call("POST", node_path + "/inspect", body=pin).status == 404,
               "other nodes cannot inspect the approval")
    inspected = node.call("POST", node_path + "/inspect", body=pin)
    ctx.expect(inspected.status == 200 and inspected.json["status"] == "needs_owner"
               and all(inspected.json.get(k) == v for k, v in pin.items()), "approval shows the exact target", inspected)
    ctx.expect(inspected.json["commit_provenance"] == "claimed commit (from bundle.json)"
               and inspected.json["backup"] == "Local backup only", "staged bundle pin describes its source and backup")
    conversation = local.call("GET", path)
    ctx.expect(conversation.status == 200, "the approval can be shown in the conversation", conversation)
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE action_approvals SET assigned_member_id=? WHERE id=?",
                   ("policy-" + admin.credential_id, approval))

    filtered_path = "action-approvals?conversation=" + conv["conversation_id"]
    for actor in (local, admin, hosted):
        filtered = actor.call("GET", filtered_path)
        rows = filtered.json["action_approvals"]
        ctx.expect(filtered.status == 200 and [row["id"] for row in rows] == ([] if actor == admin else [approval]),
                   "conversation filter includes the linked release approval for authorized actors", filtered)
        if rows:
            ctx.expect(("bundle" in rows[0]) == (actor != hosted), "filtered lead list omits release details", rows[0])

    def check_owner_actions(approval_path):
        for actor, allowed in ((local, True), (hosted, False), (admin, False)):
            detail = actor.call("GET", approval_path)
            if actor == admin:
                ctx.expect(detail.status == 404, "release approval is hidden from non-owners", detail)
            else:
                ctx.expect(detail.status == 200 and detail.json.get("actions") == dict.fromkeys(
                    ("decide", "answer", "clear"), allowed
                ), "release approval projects owner-only actions", detail)
        for actor in (hosted, admin):
            for op, body in (("decide", {"decision": "approve"}), ("answer", {"choice": "go"}), ("clear", {})):
                refused = actor.call("POST", approval_path + "/" + op, body=body)
                ctx.expect(refused.status == 403, "only a human owner can resolve a release approval", refused)

    check_owner_actions(client_path)
    duplicate = hosted.call("POST", path + "/install-release", body={})
    ctx.expect(duplicate.status == 409, "only one release switch may be open", duplicate)
    claim = {"target_hash": pin["target_hash"]}
    ctx.expect(node.call("POST", node_path + "/claim", body=claim).status == 409,
               "execution waits for owner approval")
    decision = local.call("POST", client_path + "/decide", body={"decision": "approve"})
    ctx.expect(decision.status == 200 and decision.json["status"] == "approved", "owner approves", decision)
    ctx.expect(node.call("POST", node_path + "/inspect", body={**pin, "target_hash": "3" * 64}).status == 409,
               "inspection cannot change an approved hash")
    ctx.expect(node.call("POST", node_path + "/claim", body={"target_hash": "3" * 64}).status == 409,
               "approval cannot claim another hash")
    ctx.expect(node.call("POST", node_path + "/claim", body=claim).status == 200, "node claims the exact hash")
    receipt = {**claim, "status": "rolled_back", "release": "release-zero"}
    for _ in range(2):
        outcome = node.call("POST", node_path + "/outcome", body=receipt)
        ctx.expect(outcome.status == 200 and outcome.json["outcome"]["status"] == "rolled_back",
                   "red smoke rollback is durably recorded", outcome)
    smoke = hosted.call("GET", path + "/smoke")
    ctx.expect(smoke.status == 200 and smoke.json["last_smoke"]["smoke_ok"] is False, "smoke reads the red result", smoke)
    thread = local.call("GET", path + "/thread")
    wakes = [turn for turn in thread.json["turns"] if turn["origin"] == "wake"]
    ctx.expect(len(wakes) == 1 and wakes[0]["message"] == f"Post-install status for {approval}, commit "
               f"{pin['commit']}. Installation smoke failed and the previous release was restored. "
               "Current release: release-zero." and wakes[0]["detail"] == {"approval_id": approval},
               "one post-install wake turn describes and links to the approval", thread)
    wake = wakes[0]
    if wake["job_id"]:
        accept(ctx, node, wake["job_id"])
        finish(ctx, node, wake["job_id"])
        settled = turn(local, wake["turn_id"])
        ctx.expect(settled["state"] in ("done", "error") and settled["detail"]["approval_id"] == approval,
                   "the wake turn keeps its approval link after settlement", settled)
    rollback = hosted.call("POST", path + "/rollback", body={})
    ctx.expect(rollback.status == 200 and rollback.json["kind"] == "rollback"
               and rollback.json["status"] == "pending", "a new rollback requires approval", rollback)


    rb_client = "action-approvals/" + rollback.json["id"]
    rb_node = "release-requests/" + rollback.json["id"]
    ctx.expect(node.call("POST", rb_node + "/inspect", body=pin).status == 200, "pin rollback")
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE action_approvals SET assigned_member_id=? WHERE id=?",
                   ("policy-" + admin.credential_id, rollback.json["id"]))
    check_owner_actions(rb_client)
    ctx.expect(local.call("POST", rb_client + "/answer", body={"choice": "go"}).status == 200, "approve rollback")
    ctx.expect(node.call("POST", rb_node + "/claim", body=claim).status == 200, "claim rollback")
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE action_approvals SET request=json_set(request,'$.claim_until',?) WHERE id=?",
                   ("2000-01-01T00:00:00Z", rollback.json["id"]))
    blocked = hosted.call("POST", path + "/install-release", body={})
    ctx.expect(blocked.status == 200, "expired claim does not block another request", blocked)
    expired = local.call("GET", rb_client)
    ctx.expect(expired.json["outcome"]["status"] == "interrupted", "stale claim is interrupted", expired)
    ctx.expect(expired.json["actions"] == {"decide": False, "answer": False, "clear": True},
               "owner can only clear an interrupted claim", expired)
    ctx.expect(node.call("POST", rb_node + "/claim", body=claim).status == 409, "expired claim cannot restart")
    cleared = local.call("POST", rb_client + "/clear", body={})
    ctx.expect(cleared.status == 200 and cleared.json["outcome"]["cleared"] is True,
               "owner can clear an interrupted claim", cleared)

    trial = blocked.json["id"]
    trial_client = "action-approvals/" + trial
    trial_node = "release-requests/" + trial
    built = node.call("POST", trial_node + "/inspect", body={
        **pin, "commit_provenance": "local_main", "backup": "local_and_s3",
    })
    ctx.expect(built.status == 200 and built.json["commit_provenance"] == "Pinned from local main and built by node-bundle"
               and built.json["backup"] == "Local and configured off-machine backup", "pin second trial", built)
    ctx.expect(local.call("POST", trial_client + "/answer", body={"choice": "go"}).status == 200, "approve second trial")
    ctx.expect(node.call("POST", trial_node + "/claim", body=claim).status == 200, "claim second trial")
    checks = [{"name": "services", "ok": False, "reason": "Director unavailable"},
              {"name": "fixture-agent", "ok": False, "reason": "agent timed out"}]
    stopped = node.call("POST", trial_node + "/outcome", body={
        **claim, "status": "rollback_failed", "release": "release-one", "waiting_jobs": 3,
        "smoke": {"ok": False, "checks": checks},
    })
    ctx.expect(stopped.status == 200, "record rollback timeout", stopped)
    smoke = hosted.call("GET", path + "/smoke")
    ctx.expect(smoke.json["last_smoke"]["smoke"]["checks"] == checks, "smoke keeps the failing checks", smoke)
    attention = local.call("GET", path).json["conversation"]["attention"]
    ctx.expect(attention["assignee_member_id"] == local.call("GET", "me").json["member"]["id"],
               "release attention is assigned to the owner", attention)
    ctx.expect(attention["links"]["approval"] == trial
               and attention["question"] == "failed release is live; rollback waiting on 3 jobs",
               "rollback timeout raises a linked needs-you item", attention)
    ctx.expect(attention["actions"] == {"decide": False, "answer": True, "clear": True},
               "owner can acknowledge or clear release attention", attention)
    ctx.expect(attention["options"] == [{"id": "acknowledge", "label": "Acknowledge rollback is waiting"}]
               and attention["default"] is None, "waiting rollback offers acknowledgement", attention)
    for actor in (hosted, admin):
        ctx.expect(actor.call("POST", trial_client + "/clear", body={}).status == 403, "only owner can clear")
        answer = actor.call("POST", trial_client + "/answer", body={"choice": "acknowledge"})
        ctx.expect(answer.status == 403, "only owner can answer the install needs-you item", answer)
    answer = local.call("POST", trial_client + "/answer", body={"choice": "acknowledge"})
    ctx.expect(answer.status == 200 and answer.json["outcome"]["acknowledged"], "owner can acknowledge", answer)
    acknowledged = local.call("GET", trial_client)
    ctx.expect(acknowledged.json["actions"] == dict.fromkeys(("decide", "answer", "clear"), False)
               and not local.call("GET", path).json["conversation"].get("attention"),
               "acknowledgement dismisses rollback attention", acknowledged)

    interrupted = hosted.call("POST", path + "/install-release", body={}).json["id"]
    interrupted_node = "release-requests/" + interrupted
    interrupted_client = "action-approvals/" + interrupted
    ctx.expect(node.call("POST", interrupted_node + "/inspect", body=pin).status == 200, "pin interrupted trial")
    ctx.expect(local.call("POST", interrupted_client + "/answer", body={"choice": "go"}).status == 200,
               "approve interrupted trial")
    ctx.expect(node.call("POST", interrupted_node + "/claim", body=claim).status == 200, "claim interrupted trial")
    ctx.expect(node.call("POST", interrupted_node + "/outcome", body={
        **claim, "status": "interrupted", "release": "release-one", "switched_without_smoke": True,
    }).status == 200, "report a switch interrupted before smoke")

    doc = (Path(__file__).resolve().parents[1] / "CLIENT-API.md").read_text()
    outcome_table = doc.split("### Hosted release approvals", 1)[1].split(
        "| Field | JSON type | Presence |", 1)[1].split("The complete set", 1)[0]
    documented_fields = {line.split("`")[1] for line in outcome_table.splitlines() if line.startswith("| `")}
    rows = [local.call("GET", f"action-approvals/{aid}").json["outcome"]
            for aid in (approval, rollback.json["id"], trial, interrupted)]
    returned_fields = set().union(*rows)
    ctx.expect(documented_fields == returned_fields,
               "the release outcome field table matches approval detail responses", rows)
    ctx.expect(rows[-1]["switched_without_smoke"] is True, "interrupted receipt records the missing smoke result", rows[-1])


@case("client", "policy", "foreign-records-and-device-scope", ("rust",))
def foreign_records_and_device_scope(ctx):
    local, node = owner(ctx), ctx.node2()
    token, _ = ctx.target.pair("bootstrap-owner-tablet")
    local.upload_bundle()
    ctx.expect(Client(ctx.target, token).submit(node.name).status == 201,
               "owner's paired device can use its member's uploaded bundle")
    member, viewer = (device(ctx, role) for role in ("member", "viewer"))
    issued = viewer.call("POST", "pairings", body={"name": "viewer-tablet"})
    ctx.expect(issued.status == 200, "viewer can pair own device", issued)
    paired = request(ctx.target.url, "POST", "/api/v1/pair", body={"code": issued.json["code"]})
    tablet = paired.json["credential_id"]
    inventory = viewer.call("GET", "credentials").json["credentials"]
    ctx.expect({row["id"] for row in inventory} == {viewer.credential_id, tablet}, "inventory is member-scoped")
    ctx.expect(member.call("DELETE", "credentials/" + tablet).status == 404, "another member cannot revoke tablet")
    ctx.expect(Client(ctx.target, paired.json["token"]).call("GET", "me").status == 200, "tablet survives refusal")
    ctx.expect(viewer.call("DELETE", "credentials/" + tablet).status == 200, "member can revoke own tablet")

    raw = b"foreign-only policy test blob"
    sha = hashlib.sha256(raw).hexdigest()
    # The physical blob store is shared. Seed a reference exclusively in a different org.
    obj = ctx.target.data / "objects" / sha[:2] / sha
    obj.parent.mkdir(exist_ok=True)
    obj.write_bytes(raw)
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("INSERT INTO orgs (id, name) VALUES ('foreign-policy', 'Foreign')")
        foreign_node = db.execute(
            "INSERT INTO nodes (org_id,name,token_hash,created_at) VALUES ('foreign-policy','foreign','foreign','now')"
        ).lastrowid
        db.execute(
            """INSERT INTO jobs (org_id,job_id,node_id,spec,task,trials,bundle_sha256,meta,state,created_at)
               VALUES ('foreign-policy','foreign-job',?,'spec.yaml','tasks/t',1,'foreign','{}','done','now')""",
            (foreign_node,))
        db.execute(
            """INSERT INTO job_files (org_id,job_id,path,sha256,bytes,mode)
               VALUES ('foreign-policy','foreign-job','spec.yaml',?,?,420)""",
            (sha, len(raw)))
    admin = device(ctx, "admin")
    for actor in (local, admin, member, viewer):
        for path in ("jobs/foreign-job", "jobs/foreign-job/files/spec.yaml", "objects/" + sha):
            ctx.expect(actor.call("GET", path).status == 404, "foreign ID is hidden")
        foreign = actor.call("POST", "jobs/foreign-job/cancel", body={}, key=new_key())
        unknown = actor.call("POST", "jobs/unknown-job/cancel", body={}, key=new_key())
        ctx.expect(foreign.status == unknown.status == 404 and foreign.body == unknown.body,
                   "foreign and unknown work are indistinguishable", foreign)
        for method, path in (("PATCH", "conversations/unknown"), ("POST", "intents/unknown/jobs"),
                             ("POST", "runs/unknown/decision")):
            result = actor.call(method, path, body={}, key=new_key())
            ctx.expect(result.status == 404, "unknown work is hidden for every role", result)
    def member_intents():
        with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
            return db.execute(
                "SELECT intent_id FROM intents WHERE responsible_member_id = ? ORDER BY intent_id",
                ("policy-" + member.credential_id,),
            ).fetchall()

    before = member_intents()
    refused = member.submit(node.name, files=None, from_job="foreign-job")
    ctx.expect(refused.status == 404, "foreign bundle cannot be copied", refused)
    refused = member.submit(node.name, files=[{"path": "spec.yaml", "sha256": sha, "mode": 420}])
    ctx.expect(refused.status == 404, "foreign object cannot be linked by hash", refused)
    ctx.expect(member_intents() == before, "foreign links create no work")


@case("client", "policy", "owner-corrections-and-pairing-reset", ("rust",))
def owner_corrections_and_pairing_reset(ctx):
    local, node = owner(ctx), ctx.node2()
    member, paired_owner = device(ctx, "member"), device(ctx, "owner")
    cid = created(ctx, member, node)["conversation_id"]
    turn = post(ctx, local, {"conversation_id": cid}, "Owner instruction")
    bundle = submitted(ctx, member, node, origin={
        "kind": "conversation_turn", "conversation_id": cid, "turn_id": turn["turn_id"],
    })
    iid = bundle["intent"]["intent_id"]
    mark = f"intents/{iid}/owner-messages/{turn['turn_id']}/mark"
    initial = paired_owner.call("PUT", mark, body={"mark": "instruction"})
    ctx.expect(initial.status == 200, "paired owner marks message", initial)
    refused = member.call("PUT", mark, body={"mark": "answer"})
    ctx.expect(refused.status == 403, "member cannot overwrite paired owner's mark", refused)
    ctx.expect(local.call("GET", f"intents/{iid}/work-trace").json == initial.json, "owner mark is preserved")

    request(ctx.target.url, "POST", "/api/v1/pair", body={"code": "bogus-code"})

    def failures():
        with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
            return db.execute("SELECT value FROM org_settings WHERE key = 'pairing_failures:127.0.0.1'").fetchone()

    before = failures()
    ctx.expect(before is not None, "failed exchange counted")
    ctx.expect(member.call("POST", "pairings", body={"name": "member-tablet"}).status == 200, "member issues pairing")
    ctx.expect(failures() == before, "member pairing preserves failure count")
    ctx.expect(paired_owner.call("POST", "pairings", body={"name": "owner-tablet"}).status == 200, "owner issues pairing")
    ctx.expect(failures() is None, "owner pairing resets failure count")


@case("client", "policy", "paired-device-cannot-land-via-mcp", ("rust",))
def paired_device_cannot_land_via_mcp(ctx):
    """A paired owner has run-decision authority without access to the host land tool."""
    actor = device(ctx, "owner")
    credential = ctx.target.tmp / "policy-paired-credential"
    credential.write_text(actor.token)
    repo = Path(__file__).resolve().parents[3]
    rpc = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
           "params": {"name": "land_task", "arguments": {"repo": "fixture", "lane": "fixture"}}}
    result = subprocess.run(
        [sys.executable, "-B", str(repo / "cs/scripts/cs"), "mcp",
         "--director", ctx.target.url, "--credential-file", str(credential)],
        input=json.dumps(rpc) + "\n", text=True, capture_output=True, env=ctx.target.env, timeout=15,
    )
    ctx.expect(result.returncode == 0, "MCP answers", result.stderr)
    answer = json.loads(result.stdout)["result"]
    ctx.expect(answer["isError"] and "host owner credential" in answer["content"][0]["text"],
               "paired owner stops at the capability check", answer)


@case("client", "policy", "member-invite", ("rust",))
def member_invite(ctx):
    local = owner(ctx)
    def issue(actor, **fields):
        return actor.call("POST", "invites", body=fields)

    def exchange(code, name="phone", token=None):
        return request(ctx.target.url, "POST", "/api/v1/invites/accept",
                       token=token, body={"code": code, "name": name})

    def enroll(role):
        issued = issue(local, display_name=role.title(), role=role)
        ctx.expect(issued.status == 201, "owner can invite")
        result = exchange(issued.json["code"])
        ctx.expect(result.status == 201 and result.json["role"] == role, "invite enrolls requested role")
        return Client(ctx.target, result.json["token"], result.json["credential_id"])

    local_id = local.call("GET", "me").json["member"]["id"]
    admin, viewer = enroll("admin"), enroll("viewer")
    admin_id = admin.call("GET", "me").json["member"]["id"]
    ctx.expect(issue(local, display_name="peer", role="owner").status == 422, "no owner invites")
    ctx.expect(issue(admin, display_name="peer", role="admin").status == 403, "admins cannot grant admin")
    ctx.expect(viewer.call("GET", "members").status == 403, "viewers cannot manage members")
    ctx.expect(all(m["role"] in ("member", "viewer") for m in admin.call("GET", "members").json["members"]),
               "admin inventory excludes peers and owners")
    ctx.expect(local.call("DELETE", "credentials/" + local.call("GET", "me").json["credential_id"]).status == 409,
               "host credential is protected")
    ctx.expect(local.call("PATCH", "members/" + local_id, body={"role": "admin"}, key=new_key()).status == 409,
               "last and bootstrap owner cannot be demoted")
    viewer_id = viewer.call("GET", "me").json["member"]["id"]
    promoted = local.call("PATCH", "members/" + viewer_id, body={"role": "owner"}, key=new_key())
    ctx.expect(promoted.status == 200 and viewer.call("GET", "me").json["role"] == "owner",
               "an active member can be promoted and its existing device sees the new role")
    ctx.expect(local.call("PATCH", "members/" + local_id, body={"status": "disabled"}, key=new_key()).status == 409,
               "bootstrap owner stays active even when another owner exists")
    ctx.expect(local.call("PATCH", "members/" + viewer_id, body={"role": "viewer"}, key=new_key()).status == 200,
               "an owner can demote a peer while another active owner remains")
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("INSERT INTO orgs VALUES ('foreign-invites','Foreign')")
        db.execute("INSERT INTO members VALUES ('foreign-member','foreign-invites','Foreign','member','active')")
    ctx.expect(local.call("PATCH", "members/foreign-member", body={"status": "disabled"}, key=new_key()).status == 404,
               "foreign members cannot be changed")

    issued = issue(admin, display_name="New member", role="member")
    ctx.expect(issued.status == 201, "admin can invite member")
    invite = issued.json
    target = invite["member_id"]
    ctx.expect(any(m["id"] == target and m["status"] == "pending" for m in local.call("GET", "members").json["members"]),
               "unspent invite leaves pending member")
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE invites SET expires_at='2000' WHERE id=?", (invite["id"],))
    rejected = exchange(invite["code"])
    ctx.expect(rejected.status == 403 and rejected.json["code"] == "invite_refused", "expired invites refuse")
    for response in (exchange("csi_" + "0" * 64), exchange("malformed"),
                     request(ctx.target.url, "POST", "/api/v1/invites/accept", raw=b"malformed")):
        ctx.expect((response.status, response.body) == (403, rejected.body), "invalid codes refuse identically")
    replaced = issue(admin, member_id=target).json
    ctx.expect(exchange(invite["code"]).body == rejected.body, "reissued invites refuse identically")
    cancel_path, cancel_key = "invites/" + replaced["id"], new_key()
    ctx.expect(local.call("DELETE", cancel_path).status == 428, "invite cancellation requires a key")
    ctx.expect(local.call("DELETE", cancel_path, key=cancel_key).status == 200, "cancel retains pending target")
    replay = local.call("DELETE", cancel_path, key=cancel_key)
    ctx.expect(replay.status == 200 and replay.header("idempotent-replayed") == "true", "invite cancellation replays")
    ctx.expect(exchange(replaced["code"]).body == rejected.body, "revoked invites refuse identically")
    active = issue(admin, member_id=target).json
    ctx.expect(active["code"] not in str(local.call("GET", "invites").body), "invite inventory omits secret")

    for name in ("bad name", ".phone", "a" * 101):
        bad_name = exchange(active["code"], name=name)
        ctx.expect((bad_name.status, bad_name.body) == (403, rejected.body),
                   "bad device name does not reveal a live code")
    hosted = lead(ctx, local, created(ctx, local, ctx.node2()))
    lead_exchange = exchange(active["code"], token=hosted.token)
    ctx.expect(lead_exchange.status == 403 and lead_exchange.json["code"] == "owner_only",
               "lead cannot exchange an invite")
    wrong_pairing = request(ctx.target.url, "POST", "/api/v1/pair", body={"code": "invalid"})
    ctx.expect(wrong_pairing.status == 403, "bad pairing code refuses")

    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("""CREATE TRIGGER refuse_device BEFORE INSERT ON credentials
                      WHEN NEW.kind='device' BEGIN SELECT RAISE(ABORT,'fixture'); END""")
    ctx.expect(exchange(active["code"]).status == 500, "a failed device insert rolls back acceptance")
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        ctx.expect(db.execute("SELECT status FROM members WHERE id=?", (target,)).fetchone() == ("pending",),
                   "failed enrollment leaves member pending")
        db.execute("DROP TRIGGER refuse_device")

    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(exchange, [active["code"]] * 2))
    ctx.expect(sorted(result.status for result in results) == [201, 403], "concurrent accepts mint once")
    paired = next(result.json for result in results if result.status == 201)
    ctx.expect(next(result.body for result in results if result.status == 403) == rejected.body,
               "consumed invites refuse identically")
    member = Client(ctx.target, paired["token"], paired["credential_id"])
    ctx.expect(member.call("GET", "members").status == 403, "members cannot manage members")
    ctx.expect(issue(local, member_id=target).status == 409, "accepted invites cannot be reissued")
    ctx.expect(member.call("DELETE", "credentials/" + member.credential_id).status == 409, "self-revoke refuses")
    ctx.expect(admin.call("POST", "pairings", body={"name": "host", "member_id": local_id}).status == 403,
               "admin cannot target owner")
    ctx.expect(admin.call("PATCH", "members/" + target, body={"role": "admin"}, key=new_key()).status == 403,
               "admin cannot promote to peer")
    recovery = admin.call("POST", "pairings", body={"name": "recovery", "member_id": target})
    ctx.expect(recovery.status == 200 and recovery.json["member_id"] == target, "admin can target member")
    pair = lambda code: request(ctx.target.url, "POST", "/api/v1/pair", body={"code": code})
    orphan = issue(admin, display_name="Outstanding", role="viewer").json
    ctx.expect(local.call("PATCH", "members/" + admin_id, body={"role": "member"}, key=new_key()).status == 200,
               "owner can demote an admin")
    ctx.expect(pair(recovery.json["code"]).status == 403, "exchange rechecks issuer authority")
    ctx.expect(exchange(orphan["code"]).body == rejected.body, "invite exchange rechecks issuer authority")
    ctx.expect(local.call("PATCH", "members/" + admin_id, body={"status": "disabled"}, key=new_key()).status == 200,
               "owner can disable a former admin")
    ctx.expect(any(i["id"] == orphan["id"] and i["status"] == "revoked"
                   for i in local.call("GET", "invites").json["invites"]),
               "disable permanently voids outstanding invites from the issuer")
    recovery = local.call("POST", "pairings", body={"name": "recovery", "member_id": target})
    ctx.expect(pair(recovery.json["code"]).json["member"]["id"] == target, "recovery binds target")
    credentials = member.call("GET", "credentials").json
    ctx.expect(len(credentials["credentials"]) == 2 and all(c["member_id"] == target for c in credentials["credentials"]),
               "own-device list includes only own devices")
    ctx.expect(all(s not in json.dumps(credentials) for s in (active["code"], recovery.json["code"], member.token)),
               "credential inventory omits secrets")

    with closing(member.stream()) as stream:
        ctx.expect(stream.next()[0] == "hello", "member stream opens")
        key = new_key()
        disabled = local.call("PATCH", "members/" + target, body={"status": "disabled"}, key=key)
        ctx.expect(disabled.status == 200 and disabled.json["member"]["status"] == "disabled", "disable commits")
        replay = local.call("PATCH", "members/" + target, body={"status": "disabled"}, key=key)
        ctx.expect(replay.body == disabled.body and replay.header("idempotent-replayed") == "true",
                   "lifecycle PATCH replays")
        changed, _ = stream.until(lambda name, data: name == "change" and data["kind"] == "authorization_changed")
        ctx.expect(changed is not None and active["code"] not in str(changed) and member.token not in str(changed),
                   "authorization event omits secrets")
        ctx.expect(stream.next()[0] == "revoked" and stream.next() is None and stream.ended,
                   "disable closes the stream")
    ctx.expect(member.call("GET", "me").status == 401, "disabled credential refuses")
    ctx.expect(local.call("PATCH", "members/" + target, body={"status": "active"}, key=new_key()).status == 200,
               "manager can reactivate")
    ctx.expect(member.call("GET", "me").status == 401, "reactivation leaves old credentials revoked")

    log = (ctx.target.tmp / "server.log").read_bytes()
    for code in (invite["code"], replaced["code"], active["code"], orphan["code"], recovery.json["code"]):
        ctx.expect(code.encode() not in log, "Director log omits enrollment secrets")


@case("client", "policy", "member-device-revocation", ("rust",))
def member_device_revocation(ctx):
    local, node = owner(ctx), ctx.node2()
    actors = {role: device(ctx, role) for role in ("viewer", "member", "admin", "owner")}
    admin, member = actors["admin"], actors["member"]
    peer = device(ctx, "admin")
    secrets = []

    def pairing(actor, name, target=None):
        body = {"name": name}
        if target is not None:
            body["member_id"] = target
        answer = actor.call("POST", "pairings", body=body)
        ctx.expect(answer.status == 200, "pairing issued")
        code = answer.json["code"]
        secrets.append(code)
        return code

    def exchange(code):
        return request(ctx.target.url, "POST", "/api/v1/pair", body={"code": code})

    def paired(code):
        answer = exchange(code)
        ctx.expect(answer.status == 200, "pairing exchanged")
        return Client(ctx.target, answer.json["token"], answer.json["credential_id"])

    for role, actor in actors.items():
        second = paired(pairing(actor, role + "-second"))
        voided = pairing(second, role + "-voided")
        unaffected = pairing(actor, role + "-unaffected")
        ctx.expect(actor.call("DELETE", "credentials/" + second.credential_id).status == 200,
                   f"{role} can revoke another own device")
        ctx.expect(second.call("GET", "me").status == 401, "revoked device cannot authenticate")
        ctx.expect(exchange(unaffected).status == 200, "another device's pairing is still live")
        ctx.expect(exchange(voided).status == 403, "revocation voids the device's outstanding pairing")

    recovery = pairing(local, "owner-recovery", actors["owner"].call("GET", "me").json["member"]["id"])
    ctx.expect(exchange(recovery).json["member"] == actors["owner"].call("GET", "me").json["member"],
               "owner can recover another owner's device")
    second_member = paired(pairing(member, "member-managed"))
    hosted = lead(ctx, member, created(ctx, member, node))
    inventory = admin.call("GET", "credentials").json
    ids = {row["id"] for row in inventory["credentials"]}
    ctx.expect({admin.credential_id, member.credential_id, actors["viewer"].credential_id,
                second_member.credential_id, hosted.credential_id} <= ids,
               "admin sees own devices and member and viewer credentials")
    ctx.expect(ids.isdisjoint({peer.credential_id, local.credential_id, actors["owner"].credential_id}),
               "admin inventory excludes peer and owner credentials")
    for actor in (peer, actors["owner"]):
        ctx.expect(admin.call("DELETE", "credentials/" + actor.credential_id).status == 404,
                   "admin cannot revoke a peer or owner device")
    ctx.expect(admin.call("DELETE", "credentials/" + second_member.credential_id).status == 200,
               "admin can revoke a member device")
    ctx.expect(member.call("DELETE", "credentials/" + hosted.credential_id).status == 200,
               "member can revoke a lead they minted")
    ctx.expect(hosted.call("GET", "me").status == 401, "revoked lead cannot authenticate")
    log = (ctx.target.tmp / "server.log").read_bytes()
    ctx.expect(all(code.encode() not in log for code in secrets), "pairing secrets absent from Director log")


@case("client", "policy", "lead-adoption", ("rust",))
def lead_adoption(ctx):
    local, node = owner(ctx), ctx.node2()
    member, admin, viewer = (device(ctx, role) for role in ("member", "admin", "viewer"))
    member_id = member.call("GET", "me").json["member"]["id"]
    ctx.expect(local.call("PUT", "lead-node", body={"node": node.name}).status == 200, "set lead home")
    made = member.call("POST", "conversations", body={"spec": "hosted-lead", "model": "claude-code-sonnet"},
                       key=new_key())
    ctx.expect(made.status == 201, "member creates a hosted conversation", made)
    conversation = made.json["conversation"]
    cid = conversation["conversation_id"]
    path = "conversations/" + cid
    manual = lead(ctx, admin, conversation)
    ctx.expect(manual.call("GET", "me").json["member"]["id"] == member_id, "manual mint inherits member sponsor")
    ctx.expect(ctx.node2("wrong-rotation-node").call("POST", path + "/lead-credential").status == 403,
               "a different node cannot rotate the credential")
    rotation = node.call("POST", path + "/lead-credential")
    ctx.expect(rotation.status == 201, "bound node rotates", rotation)
    old = Client(ctx.target, rotation.json["token"], rotation.json["credential_id"])
    ctx.expect(manual.call("GET", "me").status == 401, "rotation revokes the previous credential")
    ctx.expect(old.call("GET", "me").json["member"]["id"] == member_id, "rotation inherits sponsor")
    for actor, allowed in ((local, True), (admin, True), (member, False), (viewer, False), (old, False)):
        ctx.expect(("approve_action" in actor.call("GET", "me").json["capabilities"]) == allowed,
                   "human approval capability")
    for actor, allowed in ((local, True), (admin, True), (member, True), (viewer, False), (old, False)):
        ctx.expect(("sponsor_lead" in actor.call("GET", "me").json["capabilities"]) == allowed,
                   "sponsorship capability")
    ctx.expect(old.call("POST", "action-approvals/unknown/decide", body={"decision": "approve"}).status == 403,
               "lead cannot claim a human decision")
    first = post(ctx, member, conversation, "Already offered")
    queued = post(ctx, member, conversation, "Keep queued")
    accept(ctx, node, first["job_id"])
    attention = local.call("PUT", path + "/attention", body={"question": "Decide the run"})
    ctx.expect(attention.json["conversation"]["attention"]["assignee_member_id"] == member_id,
               "operational attention routes to the sponsor")
    demoted = local.call("PATCH", "members/" + member_id, body={"role": "viewer"}, key=new_key())
    ctx.expect(demoted.status == 200, "demote sponsor", demoted)
    ctx.expect(old.call("GET", "me").json["capabilities"] == ["read_work"], "lead reloads sponsor role")
    ctx.expect(node.call("POST", path + "/lead-credential").status == 403, "ineligible sponsor blocks rotation")
    ctx.expect(local.call("POST", "credentials/lead", body={"name": "refused", "conversation_id": cid}).status == 403,
               "ineligible sponsor also blocks owner mint")
    finish(ctx, node, first["job_id"])
    ctx.expect(turn(local, queued["turn_id"])["state"] == "queued", "settlement leaves queued work paused")
    sponsor = local.call("GET", "me").json["member"]["id"]
    current = local.call("GET", path).json["conversation"]
    ctx.expect(current["paused_reason"] == "sponsor_ineligible", "conversation projects sponsor suspension")
    ctx.expect(current["attention"]["assignee_member_id"] == sponsor, "operational attention reroutes to owner")
    for operation, payload in (("land", {"repo": "fixture", "lane": "fixture"}),
                               ("install-release", {}), ("rollback", {})):
        outcome = local.call("POST", path + "/" + operation, body=payload)
        ctx.expect(outcome.status == 409 and outcome.json["code"] == "sponsor_ineligible",
                   "host action explains sponsor suspension", outcome)
    adopted = local.call("PATCH", path, body={"sponsor_member_id": sponsor}, key=new_key())
    ctx.expect(adopted.status == 200, "owner adopts hosted conversation", adopted)
    ctx.expect(adopted.json["conversation"]["paused_reason"] is None, "conversation clears sponsor suspension")
    ctx.expect(adopted.json["conversation"]["responsible_member_id"] == member_id, "adoption preserves work history")
    ctx.expect(old.call("GET", "me").status == 401, "adoption revokes the old lead")
    ctx.expect(turn(local, queued["turn_id"])["state"] == "running", "adoption resumes queued work")
    ctx.expect(admin.call("PATCH", path, body={"sponsor_member_id": admin.call("GET", "me").json["member"]["id"]},
                          key=new_key()).status == 403, "admin cannot adopt from an owner")
    replacement = lead(ctx, local, conversation)
    ctx.expect(replacement.call("GET", "me").json["member"]["id"] == sponsor,
               "new lead inherits the adopted sponsor")


@case("client", "policy", "adoption-transfers-open-attention", ("rust",))
def adoption_transfers_attention(ctx):
    local, node = owner(ctx), ctx.node2()
    member, admin = device(ctx, "member"), device(ctx, "admin")
    admin_id = admin.call("GET", "me").json["member"]["id"]
    conversation = created(ctx, member, node)
    path = "conversations/" + conversation["conversation_id"]
    anchor = post(ctx, member, conversation, "Review the setup")
    origin = {"kind": "conversation_turn", "conversation_id": conversation["conversation_id"],
              "turn_id": anchor["turn_id"]}
    work = submitted(ctx, member, node, origin=origin)
    job = work["jobs"][0]["job_id"]
    accept(ctx, node, job)
    sent = node.call("POST", f"jobs/{job}/action-approvals", body={
        "run_id": "adoption-assignment", "argv": ["/usr/bin/curl", "https://example.test"],
        "context": {"home": "/home/alice", "workspace": "/home/alice/work"},
        "needs": ["network"], "requesting_model": "fixture", "reason": "fetch documentation",
    })
    ctx.expect(sent.status == 200, "create approval", sent)
    approval = sent.json["id"]
    ctx.expect(member.call("PUT", path + "/attention", body={"question": "Which option?", "job_id": job}).status == 200,
               "raise open conversation and task attention")
    adopted = admin.call("PATCH", path, body={"sponsor_member_id": admin_id}, key=new_key())
    ctx.expect(adopted.status == 200, "admin adopts the member's conversation", adopted)
    row = admin.call("GET", path).json["conversation"]
    ctx.expect(row["attention"]["assignee_member_id"] == admin_id and not row["actions"]["adopt"],
               "new sponsor sees the question and cannot adopt their own conversation", row)
    ctx.expect(local.call("GET", "action-approvals/" + approval).json["assignee_member_id"] == admin_id,
               "open approval transfers to eligible sponsor")
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        moved = db.execute("SELECT record_kind FROM assignment_changes WHERE assignee_member_id=?",
                           (admin_id,)).fetchall()
    ctx.expect(set(moved) == {("action_approvals",), ("conversations",), ("task_needs_you",)},
               "all open attention transfers are audited", moved)


@case("client", "policy", "plain-conversation-adoption", ("rust",))
def plain_conversation_adoption(ctx):
    local, member, admin, peer = owner(ctx), device(ctx, "member"), device(ctx, "admin"), device(ctx, "admin")
    node = ctx.node2()
    member_id = member.call("GET", "me").json["member"]["id"]
    owner_id = local.call("GET", "me").json["member"]["id"]
    admin_id = admin.call("GET", "me").json["member"]["id"]
    peer_id = peer.call("GET", "me").json["member"]["id"]
    conversation = created(ctx, member, node)
    path = "conversations/" + conversation["conversation_id"]
    ctx.expect(local.call("PATCH", "members/" + member_id, body={"role": "viewer"},
                          key=new_key()).status == 200, "demote conversation sponsor")
    queued = post(ctx, local, conversation, "Resume after adoption")
    ctx.expect(turn(local, queued["turn_id"])["state"] == "queued", "suspended conversation queues owner work")
    ctx.expect(local.call("PATCH", path, body={"sponsor_member_id": member_id}, key=new_key()).status == 403,
               "owner cannot assign an ineligible sponsor")
    ctx.expect(admin.call("PATCH", path, body={"sponsor_member_id": owner_id}, key=new_key()).status == 403,
               "admin cannot assign to an owner")
    adopted = admin.call("PATCH", path, body={"sponsor_member_id": admin_id}, key=new_key())
    ctx.expect(adopted.status == 200, "admin adopts from an ineligible member", adopted)
    ctx.expect(peer.call("PATCH", path, body={"sponsor_member_id": peer_id}, key=new_key()).status == 403,
               "admin cannot adopt from another admin")
    adopted = local.call("PATCH", path, body={"sponsor_member_id": owner_id}, key=new_key())
    ctx.expect(adopted.status == 200, "owner can adopt a plain conversation", adopted)
    ctx.expect(adopted.json["conversation"]["sponsor_member_id"] == owner_id,
               "plain conversation has the new sponsor", adopted)
    ctx.expect(turn(local, queued["turn_id"])["state"] == "running", "adoption resumes owner work")


@case("client", "policy", "sponsor-recovery", ("rust",))
def sponsor_recovery(ctx):
    local, member, node = owner(ctx), device(ctx, "member"), ctx.node2()
    member_id = member.call("GET", "me").json["member"]["id"]
    member_path = "members/" + member_id
    conversation = created(ctx, member, node)
    path = "conversations/" + conversation["conversation_id"]
    ctx.expect(local.call("PATCH", member_path, body={"role": "viewer"}, key=new_key()).status == 200,
               "demote the sponsor")
    queued = post(ctx, local, conversation, "Resume after promotion")
    ctx.expect(turn(local, queued["turn_id"])["state"] == "queued", "turn waits during suspension")
    ctx.expect(local.call("PATCH", member_path, body={"role": "member"}, key=new_key()).status == 200,
               "restore sponsor eligibility")
    ctx.expect(turn(local, queued["turn_id"])["state"] == "running", "promotion resumes queued turn")

    jid = turn(local, queued["turn_id"])["job_id"]
    accept(ctx, node, jid)
    finish(ctx, node, jid)
    ctx.expect(local.call("PUT", path + "/budget", body={"usd": 0}).status == 200, "exhaust turn budget")
    waiting = post(ctx, member, conversation, "Resume after adoption")
    jid = waiting["job_id"]
    ctx.expect(local.call("GET", "jobs/" + jid).json["budget"]["reason"] == "budget_exhausted",
               "the turn job is waiting for budget")
    reactivation_chat = created(ctx, member, node)
    ctx.expect(local.call("PATCH", member_path, body={"status": "disabled"}, key=new_key()).status == 200,
               "disable the sponsor of a waiting job")
    reactivation_turn = post(ctx, local, reactivation_chat, "Resume after reactivation")
    ctx.expect(turn(local, reactivation_turn["turn_id"])["state"] == "queued",
               "disabled sponsor suspends queued turns")
    ctx.expect(local.call("PUT", path + "/budget", body={"usd": 2}).status == 200, "run the release sweep")
    job = local.call("GET", "jobs/" + jid).json
    ctx.expect(job["state"] == "waiting" and job["budget"]["reason"] == "sponsor_ineligible",
               "sponsorship keeps the job waiting with a reason", job)
    key = new_key()
    owner_id = local.call("GET", "me").json["member"]["id"]
    adoption = local.call("PATCH", path, body={"sponsor_member_id": owner_id}, key=key)
    ctx.expect(adoption.status == 200, "adopt the conversation", adoption)
    job = local.call("GET", "jobs/" + jid).json
    ctx.expect(job["state"] == "offered" and job["budget"]["reason"] is None,
               "adoption offers the waiting job", job)
    ctx.expect(local.call("PATCH", member_path, body={"status": "active"}, key=new_key()).status == 200,
               "reactivate the former sponsor")
    ctx.expect(turn(local, reactivation_turn["turn_id"])["state"] == "running",
               "reactivation resumes queued turns")
    target = {"sponsor_member_id": member_id}
    key = new_key()
    ctx.expect(local.call("PATCH", path, body=target, key=key).status == 200,
               "adopt to a reactivated sponsor")
    ctx.expect(local.call("PATCH", member_path, body={"status": "disabled"}, key=new_key()).status == 200,
               "disable the new sponsor before retrying")
    ctx.expect(local.call("PATCH", path, body=target, key=key).status == 200,
               "authorized adoption retry succeeds after the destination becomes ineligible")


@case("client", "policy", "lead-approval-status-only", ("rust",))
def lead_approval_status_only(ctx):
    local, admin, node = owner(ctx), device(ctx, "admin"), ctx.node2()
    sponsor_id = admin.call("GET", "me").json["member"]["id"]
    conversation = created(ctx, admin, node)
    credential = lead(ctx, admin, conversation)
    job = post(ctx, admin, conversation, "Request approval")["job_id"]
    accept(ctx, node, job)
    sent = node.call("POST", f"jobs/{job}/action-approvals", body={
        "run_id": "sponsor-approval", "argv": ["/usr/bin/curl", "https://example.test"],
                "context": {"home": "/home/alice", "workspace": "/home/alice/work"},
        "needs": ["network"], "requesting_model": "requester", "reason": "publish work",
    })
    ctx.expect(sent.status == 200, "submit a real approval", sent)
    aid = sent.json["id"]
    path = "action-approvals/" + aid
    for _ in range(80):
        row = local.call("GET", path).json
        if row["status"] == "needs_owner":
            break
        time.sleep(0.025)
    ctx.expect(row["status"] == "needs_owner" and row["assignee_member_id"] == sponsor_id,
               "approval is assigned to the lead sponsor", row)
    cid = conversation["conversation_id"]
    response = credential.call("GET", f"action-approvals?conversation={cid}")
    listed = response.json["action_approvals"]
    ctx.expect(response.status == 200 and [row["id"] for row in listed] == [aid],
               "lead lists its conversation approval", response)
    ctx.expect(set(listed[0]) <= {"id", "job_id", "run_id", "conversation_id", "status", "revision",
                                 "verdict", "outcome", "kind", "commit", "base", "target_run_id", "actions"},
               "lead sees only approval status fields", listed)
    ctx.expect(credential.call("POST", path + "/decide", body={"decision": "deny"}).status == 403,
               "lead cannot decide an approval assigned to its sponsor")
    decision = local.call("POST", path + "/decide", body={"decision": "deny"})
    owner_credential = local.call("GET", "me").json["credential_id"]
    ctx.expect(decision.status == 200 and decision.json["decided_by"] == owner_credential,
               "owner can decide with their own credential recorded", decision)


@case("client", "policy", "schedule-sponsor-suspension", ("rust",))
def schedule_sponsor_suspension(ctx):
    local, node = owner(ctx), ctx.node2()
    admin = device(ctx, "admin")
    admin_id = admin.call("GET", "me").json["member"]["id"]
    owner_id = local.call("GET", "me").json["member"]["id"]
    schedule = create_schedule(ctx, node, admin)
    path = "schedules/" + schedule["schedule_key"]
    peer = device(ctx, "admin")
    peer_id = peer.call("GET", "me").json["member"]["id"]
    ctx.expect(peer.call("PATCH", path, body={"responsible_member_id": peer_id}, key=new_key()).status == 403,
               "admin cannot take over another admin's schedule")
    command = ("job", "create", "--node", node.name, "--bundle", str(ctx.target.bundle),
               "--meta", "schedule_id=" + schedule["schedule_id"])
    first = ctx.target.admin(*command)["job_id"]
    accept(ctx, node, first)
    sent = node.call("POST", f"jobs/{first}/action-approvals", body={
        "run_id": "schedule-transfer", "argv": ["/usr/bin/curl", "https://example.test"],
        "context": {"home": "/home/alice", "workspace": "/home/alice/work"},
        "needs": ["network"], "requesting_model": "fixture", "reason": "fetch documentation",
    })
    ctx.expect(sent.status == 200, "scheduled job creates an approval", sent)
    aid = sent.json["id"]
    for sponsor in (peer_id, admin_id):
        result = local.call("PATCH", path, body={"responsible_member_id": sponsor}, key=new_key())
        ctx.expect(result.status == 200, "schedule reassigned", result)
        row = local.call("GET", "action-approvals/" + aid).json
        ctx.expect(row["assignee_member_id"] == sponsor, "open scheduled approval follows sponsorship", row)
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        audited = db.execute("SELECT count(*) FROM assignment_changes WHERE record_id=?", (aid,)).fetchone()
        ctx.expect(audited == (2,), "both scheduled transfers were audited", audited)
    disabled = local.call("PATCH", "members/" + admin_id, body={"status": "disabled"}, key=new_key())
    ctx.expect(disabled.status == 200, "disable the schedule's responsible member", disabled)
    rows = local.call("GET", "schedules").json["schedules"]
    paused = next(row for row in rows if row["schedule_key"] == schedule["schedule_key"])
    ctx.expect(paused["paused_reason"] == "sponsor_ineligible", "schedule projects sponsor suspension")
    try:
        ctx.target.admin(*command)
    except RuntimeError as error:
        ctx.expect("cannot sponsor new work" in str(error), "disabled member suspends new firings")
    else:
        ctx.expect(False, "disabled member's firing was offered")
    finish(ctx, node, first)
    member = device(ctx, "member")
    member_id = member.call("GET", "me").json["member"]["id"]
    target, key = {"responsible_member_id": member_id}, new_key()
    ctx.expect(local.call("PATCH", path, body=target, key=key).status == 200, "reassign to eligible member")
    ctx.expect(local.call("PATCH", "members/" + member_id, body={"status": "disabled"},
                          key=new_key()).status == 200, "disable new schedule sponsor")
    ctx.expect(local.call("PATCH", path, body=target, key=key).status == 200,
               "authorized reassignment retry succeeds after the destination becomes ineligible")
    reassigned = local.call("PATCH", path, body={"responsible_member_id": owner_id}, key=new_key())
    ctx.expect(reassigned.status == 200 and reassigned.json["schedule"]["responsible_member_id"] == owner_id,
               "owner reassigns schedule", reassigned)
    ctx.expect(reassigned.json["schedule"]["paused_reason"] is None, "schedule clears sponsor suspension")
    ctx.expect(peer.call("PATCH", path, body={"responsible_member_id": peer_id}, key=new_key()).status == 403,
               "admin cannot take over an owner's schedule")
    ctx.expect(ctx.target.admin(*command)["job_id"] != first, "explicit reassignment resumes firings")


@case("client", "policy", "heartbeat-sponsor-suspension", ("rust",))
def heartbeat_sponsor_suspension(ctx):
    local, node = owner(ctx), ctx.node2()
    member, admin = device(ctx, "member"), device(ctx, "admin")
    member_id = member.call("GET", "me").json["member"]["id"]
    admin_id = admin.call("GET", "me").json["member"]["id"]
    project_id = project(ctx, local, name="Sponsor heartbeat")["project_id"]
    ctx.expect(local.call("PUT", "lead-node", body={"node": node.name}).status == 200, "set lead home")
    response = member.call("POST", "conversations", body={
        "spec": "hosted-lead", "model": "claude-code-sonnet", "project_id": project_id,
    }, key=new_key())
    ctx.expect(response.status == 201, "create hosted conversation", response)
    conversation = response.json["conversation"]
    cid = conversation["conversation_id"]
    path = "conversations/" + cid
    minted = node.call("POST", path + "/lead-credential")
    ctx.expect(minted.status == 201, "bound node mints for the member", minted)
    old = Client(ctx.target, minted.json["token"], minted.json["credential_id"])
    ctx.expect(admin.call("PATCH", "members/" + member_id, body={"status": "disabled"}, key=new_key()).status == 200,
               "admin disables the old sponsor")
    ctx.expect(old.call("GET", "me").status == 401, "disabled sponsor's credential is revoked")
    ctx.expect(node.call("POST", path + "/lead-credential").status == 403, "rotation pauses while disabled")
    heartbeat = "projects/" + project_id + "/heartbeat"
    config = {"interval_seconds": 1, "conversation_id": cid}
    ctx.expect(local.call("PUT", heartbeat, body=config).status == 200, "enable a due heartbeat")
    time.sleep(2.2)
    ctx.expect(thread(local, conversation)["turns"] == [], "a disabled sponsor cannot trigger a heartbeat")
    adopted = admin.call("PATCH", path, body={"sponsor_member_id": admin_id}, key=new_key())
    ctx.expect(adopted.status == 200, "admin adopts from a disabled member", adopted)
    for _ in range(50):
        turns = thread(local, conversation)["turns"]
        if turns:
            break
        time.sleep(0.1)
    ctx.expect(len(turns) == 1 and turns[0]["origin"] == "heartbeat", "adoption resumes the heartbeat", turns)
    ctx.expect(local.call("PUT", heartbeat, body={"interval_seconds": None}).status == 200, "stop the test heartbeat")
    rotated = node.call("POST", path + "/lead-credential")
    ctx.expect(rotated.status == 201, "the node can rotate after adoption", rotated)
    new = Client(ctx.target, rotated.json["token"], rotated.json["credential_id"])
    ctx.expect(new.call("GET", "me").json["member"]["id"] == admin_id, "rotation inherits the new sponsor")


@case("client", "policy", "disable-side-effects", ("rust",))
def disable_side_effects(ctx):
    local, node = owner(ctx), ctx.node2()
    admin = device(ctx, "admin")
    target = admin.call("GET", "me").json["member"]["id"]
    bootstrap = local.call("GET", "me").json["member"]["id"]
    conversation = created(ctx, admin, node)
    hosted = lead(ctx, admin, conversation)
    path = "conversations/" + conversation["conversation_id"]
    pairing = admin.call("POST", "pairings", body={"name": "outstanding"}).json["code"]
    job = submitted(ctx, admin, node)["jobs"][0]["job_id"]
    accept(ctx, node, job)
    approval = node.call("POST", f"jobs/{job}/action-approvals", body={
        "run_id": "disable-action", "argv": ["/usr/bin/curl", "https://example.test"],
                "context": {"home": "/home/alice", "workspace": "/home/alice/work"}, "needs": ["network"],
        "requesting_model": "requester", "reason": "publish work",
    })
    ctx.expect(approval.status == 200, "node creates approval", approval)
    attention = admin.call("PUT", path + "/attention", body={"question": "Review the job", "job_id": job})
    ctx.expect(attention.json["conversation"]["attention"]["assignee_member_id"] == target,
               "conversation attention starts assigned to admin")
    approval_id = approval.json["id"]
    db_path = ctx.target.data / "director.sqlite"
    with sqlite3.connect(db_path) as db:
        org, = db.execute("SELECT org_id FROM members WHERE id=?", (target,)).fetchone()
        db.execute("INSERT INTO ui_codes (org_id,member_id,code_hash,expires_at) VALUES (?,?,?,?)",
                   (org, target, "disable-ui-code", int(time.time()) + 3600))
        db.execute("INSERT INTO ui_sessions (org_id,member_id,token_hash,expires_at,id) VALUES (?,?,?,?,?)",
                   (org, target, "disable-ui-token", int(time.time()) + 3600, "disable-ui-session"))
        node_id, = db.execute("SELECT id FROM nodes WHERE org_id=? AND name=?", (org, node.name)).fetchone()
        db.executemany(
            """INSERT INTO action_approvals (org_id,id,conversation_id,node_id,request,created_at,assigned_member_id)
               VALUES (?,?,?,?,json_object('kind',?),'now',?)""",
            [(org, "disable-" + kind, conversation["conversation_id"], node_id, kind, target)
             for kind in ("install", "rollback")],
        )
        ctx.expect(db.execute("SELECT assigned_member_id FROM action_approvals WHERE id=?",
                              (approval_id,)).fetchone() == (target,), "approval starts assigned to admin")
    answer = local.call("PATCH", "members/" + target, body={"status": "disabled"}, key=new_key())
    ctx.expect(answer.status == 200, "owner disables admin")
    ctx.expect(hosted.call("GET", "me").status == 401, "disable invalidates sponsored lead")
    ctx.expect(local.call("POST", "credentials/lead",
                          body={"name": "refused", "conversation_id": conversation["conversation_id"]}).status == 403,
               "disabled sponsor blocks a new credential")
    ctx.expect(local.call("GET", path).json["conversation"]["attention"]["assignee_member_id"] == bootstrap,
               "disable reroutes conversation attention")
    exchanged = request(ctx.target.url, "POST", "/api/v1/pair", body={"code": pairing})
    ctx.expect(exchanged.status == 403, "disable voids outstanding pairing")
    with sqlite3.connect(db_path) as db:
        ctx.expect(db.execute("SELECT revoked_at FROM credentials WHERE id=?",
                              (hosted.credential_id,)).fetchone()[0] is not None, "lead revocation is durable")
        ctx.expect(db.execute("SELECT expires_at FROM pairings WHERE issued_by=? AND name='outstanding'",
                              (admin.credential_id,)).fetchone()[0] <= time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                   "pairing expiry is durable")
        for table in ("ui_codes", "ui_sessions"):
            ctx.expect(db.execute(f"SELECT count(*) FROM {table} WHERE member_id=?", (target,)).fetchone() == (0,),
                       "disable removes " + table)
        ctx.expect(db.execute("SELECT assigned_member_id FROM action_approvals WHERE id=?",
                              (approval_id,)).fetchone() == (bootstrap,), "approval reroutes to bootstrap owner")
        for kind in ("install", "rollback"):
            ctx.expect(db.execute("SELECT assigned_member_id FROM action_approvals WHERE id=?",
                                  ("disable-" + kind,)).fetchone() == (bootstrap,),
                       kind + " approval reroutes to the owner")
        ctx.expect(db.execute("SELECT assignee_member_id FROM task_needs_you WHERE job_id=? AND status='open'",
                              (job,)).fetchone() == (bootstrap,), "disable reroutes task Needs you")


@case("client", "policy", "hard-line-approval-authority", ("rust",))
def hard_line_approval_authority(ctx):
    local, node = owner(ctx), ctx.node2()
    paired_owner = device(ctx, "owner")
    admin, other, member, viewer = (device(ctx, role) for role in ("admin", "admin", "member", "viewer"))
    admin_id, member_id, viewer_id = (actor.call("GET", "me").json["member"]["id"]
                                      for actor in (admin, member, viewer))
    local.call("PUT", "lead-node", body={"node": node.name})
    conversation = admin.call("POST", "conversations", body={
        "node": node.name, "spec": "hosted-lead", "model": "claude-code-sonnet",
    }, key=new_key()).json["conversation"]
    hosted = lead(ctx, admin, conversation)
    intent = submitted(ctx, admin, node)
    job, intent_id = intent["jobs"][0]["job_id"], intent["intent"]["intent_id"]
    accept(ctx, node, job)
    member_job = submitted(ctx, member, node)["jobs"][0]["job_id"]
    accept(ctx, node, member_job)

    def ask(target, run, argv):
        sent = node.call("POST", f"jobs/{target}/action-approvals", body={
            "run_id": run, "argv": argv, "needs": ["network"], "requesting_model": "requester",
            "context": {"home": "/home/alice", "workspace": "/home/alice/work"}, "reason": "publish work",
        })
        ctx.expect(sent.status == 200, "node submits a hard-line request", sent)
        path = "action-approvals/" + sent.json["id"]
        for _ in range(80):
            row = local.call("GET", path).json
            if row["status"] == "needs_owner":
                break
            time.sleep(0.025)
        ctx.expect(row["status"] == "needs_owner" and row["verdict"] == "no_go"
                   and "reviewer_model" not in row, "hard line goes to a human without model review", row)
        return sent.json["id"], path, row

    def refused(actor, path, status):
        for verb, body in (("decide", {"decision": "approve"}), ("answer", {"choice": "go"})):
            denied = actor.call("POST", path + "/" + verb, body=body)
            ctx.expect(denied.status == status, "caller cannot " + verb + " this approval", denied)
        ctx.expect(local.call("GET", path).json["status"] == "needs_owner", "refusal leaves the request open")

    def allowed_actions(actor, path):
        row = actor.call("GET", path)
        ctx.expect(row.status == 200 and row.json["actions"]["decide"] and row.json["actions"]["answer"]
                   and (actor in (local, paired_owner) or not row.json["actions"]["clear"]),
                   "projection offers the decision", row)

    credential_path = ["/bin/cat", "/home/alice/.ssh/id_ed25519"]
    for verb, body, status, argv in (
        ("decide", {"decision": "approve"}, "approved", ["git", "push", "--force"]),
        ("answer", {"choice": "go"}, "approved", credential_path),
    ):
        aid, path, row = ask(job, "hard-line-" + verb, argv)
        ctx.expect(row["assignee_member_id"] == admin_id, "hard line is assigned to the responsible admin", row)
        if argv[0] == "git":
            ctx.expect(row["review_reason"].startswith(
                "Git push needs a human decision (an owner or the assigned admin)"),
                "reason names who can decide", row)
        for actor in (local, paired_owner, admin):
            allowed_actions(actor, path)
        trace = admin.call("GET", f"intents/{intent_id}/work-trace").json["work_trace"]
        item = next((item for item in trace["needs_you"] if item["action_approval_id"] == aid), None)
        ctx.expect(item is not None and item["actions"]["decide"] and item["actions"]["answer"],
                   "assigned admin finds the request in needs-you", trace)
        ctx.expect(other.call("GET", path).status == 404, "unassigned admin cannot read the request")
        listed = other.call("GET", "action-approvals").json["action_approvals"]
        ctx.expect(aid not in [row["id"] for row in listed], "unassigned admin list omits the request", listed)
        refused(other, path, 404)
        for actor in (member, viewer, hosted):
            read = actor.call("GET", path)
            if read.status == 200:
                ctx.expect(not any(read.json["actions"].values()), "projection withholds actions", read)
            refused(actor, path, 403)
        decided = admin.call("POST", path + "/" + verb, body=body)
        ctx.expect(decided.status == 200 and decided.json["status"] == status
                   and decided.json["decided_by"], "assigned admin may " + verb, decided)

    aid, path, row = ask(member_job, "hard-line-member", ["git", "push"])
    ctx.expect(row["assignee_member_id"] not in (member_id, admin_id), "member's hard line goes to an owner", row)
    refused(admin, path, 404)
    for actor, actor_id in ((member, member_id), (viewer, viewer_id)):
        with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
            db.execute("UPDATE action_approvals SET assigned_member_id=? WHERE id=?", (actor_id, aid))
        refused(actor, path, 403)
    decided = paired_owner.call("POST", path + "/decide", body={"decision": "deny"})
    ctx.expect(decided.status == 200 and decided.json["status"] == "denied", "any owner may decide", decided)

    for kind in ("land", "install", "rollback", "browser_action"):
        aid = "hard-line-owner-" + kind
        with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
            org, = db.execute("SELECT org_id FROM members WHERE id=?", (admin_id,)).fetchone()
            node_id, = db.execute("SELECT id FROM nodes WHERE org_id=? AND name=?", (org, node.name)).fetchone()
            db.execute(
                """INSERT INTO action_approvals (org_id,id,conversation_id,node_id,request,created_at,
                       assigned_member_id,status) VALUES (?,?,?,?,json_object('kind',?),'now',?,'needs_owner')""",
                (org, aid, conversation["conversation_id"], node_id, kind, admin_id))
        path = "action-approvals/" + aid
        ctx.expect(admin.call("GET", path).status == 404, kind + " is hidden from the assigned admin")
        refused(admin, path, 403)
        allowed_actions(local, path)
        with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
            db.execute("DELETE FROM action_approvals WHERE id=?", (aid,))


@case("client", "policy", "approval-reassignment-race", ("rust",))
def approval_reassignment_race(ctx):
    local, node = owner(ctx), ctx.node2()
    admin = device(ctx, "admin")
    admin_id = admin.call("GET", "me").json["member"]["id"]
    owner_identity = local.call("GET", "me").json
    owner_id = owner_identity["member"]["id"]
    job = submitted(ctx, admin, node)["jobs"][0]["job_id"]
    accept(ctx, node, job)
    response = node.call("POST", f"jobs/{job}/action-approvals", body={
        "run_id": "approval-reassignment-race", "argv": ["git", "push", "--force"],
        "needs": ["network"], "requesting_model": "requester", "reason": "publish work",
    })
    ctx.expect(response.status == 200, "node requests approval", response)
    path = "action-approvals/" + response.json["id"]
    for _ in range(80):
        if local.call("GET", path).json["status"] == "needs_owner":
            break
        time.sleep(0.025)
    ctx.expect(local.call("GET", path).json["status"] == "needs_owner", "request needs a human decision")
    barrier = Barrier(2)

    def decide():
        barrier.wait()
        return admin.call("POST", path + "/decide", body={"decision": "deny"})

    def demote():
        barrier.wait()
        return local.call("PATCH", "members/" + admin_id, body={"role": "member"}, key=new_key())

    with ThreadPoolExecutor(max_workers=2) as pool:
        decision, demotion = pool.submit(decide), pool.submit(demote)
        decision, demotion = decision.result(), demotion.result()
    ctx.expect(demotion.status == 200, "demotion committed", demotion)
    final = local.call("GET", path).json
    if decision.status == 200:
        ctx.expect(final["status"] == "denied", "decision that preceded the demotion stands", final)
    else:
        ctx.expect(decision.status == 403 and final["status"] == "needs_owner"
                   and final["assignee_member_id"] == owner_id, "demoted admin cannot decide", final)
        takeover = local.call("POST", path + "/decide", body={"decision": "deny"})
        ctx.expect(takeover.status == 200 and takeover.json["decided_by"] == owner_identity["credential_id"],
                   "owner resolves the rerouted request", takeover)


@case("client", "policy", "concurrent-last-owner", ("rust",))
def concurrent_last_owner(ctx):
    owners = [device(ctx, "owner"), device(ctx, "owner")]
    members = [actor.call("GET", "me").json["member"]["id"] for actor in owners]
    db_path = ctx.target.data / "director.sqlite"
    # Other cases leave active owners, including the protected bootstrap owner.
    # Hide them temporarily so the two requests contend for the last-owner check.
    with sqlite3.connect(db_path) as db:
        org, = db.execute("SELECT org_id FROM members WHERE id=?", (members[0],)).fetchone()
        existing = db.execute(
            "SELECT id FROM members WHERE org_id=? AND id NOT IN (?,?) AND role='owner' AND status='active'",
            (org, *members)).fetchall()
        db.executemany("UPDATE members SET status='disabled' WHERE id=?", existing)
    try:
        start = Barrier(2)

        def change(args):
            actor, member, body = args
            start.wait(timeout=5)
            return actor.call("PATCH", "members/" + member, body=body, key=new_key())

        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(change, [
                (owners[0], members[0], {"role": "admin"}),
                (owners[1], members[1], {"status": "disabled"}),
            ]))
        ctx.expect(sorted(result.status for result in results) == [200, 409],
                   "concurrent demote and disable preserve the last active owner")
        with sqlite3.connect(db_path) as db:
            count, = db.execute("SELECT count(*) FROM members WHERE org_id=? AND role='owner' AND status='active'",
                                (org,)).fetchone()
        ctx.expect(count == 1, "one owner remains active")
    finally:
        with sqlite3.connect(db_path) as db:
            db.executemany("UPDATE members SET status='active' WHERE id=?", existing)


@case("client", "policy", "invite-throttle", ("rust",))
def invite_throttle(ctx):
    local = owner(ctx)
    db_path = ctx.target.data / "director.sqlite"
    # Seed the existing source window near its limit to avoid relying on minute boundaries.
    bucket = hashlib.sha256(b"127.0.0.1").hexdigest()
    with sqlite3.connect(db_path) as db:
        org, = db.execute("SELECT value FROM meta WHERE key='default_org_id'").fetchone()
        db.execute("INSERT OR REPLACE INTO invite_throttle VALUES (?,?,?,30)",
                   (org, bucket, int(time.time()) // 60))
    limited = request(ctx.target.url, "POST", "/api/v1/invites/accept", raw=b"malformed")
    ctx.expect(limited.status == 429 and limited.headers.get("retry-after"), "malformed exchange is throttled")
    issued = local.call("POST", "pairings", body={"name": "throttle-control"})
    paired = request(ctx.target.url, "POST", "/api/v1/pair", body={"code": issued.json["code"]})
    ctx.expect(paired.status == 200, "invite limit leaves short-code exchange available")
    with sqlite3.connect(db_path) as db:
        db.execute("DELETE FROM invite_throttle WHERE org_id=? AND bucket=?", (org, bucket))

@case("client", "policy", "teams-bootstrap-owner-name")
def teams_bootstrap_owner_name(ctx):
    target = RustTarget(ctx.target.tmp / "named-owner", ctx.target.binary, owner_name="Ada Lovelace")
    try:
        actor = Client(target, target.local_token())
        ctx.expect(actor.call("GET", "me").json["member"]["display_name"] == "Ada Lovelace",
                   "a new installation seeds the configured owner name")
    finally:
        target.close()


@case("client", "policy", "teams-enrollment-origin-and-directory")
def teams_enrollment_origin_and_directory(ctx):
    local = owner(ctx)
    admin, member, viewer = (device(ctx, role) for role in ("admin", "member", "viewer"))
    node = ctx.node2()
    hosted = lead(ctx, member, created(ctx, member, node))
    for actor, expected, invitable in (
        (local, ["owner", "admin", "member", "viewer"], ["admin", "member", "viewer"]),
        (admin, ["member", "viewer"], ["member", "viewer"]),
        (member, [], []), (viewer, [], []), (hosted, [], []),
    ):
        me = actor.call("GET", "me").json
        ctx.expect(me["assignable_roles"] == expected and me["invitable_roles"] == invitable,
                   "role choices match the credential")
    directory = viewer.call("GET", "members/directory")
    member_id = member.call("GET", "me").json["member"]["id"]
    ctx.expect(directory.status == 200 and {"id": member_id, "display_name": "member",
               "role": "member", "active": True} in directory.json, "viewer reads directory", directory)
    ctx.expect(hosted.call("GET", "members/directory").status == 403, "lead cannot read the directory")
    viewer_id = viewer.call("GET", "me").json["member"]["id"]
    admin_id = admin.call("GET", "me").json["member"]["id"]
    owner_id = local.call("GET", "me").json["member"]["id"]
    for actor, target, name, status in (
        (viewer, viewer_id, "Renamed Viewer", 200),
        (admin, member_id, "Renamed Member", 200),
        (local, owner_id, "Renamed Owner", 200),
        (viewer, member_id, "Denied", 403),
        (admin, owner_id, "Denied", 403),
        (hosted, member_id, "Denied", 403),
    ):
        changed = actor.call("PATCH", "members/" + target, body={"display_name": name}, key=new_key())
        ctx.expect(changed.status == status, "rename follows self and hierarchy rules", changed)
        if status == 200:
            ctx.expect(changed.json["member"]["display_name"] == name, "rename is applied")
    ctx.expect(member.call("GET", "me").json["member"]["display_name"] == "Renamed Member",
               "renamed member uses the same device")
    conv = created(ctx, member, node)
    ctx.expect(conv["sponsor_display_name"] == conv["responsible_display_name"] == "Renamed Member",
               "conversation resolves sponsor and responsible names", conv)

    path = "advertised-url"
    advertised = "https://director.example.ts.net"
    header = {"X-ContextStack-Invite-Origin": "https://override.example:8780"}

    def issue(route, origin_header=None):
        body = {"display_name": "Invite Test", "role": "viewer"} if route == "invites" else {"name": "phone"}
        return local.call("POST", route, body=body, headers=origin_header)

    def check_link(route, response, expected):
        ctx.expect(response.status == (201 if route == "invites" else 200), "enrollment issued", response)
        link = urlsplit(response.json["link"])
        ctx.expect(link.scheme == "contextstack" and link.netloc == {"invites": "invite", "pairings": "pair"}[route]
                   and parse_qs(link.query) == {"url": [expected], "code": [response.json["code"]]},
                   "link carries the selected origin and the enrollment code")

    ctx.expect(admin.call("PUT", path, body={"url": advertised}).status == 403, "only owner sets origin")
    ctx.expect(local.call("PUT", path, body={"url": "file:///tmp"}).status == 422, "invalid setting refused")
    ctx.expect(local.call("PUT", path, body={"url": advertised + "/"}).json == {"url": advertised},
               "advertised URL is normalized")
    ctx.expect(local.call("GET", path).json == {"url": advertised}, "owner reads advertised URL")
    for route in ("invites", "pairings"):
        check_link(route, issue(route, header), advertised)
    ctx.expect(local.call("PUT", path, body={"url": None}).json == {"url": None}, "owner clears URL")
    for route in ("invites", "pairings"):
        check_link(route, issue(route), ctx.target.url)
        check_link(route, issue(route, header), header["X-ContextStack-Invite-Origin"])
        ctx.expect(issue(route, {"X-ContextStack-Invite-Origin": "https://a.test/path"}).status == 422,
                   "malformed loopback override refused")
    invites = local.call("GET", "invites").json["invites"]
    row = next(row for row in invites if row["display_name"] == "Invite Test")
    ctx.expect(set(row) == {"id", "member_id", "display_name", "role", "status", "expires_at", "issued_by"}
               and row["status"] == "pending", "invite inventory has the documented shape")


@case("client", "policy", "teams-record-actions")
def teams_record_actions(ctx):
    local, node = owner(ctx), ctx.node2()
    admin, member, viewer, other = (device(ctx, r) for r in ("admin", "member", "viewer", "member"))
    admin_id = admin.call("GET", "me").json["member"]["id"]

    def check(actor, flags, flag, method, path, body, expected_status):
        response = actor.call(method, path, body=body, key=new_key())
        ctx.expect(flags[flag] is (response.status == expected_status),
                   f"{flag} matches {method} authorization for {role}", response)
        ctx.expect(response.status in (expected_status, 403, 404), "operation succeeds or is refused by policy", response)
        return response

    for role in ("owner", "admin", "member", "viewer", "lead", "other"):
        conversation = created(ctx, member, node)
        cid = conversation["conversation_id"]
        cpath = "conversations/" + cid
        hosted = lead(ctx, member, conversation)
        actor = {"owner": local, "admin": admin, "member": member,
                 "viewer": viewer, "lead": hosted, "other": other}[role]
        turn = post(ctx, member, conversation, "Exercise record permissions")
        origin = {"kind": "conversation_turn", "conversation_id": cid, "turn_id": turn["turn_id"]}
        made = submitted(ctx, hosted, node, origin=origin)
        iid = made["intent"]["intent_id"]
        jid = made["jobs"][0]["job_id"]
        intent = actor.call("GET", "intents/" + iid).json["intent"]
        check(actor, intent["actions"], "control", "PUT",
              f"intents/{iid}/owner-messages/{turn['turn_id']}/mark", {"mark": "instruction"}, 200)

        accept(ctx, node, jid)
        rid = finish(ctx, node, jid, run_id=f"{ctx.serial_tag()}-{role}")["envelope"]["run_id"]
        run = actor.call("GET", "runs/" + rid).json
        check(actor, run["actions"], "decide", "POST", f"runs/{rid}/decision", {"decision": "reject"}, 201)

        waiting = submitted(ctx, hosted, node, origin=origin)
        needs_iid, job = waiting["intent"]["intent_id"], waiting["jobs"][0]["job_id"]
        accept(ctx, node, job)
        approval_paths = []
        for verb in ("decide", "answer"):
            result = node.call("POST", f"jobs/{job}/action-approvals", body={
                "run_id": f"{ctx.serial_tag()}-{role}-approval", "requesting_model": "fixture",
                "needs": ["network"], "argv": ["/usr/bin/curl", "https://example.test"],
                "context": {"home": "/home/alice", "workspace": "/home/alice/work"}, "reason": f"Test {verb}",
            })
            ctx.expect(result.status == 200, "approval fixture created", result)
            aid = result.json["id"]
            path = "action-approvals/" + aid
            for _ in range(160):
                if local.call("GET", path).json["status"] == "needs_owner":
                    break
                time.sleep(0.025)
            with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
                db.execute("UPDATE action_approvals SET assigned_member_id=? WHERE id=?", (admin_id, aid))
            approval_paths.append((verb, path, aid))

        for verb, path, aid in approval_paths:
            read = actor.call("GET", path)
            flags = read.json["actions"] if read.status == 200 else dict.fromkeys(("decide", "answer", "clear"), False)
            if role in ("owner", "admin"):
                task = actor.call("GET", f"intents/{needs_iid}/work-trace").json["work_trace"]
                item = next(item for item in task["needs_you"] if item["action_approval_id"] == aid)
                ctx.expect(item["actions"] == flags and item["assignee_display_name"] == "admin",
                           "needs-you uses approval flags and includes assignee name", item)
            if role == "owner":
                attention = actor.call("GET", cpath).json["conversation"]["attention"]
                ctx.expect(attention["action_approval_id"] == aid and attention["kind"] == "action"
                           and attention["actions"] == flags, "attention projects approval kind and actions", attention)
            payload = {"decision": "deny"} if verb == "decide" else {"choice": "no_go"}
            decision = check(actor, flags, verb, "POST", path + "/" + verb, payload, 200)
            if decision.status == 200:
                ctx.expect(decision.json["actions"] == dict.fromkeys(("decide", "answer", "clear"), False),
                           "closed approval loses its actions")

        job_row = actor.call("GET", "jobs/" + job).json
        check(actor, job_row["actions"], "cancel", "POST", f"jobs/{job}/cancel", {}, 202)
        conv = actor.call("GET", cpath).json["conversation"]
        check(actor, conv["actions"], "control", "PATCH", cpath, {"title": "Updated title"}, 200)
        check(actor, conv["actions"], "archive", "POST", cpath + "/archive", {}, 200)
        actor_id = actor.call("GET", "me").json["member"]["id"]
        check(actor, conv["actions"], "adopt", "PATCH", cpath, {"sponsor_member_id": actor_id}, 200)
