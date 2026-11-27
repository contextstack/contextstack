"""Action approval routes, ownership and one-shot claims."""
import hmac
import json
import sqlite3
import time
from datetime import datetime, timezone

from cases.client import accept, device, finish, lead, owner, refused, submitted
from cases.conversations import created, post
from cases.policy import device as role_device, new_key
from harness.run import case
from harness.wire import JOB_FILES, sha


SCREENSHOT = bytes.fromhex("ffd8ffe000104a4649460001ffd9")
VALUES = [
    {"name": "qty", "value": "100", "hidden": True},
    {"name": "csrf", "prefix": "abcdef123456", "suffix": "wxyz", "length": 40,
     "hidden": True, "excluded": True},
    {"name": "password", "length": 10, "hidden": False},
    {"name": "confirm", "value": "Buy", "hidden": False},
]


def browser_bundle(ctx, local, target):
    original, _ = JOB_FILES["spec.yaml"]
    spec = original + f"""
browser:
  origins: [https://example.test]
  actions:
    read: true
    submit:
      - {{origin: https://example.test, form: "{target}", purpose: fixture}}
""".encode()
    files = local.upload_bundle()
    uploaded = local.call("PUT", "objects/" + sha(spec), raw=spec)
    ctx.expect(uploaded.status == 200, "browser spec uploaded", uploaded)
    for file in files:
        if file["path"] == "spec.yaml":
            file.update(sha256=sha(spec), bytes=len(spec))
    return files


def capture(node, job, run, binding):
    fields = dict(binding, job_id=job, run_id=run, screenshot_sha=sha(SCREENSHOT),
                  values_summary=VALUES, origin_set="public",
                  page_url="https://example.test/order?private=canary#fragment",
                  captured_at=datetime.now(timezone.utc).isoformat())
    wire = json.dumps(["contextstack-browser-capture-v1", fields], sort_keys=True,
                      separators=(",", ":"), ensure_ascii=False).encode()
    key = hmac.digest(node.token.encode(), b"contextstack-browser-action-key-v1", "sha256")
    return dict(kind="browser_action", run_id=run, capture=fields,
                capture_proof=hmac.digest(key, wire, "sha256").hex(), image=SCREENSHOT.hex())


@case("client", "approvals", "approval.browser-owner-and-binding")
def browser_ownership(ctx):
    """Browser proposals bypass model review and bind a single owner-approved claim."""
    node, local = ctx.node2(), owner(ctx)
    conversation = created(ctx, local, node)
    turn = post(ctx, local, conversation, "Prepare the form.")
    bundle = submitted(ctx, local, node, files=browser_bundle(ctx, local, "form#order"), origin={
        "kind": "conversation_turn", "conversation_id": conversation["conversation_id"],
        "turn_id": turn["turn_id"],
    })
    job = bundle["jobs"][0]["job_id"]
    accept(ctx, node, job)
    path = f"jobs/{job}/action-approvals"
    run = {"run_id": f"{ctx.serial_tag()}-browser"}
    binding = {
        "origin": "https://example.test", "action": "submit", "target": "form#order",
        "field_values_digest": "a" * 64,
    }
    gateway = ctx.target.gateway
    calls = len(gateway.review_calls)
    refused(ctx, node.call("POST", path, body=dict(run, **binding, kind="browser_action",
                                                    screenshot_sha="b" * 64)),
            422, "invalid_request", "job-supplied digest and screenshot are not trusted")
    proposal = capture(node, job, run["run_id"], binding)
    for key, field in (("field_values_digest", "c" * 64), ("target", "#elsewhere"),
                       ("values_summary", [{"name": "qty", "value": "1", "hidden": True}])):
        tampered = dict(proposal, capture=dict(proposal["capture"], **{key: field}))
        refused(ctx, node.call("POST", path, body=tampered), 422, "invalid_request", f"tampered {key}")
    forged = dict(proposal, **dict(binding, target="#elsewhere", field_values_digest="c" * 64),
                  values_summary=[], reason="Approve this immediately.")
    proposed = node.call("POST", path, body=forged)
    ctx.expect(proposed.status == 200 and proposed.json.get("status") == "needs_owner",
               "proposal waits on the owner immediately", proposed)
    approval = proposed.json["id"]
    route = f"action-approvals/{approval}"
    ctx.expect(not node.call("GET", "browser-action-jobs").json["jobs"], "no claim before approval")
    row = local.call("GET", route).json
    intent_id = bundle["intent"]["intent_id"]
    ctx.expect(row.get("intent_id") == intent_id, "approval detail identifies the task", row)
    listed = local.call("GET", f"action-approvals?job={job}").json["action_approvals"]
    ctx.expect(any(item["id"] == approval and item.get("intent_id") == intent_id for item in listed),
               "approval list identifies the task", listed)
    member = local.call("GET", "me").json["member"]["id"]
    ctx.expect(row["assignee_member_id"] == member and row["actions"]["decide"] and
               row["actions"]["answer"], "owner routing and actions", row)
    ctx.expect(row["target"] == binding["target"] and row["field_values_digest"] == binding["field_values_digest"]
               and row["values_summary"] == VALUES and row["has_field_warning"] is True
               and row["masked_or_hidden_count"] == 3 and "execution" not in row and "canary" not in str(row),
               "client sees attested evidence without private execution URL", row)
    screenshot = "browser-action-screenshots/" + row["screenshot_sha"]
    image = local.call("GET", screenshot)
    ctx.expect(image.status == 200 and image.body == SCREENSHOT and image.header("content-type") == "image/jpeg",
               "owner can fetch the captured screenshot", image)
    ctx.expect("reviewer_model" not in row and "Approve this" not in str(row),
               "page-derived reason is discarded", row)
    trace = local.call("GET", f"intents/{intent_id}/work-trace")
    item = next(item for item in trace.json["work_trace"]["needs_you"]
                if item["action_approval_id"] == approval)
    ctx.expect(all(item[key] == row[key] for key in ("intent_id", "origin", "action", "target", "screenshot_sha",
                                                    "values_summary", "expires_at",
                                                    "has_field_warning", "masked_or_hidden_count")),
               "needs-you shows the browser proposal", item)

    hosted = lead(ctx, local, conversation, bundle["intent"]["intent_id"])
    for actor, code in ((role_device(ctx, "admin"), "forbidden"), (hosted, "owner_only"),
                        (role_device(ctx, "viewer"), "forbidden")):
        for operation, body in (
            ("decide", {"decision": "approve"}), ("answer", {"choice": "go"}),
            ("answer", {"choice": "acknowledge"}), ("clear", {}),
        ):
            refused(ctx, actor.call("POST", f"{route}/{operation}", body=body), 403, code,
                    f"non-owner {operation}")
        ctx.expect(actor.call("GET", screenshot).status == 403, "owner-only screenshot")
        visible = actor.call("GET", route)
        if actor is hosted:
            ctx.expect(visible.status == 200 and visible.json.get("intent_id") == intent_id,
                       "lead approval status identifies the task", visible)
        if visible.status == 200:
            ctx.expect(not visible.json["actions"]["decide"] and not visible.json["actions"]["answer"],
                       "non-owner actions are disabled", visible)

    approved = local.call("POST", route + "/answer", body={"choice": "go"})
    ctx.expect(approved.status == 200 and approved.json["status"] == "approved", "owner approves", approved)
    ctx.expect(approved.json.get("intent_id") == intent_id, "decision response identifies the task", approved)
    followup = approved.json["followup_job_id"]
    claim = f"jobs/{followup}/action-approvals/{approval}/claim"
    run = {"run_id": followup}
    for field, changed in (("field_values_digest", "c" * 64), ("origin", "https://other.test"),
                           ("target", "#other"), ("action", "buy")):
        presented = dict(run, **binding)
        presented[field] = changed
        refused(ctx, node.call("POST", claim, body=presented), 409, "conflict", f"changed {field}")
    claimed = node.call("POST", claim, body=dict(run, **binding))
    ctx.expect(claimed.status == 200 and claimed.json.get("claimed_at") and
               len(claimed.json.get("claim_proof", "")) == 64, "exact binding is claimed", claimed)
    ctx.expect("claim_proof" not in local.call("GET", route).json,
               "claim proof stays on the one-time node response")
    refused(ctx, node.call("POST", claim, body=dict(run, **binding)), 409, "conflict", "second claim")
    outcome = node.call("POST", claim.removesuffix("claim") + "outcome",
                        body=dict(run, status="submission_unknown"))
    ctx.expect(outcome.status == 200 and outcome.json["outcome"]["status"] == "submission_unknown"
               and "Check the site before trying again." in outcome.json["outcome"]["message"],
               "uncertain delivery is recorded with the check-before-retry instruction", outcome)
    ctx.expect(len(gateway.review_calls) == calls, "browser approval made no reviewer calls")


@case("client", "approvals", "approval.browser-followup")
def browser_followup(ctx):
    """A declared scheduled action survives the proposing job and queues one bound follow-up."""
    node, other, local = ctx.node2(), ctx.node2(), owner(ctx)
    bundle = submitted(ctx, local, node, files=browser_bundle(ctx, local, "#order"))
    source = bundle["jobs"][0]["job_id"]
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE jobs SET meta=json_set(meta,'$.schedule_id','browser-fixture') WHERE job_id=?", (source,))
    accept(ctx, node, source)
    path = f"jobs/{source}/action-approvals"
    binding = {"origin": "https://example.test", "action": "submit", "target": "#order",
               "field_values_digest": "a" * 64}
    undeclared = capture(node, source, "browser-undeclared", dict(binding, target="#elsewhere"))
    refused(ctx, node.call("POST", path, body=undeclared), 422,
            "invalid_request", "undeclared form")
    outcomes = (
        ("submitted", {"status": "submitted"},
         {"status": "submitted",
          "message": "The site received the submission. Check the site before trying again."},
         "done", "browser_action"),
        ("submission_unknown", {"status": "submission_unknown"},
         {"status": "submission_unknown",
          "message": "The site may have received the submission. Check the site before trying again."},
         "done", "browser_action_unknown"),
        ("refused", {"status": "refused", "reason": "site blocked the submission"},
         {"status": "refused", "message": "The browser refused the action before sending the submission.",
          "reason": "site blocked the submission"},
         "error", "browser_action"),
        ("succeeded", {"exit_code": 0}, {"status": "succeeded", "exit_code": 0},
         "done", "browser_action"),
    )
    approvals = []
    for status, report, expected, state, stop in outcomes:
        proposed = node.call("POST", path, body=capture(node, source, f"browser-{status}", binding))
        ctx.expect(proposed.status == 200 and proposed.json.get("status") == "needs_owner",
                   f"{status} proposal accepted", proposed)
        approvals.append((proposed.json["id"], status, report, expected, state, stop))

    finish(ctx, node, source, run_id="browser-proposals")
    ctx.expect(all(local.call("GET", f"action-approvals/{item[0]}").json["status"] == "needs_owner"
                   for item in approvals), "job end preserves proposals", approvals)
    ctx.expect(node.call("GET", "browser-action-jobs").json["jobs"] == [],
               "no work before approval")
    queued = []
    for approval, status, report, expected, state, stop in approvals:
        route = f"action-approvals/{approval}"
        approved = local.call("POST", route + "/decide", body={"decision": "approve"})
        ctx.expect(approved.status == 200 and approved.json.get("followup_job_id"),
                   f"{status} follow-up queued", approved)
        queued.append((approved.json["followup_job_id"], approval, status, report, expected, state, stop))

    pending = node.call("GET", "browser-action-jobs").json["jobs"]
    ctx.expect({item["job_id"] for item in pending} == {item[0] for item in queued},
               "all follow-ups belong to the owning node", pending)
    ctx.expect(other.call("GET", "browser-action-jobs").json["jobs"] == [],
               "other node has no follow-up")
    for job, approval, status, report, expected, state, stop in queued:
        claim = f"jobs/{job}/action-approvals/{approval}/claim"
        claimed = node.call("POST", claim, body=dict(binding, run_id=job))
        ctx.expect(claimed.status == 200 and claimed.json.get("claim_proof"),
                   f"{status} follow-up claimed", claimed)
        refused(ctx, node.call("POST", claim, body=dict(binding, run_id=job)),
                409, "conflict", f"{status} follow-up replay")
        result = node.call("POST", claim.removesuffix("claim") + "outcome",
                           body=dict(report, run_id=job))
        ctx.expect(result.status == 200 and result.json["outcome"] == expected,
                   f"{status} approval outcome stored", result)
        finished = local.call("GET", f"jobs/{job}").json
        ctx.expect(finished["state"] == state
                   and finished["terminal_row"] == {"state": state, "stop": stop},
                   f"{status} follow-up terminal mapping", finished)


@case("client", "approvals", "approval.browser-expiry-clear-and-reroute")
def browser_expiry_clear_and_reroute(ctx):
    node, local = ctx.node2(), owner(ctx)
    peer = role_device(ctx, "owner")
    peer_id = peer.call("GET", "me").json["member"]["id"]
    owner_id = local.call("GET", "me").json["member"]["id"]
    job = submitted(ctx, local, node, files=browser_bundle(ctx, local, "#order"))["jobs"][0]["job_id"]
    accept(ctx, node, job)
    path = f"jobs/{job}/action-approvals"
    binding = {"origin": "https://example.test", "action": "submit", "target": "#order",
               "field_values_digest": "a" * 64}
    expiry_decisions = {"decide": {"decision": "approve"}, "answer": {"choice": "go"}}
    for operation in (*expiry_decisions, "late", "clear", "reroute"):
        proposed = node.call("POST", path, body=capture(node, job, operation, binding))
        ctx.expect(proposed.status == 200, "record capture", proposed)
        approval = proposed.json["id"]
        route = "action-approvals/" + approval
        database = ctx.target.data / "director.sqlite"
        if operation in expiry_decisions:
            with sqlite3.connect(database) as db:
                db.execute("UPDATE action_approvals SET request=json_set(request,'$.expires_at','2000-01-01T00:00:00Z') WHERE id=?", (approval,))
            refused(ctx, local.call("POST", f"{route}/{operation}", body=expiry_decisions[operation]),
                    409, "conflict", f"{operation} on expired proposal")
            ctx.expect(local.call("GET", route).json["status"] == "expired", "expiry is visible")
            continue
        if operation == "reroute":
            with sqlite3.connect(database) as db:
                db.execute("UPDATE action_approvals SET assigned_member_id=? WHERE id=?", (peer_id, approval))
            demoted = local.call("PATCH", "members/" + peer_id, body={"role": "admin"}, key=new_key())
            ctx.expect(demoted.status == 200 and local.call("GET", route).json["assignee_member_id"] == owner_id,
                       "demotion keeps approval with an active owner", demoted)
            continue
        approved = local.call("POST", route + "/decide", body={"decision": "approve"})
        followup = approved.json["followup_job_id"]
        if operation == "late":
            with sqlite3.connect(database) as db:
                db.execute("UPDATE action_approvals SET decided_at=strftime('%Y-%m-%dT%H:%M:%SZ','now','-301 seconds') WHERE id=?", (approval,))
        else:
            cleared = local.call("POST", route + "/clear", body={})
            ctx.expect(cleared.status == 200 and cleared.json["status"] == "denied", "cleared", cleared)
        claim = node.call("POST", f"jobs/{followup}/action-approvals/{approval}/claim",
                          body=dict(binding, run_id=followup))
        refused(ctx, claim, 409, "conflict", f"{operation} claim")


@case("client", "approvals", "approval.browser-ownerless-reroute")
def browser_ownerless_reroute(ctx):
    """A membership change can proceed even if a stranded approval has no eligible owner."""
    node, local = ctx.node2(), owner(ctx)
    admin, former = role_device(ctx, "admin"), role_device(ctx, "member")
    admin_id = admin.call("GET", "me").json["member"]["id"]
    former_id = former.call("GET", "me").json["member"]["id"]
    job = submitted(ctx, local, node, files=browser_bundle(ctx, local, "#order"))["jobs"][0]["job_id"]
    accept(ctx, node, job)
    path = f"jobs/{job}/action-approvals"
    binding = {"origin": "https://example.test", "action": "submit", "target": "#order",
               "field_values_digest": "a" * 64}
    proposed = node.call("POST", path, body=capture(node, job, "ownerless", binding))
    ctx.expect(proposed.status == 200, "create browser proposal", proposed)
    approval = proposed.json["id"]
    database = ctx.target.data / "director.sqlite"
    with sqlite3.connect(database) as db:
        org = db.execute("SELECT org_id FROM members WHERE id=?", (admin_id,)).fetchone()[0]
        db.execute("UPDATE members SET status='disabled' WHERE org_id=? AND role='owner'", (org,))
        db.execute("UPDATE action_approvals SET assigned_member_id=? WHERE id=?", (former_id, approval))
    changed = admin.call("PATCH", "members/" + former_id, body={"role": "viewer"}, key=new_key())
    ctx.expect(changed.status == 200, "membership change succeeds without an eligible owner", changed)
    with sqlite3.connect(database) as db:
        row = db.execute("SELECT assigned_member_id,status FROM action_approvals WHERE id=?", (approval,)).fetchone()
        ctx.expect(row == (former_id, "needs_owner"), "browser proposal retains its assignment")
        db.execute("UPDATE action_approvals SET assigned_member_id=? WHERE id=?", (admin_id, approval))
        db.execute("UPDATE members SET status='active' WHERE org_id=? AND role='owner'", (org,))
    refused(ctx, admin.call("GET", "action-approvals/" + approval), 404, "not_found",
            "assignment does not expose browser evidence to an admin")
    listed = admin.call("GET", "action-approvals")
    ctx.expect(listed.status == 200 and all(row["id"] != approval for row in listed.json["action_approvals"]),
               "stranded browser evidence stays out of the admin list", listed)


@case("client", "approvals", "approval.owner-and-one-shot")
def ownership(ctx):
    """An accepted job's node can ask and claim. Only the owner can decide."""
    node, other = ctx.node2(), ctx.node2()
    local, paired = owner(ctx), device(ctx)
    conversation = created(ctx, local, node)
    turn = post(ctx, local, conversation, "Prepare the requested action.")
    bundle = submitted(ctx, local, node, origin={
        "kind": "conversation_turn", "conversation_id": conversation["conversation_id"],
        "turn_id": turn["turn_id"],
    })
    job = bundle["jobs"][0]["job_id"]
    accept(ctx, node, job)
    path = f"jobs/{job}/action-approvals"
    run = {"run_id": f"{ctx.serial_tag()}-action"}
    request = dict(run, asking="ignored", requesting_model="requester", needs=["network"],
                   argv=["git", "push", "--force"], reason="publish work")
    refused(ctx, other.call("POST", path, body=request), 404, "not_found", "another node's job")
    first = node.call("POST", path, body=request)
    ctx.expect(first.status == 200 and first.json["status"] == "pending", "request recorded", first)
    approval = first.json["id"]
    route = "action-approvals/" + approval
    until = time.monotonic() + 4
    while time.monotonic() < until:
        row = paired.call("GET", route).json
        if row["status"] != "pending":
            break
        time.sleep(.025)
    ctx.expect(row["status"] == "needs_owner" and row["verdict"] == "no_go"
               and "reviewer_model" not in row and row["argv"] == request["argv"], "hard line skipped review", row)
    ctx.expect(row["job_id"] == job and row["run_id"] == run["run_id"]
               and row["requesting_model"] == "requester", "record binds job, run and model", row)
    listed = local.call("GET", f"action-approvals?status=needs_owner&job={job}")
    ctx.expect(listed.json == {"action_approvals": [row]}, "owner queue includes the action", listed)
    trace = local.call("GET", f"intents/{bundle['intent']['intent_id']}/work-trace")
    ctx.expect(any(item.get("action_approval_id") == approval and item["status"] == "open"
                   for item in trace.json["work_trace"]["needs_you"]), "action is on needs-you", trace)
    attention = local.call("GET", "conversations/" + conversation["conversation_id"]).json["conversation"]["attention"]
    ctx.expect(attention["action_approval_id"] == approval
               and attention.get("intent_id") == bundle["intent"]["intent_id"],
               "conversation attention identifies the approval and task", attention)
    decision = {"decision": "approve"}
    claim = f"{path}/{approval}/claim"
    refused(ctx, node.call("POST", claim, body=run), 409, "conflict", "claim before approval")
    approved = paired.call("POST", route + "/decide", body=decision)
    ctx.expect(approved.status == 200 and approved.json["status"] == "approved"
               and approved.json["decided_by"] == paired.credential_id and approved.json["decided_at"],
               "paired owner decision is attributed", approved)
    ctx.expect(paired.call("GET", route).json["decided_by"] == paired.credential_id,
               "the actor is persisted on the approval")
    refused(ctx, local.call("POST", route + "/decide", body=decision), 409, "conflict", "decision is final")
    refused(ctx, other.call("POST", claim, body=run), 404, "not_found", "claim by another node")
    refused(ctx, node.call("POST", claim, body={"run_id": "wrong"}), 404, "not_found", "claim by another run")
    claimed = node.call("POST", claim, body=run)
    ctx.expect(claimed.status == 200 and claimed.json["outcome"] == {"status": "unknown"},
               "claim consumed before launch", claimed)
    refused(ctx, node.call("POST", claim, body=run), 409, "conflict", "second claim")
    replay = node.call("POST", path, body=request)
    ctx.expect(replay.json["id"] == approval and replay.json.get("claimed_at"), "replay returns same record", replay)
    outcome = node.call("POST", f"{path}/{approval}/outcome", body=dict(run, exit_code=0))
    ctx.expect(outcome.status == 200 and outcome.json["outcome"] == {"status":"succeeded","exit_code":0},
               "result stored", outcome)
    trace = local.call("GET", f"intents/{bundle['intent']['intent_id']}/work-trace")
    ctx.expect(any(item.get("action_approval_id") == approval and item["status"] == "cleared"
                   for item in trace.json["work_trace"]["needs_you"]), "decision clears needs-you", trace)

    second = node.call("POST", path, body=dict(request, reason="another reason"))
    second_path = "action-approvals/" + second.json["id"]
    until = time.monotonic() + 4
    while local.call("GET", second_path).json["status"] == "pending" and time.monotonic() < until:
        time.sleep(.025)
    denied = local.call("POST", second_path + "/decide", body={"decision":"deny"})
    ctx.expect(denied.status == 200 and denied.json["status"] == "denied", "owner denies", denied)


@case("client", "approvals", "approval.reviews-independent-model")
def independent_review(ctx):
    """The Director calls a different model without tools and applies its verdict."""
    node, local = ctx.node2(), owner(ctx)
    bundle = submitted(ctx, local, node)
    job = bundle["jobs"][0]["job_id"]
    accept(ctx, node, job)
    path = f"jobs/{job}/action-approvals"
    context = {"home": "/home/fixture", "workspace": "/home/fixture/workspace",
               "job_tmp": "/home/fixture/runs/run/tmp",
               "goal": "Verify the lane changes.", "diff": "diff --git a/src.rs b/src.rs",
               "scripts": [{"path": "cs/tasks/regressions/trace-e2e/check",
                            "contents": "#!/bin/sh\nexit 0\n"}]}
    gateway = ctx.target.gateway
    def refusal(source, reason):
        return {"status": "error", "error": {"code": "gateway.policy_denied",
                "message": "private upstream detail",
                "budget": {"source": source, "reason": reason}}}

    gateway.review_calls.clear()
    try:
        for scenario, verdict, status in (("go", "go", "approved"),
                                           ("no_go", "no_go", "needs_owner"),
                                           ("refused", "no_go", "needs_owner"),
                                           ("empty", "go", "approved")):
            answer = {"status": "ok", "model": "review-model", "message": {
                "content": json.dumps({"verdict": verdict, "reason": "fixture review reason"})}}
            gateway.review_responses = {
                "first-model": ("anthropic", {"status": "ok", "model": "first-model", "message": {"content": None}}
                                if scenario == "empty" else refusal("anthropic", "off")),
                "review-model": ("openai", refusal("openai", "exhausted") if scenario == "refused" else answer),
            }
            request = {"run_id": f"{ctx.serial_tag()}-{scenario}", "context": context,
                       "requesting_model": "local-coder", "needs": ["network:loopback:3000", "write_paths"],
                       "write_paths": [context["job_tmp"]],
                       "argv": ["cs/tasks/regressions/trace-e2e/check"],
                       "reason": "The regression needs a local server and scratch files."}
            sent = node.call("POST", path, body=request)
            ctx.expect(sent.status == 200, "request accepted", sent)
            until = time.monotonic() + 4
            row = sent.json
            while row["status"] == "pending" and time.monotonic() < until:
                time.sleep(.025)
                row = local.call("GET", "action-approvals/" + row["id"]).json
            ctx.expect(row["status"] == status and row["verdict"] == verdict, "verdict persisted", row)
            if scenario == "refused":
                ctx.expect(row["outcome"]["status"] == "waiting_on_owner"
                           and row.get("reviewer_model") is None
                           and row["review_reason"] == (
                               "No action reviewer could run: first-model refused (gateway.policy_denied: anthropic off); "
                               "review-model refused (gateway.policy_denied: openai exhausted)."
                           ), "refusals escalated with safe details", row)
            else:
                ctx.expect(row["reviewer_model"] == "review-model"
                           and row["review_reason"] == "fixture review reason", "answered reviewer persisted", row)
            first, call = gateway.review_calls[-2:]
            ctx.expect(first["capability"]["model_tier_hint"] == "first-model"
                       and call["capability"]["model_tier_hint"] == "review-model"
                       and first["request_id"] != call["request_id"], "reviewers tried in order", (first, call))
            ctx.expect(call["capability"]["model_tier_hint"] != request["requesting_model"]
                       and not call.get("tool_schema"), "different model with no tools", call)
            prompt = json.loads(call["messages"][1]["content"])
            ctx.expect(prompt["lane"] == context and prompt["scope"] == "routine_widening"
                       and prompt["request"]["argv"] == request["argv"]
                       and prompt["request"]["needs"] == request["needs"]
                       and prompt["request"]["write_paths"] == request["write_paths"]
                       and prompt["request"]["reason"] == request["reason"],
                       "reviewer received the regression source and exact widening", call)
        ctx.expect(len(gateway.review_calls) == 8, "two calls per review", gateway.review_calls)
    finally:
        gateway.review_responses = {}
        gateway.review_calls.clear()


@case("client", "approvals", "approval.change-events")
def change_events(ctx):
    """Approval changes reach authorized clients and stay outside a foreign lead's scope."""
    node, foreign_node, local = ctx.node2(), ctx.node2(), owner(ctx)
    job = submitted(ctx, local, node)["jobs"][0]["job_id"]
    accept(ctx, node, job)
    foreign = submitted(ctx, local, foreign_node)
    foreign_intent = foreign["intent"]["intent_id"]
    foreign_job = foreign["jobs"][0]["job_id"]
    unrelated = lead(ctx, local, created(ctx, local, foreign_node, intent_id=foreign_intent), foreign_intent)
    own_stream, foreign_stream = device(ctx).stream(), unrelated.stream()
    try:
        for stream in (own_stream, foreign_stream):
            ctx.expect(stream.until(lambda name, _: name == "hello")[0] is not None, "stream ready")

        path = f"jobs/{job}/action-approvals"
        run = {"run_id": f"{ctx.serial_tag()}-events"}
        request = dict(run, requesting_model="requester", needs=["network"],
                       argv=["git", "push", "--force"], reason="publish work")
        sent = node.call("POST", path, body=request)
        ctx.expect(sent.status == 200 and sent.json["status"] == "pending", "approval created", sent)
        approval = sent.json["id"]
        route = f"action-approvals/{approval}"

        def changed(step, previous=0):
            event, _ = own_stream.until(lambda name, d: name == "change"
                                        and d["kind"] == "action_approval" and d["id"] == approval)
            ctx.expect(event is not None, f"{step} emits an approval change")
            _, data, event_id = event
            revision = data["revision"]
            ctx.expect(revision > previous and str(revision) == event_id
                       and data["record"] == {"id": approval, "job_id": job, "revision": revision},
                       f"{step} has a minimal refetch payload", event)
            return revision

        revision = changed("create")
        revision = changed("review escalation", revision)
        approved = local.call("POST", route + "/decide", body={"decision": "approve"})
        ctx.expect(approved.status == 200, "owner decides", approved)
        revision = changed("owner decision", revision)
        stem = f"{path}/{approval}"
        claimed = node.call("POST", stem + "/claim", body=run)
        ctx.expect(claimed.status == 200, "node claims approved action", claimed)
        revision = changed("claim", revision)
        outcome = node.call("POST", stem + "/outcome", body=dict(run, exit_code=0))
        ctx.expect(outcome.status == 200, "node records outcome", outcome)
        changed("outcome", revision)

        refused(ctx, unrelated.call("GET", route), 404, "not_found", "foreign approval record")
        accept(ctx, foreign_node, foreign_job)
        marker, seen = foreign_stream.until(lambda name, d: name == "change"
                                            and d["kind"] == "job" and d["id"] == foreign_job)
        ctx.expect(marker is not None, "foreign stream receives a later in-scope job change")
        ctx.expect(all(name != "change" or d["id"] != approval for name, d, _ in seen),
                   "foreign stream receives no approval changes", seen)
    finally:
        own_stream.close()
        foreign_stream.close()

@case("client", "approvals", "approval.expires-when-job-ends")
def expires_when_job_ends(ctx):
    """Pending and needs_owner approvals expire when their job finishes; a live job is untouched."""
    node, local = ctx.node2(), owner(ctx)
    live_job = submitted(ctx, local, node)["jobs"][0]["job_id"]
    accept(ctx, node, live_job)
    ended = submitted(ctx, local, node)
    ended_job = ended["jobs"][0]["job_id"]
    accept(ctx, node, ended_job)

    def request(job, tag, reason):
        path = f"jobs/{job}/action-approvals"
        body = {"run_id": f"{ctx.serial_tag()}-{tag}", "requesting_model": "requester",
                "needs": ["network"], "argv": ["git", "push", "--force"], "reason": reason}
        sent = node.call("POST", path, body=body)
        ctx.expect(sent.status == 200 and sent.json["status"] == "pending", "approval created", sent)
        route = "action-approvals/" + sent.json["id"]
        until = time.monotonic() + 4
        row = sent.json
        while row["status"] == "pending" and time.monotonic() < until:
            time.sleep(.025)
            row = local.call("GET", route).json
        ctx.expect(row["status"] == "needs_owner", "hard line escalates", row)
        return sent.json["id"], route

    ended_id, ended_route = request(ended_job, "ended", "publish ended work")
    live_id, live_route = request(live_job, "live", "publish live work")
    stream = device(ctx).stream()
    try:
        ctx.expect(stream.until(lambda name, _: name == "hello")[0] is not None, "stream ready")
        finish(ctx, node, ended_job)
        event, _ = stream.until(lambda name, d: name == "change" and d["kind"] == "action_approval"
                                and d["id"] == ended_id)
        ctx.expect(event is not None, "expiry publishes an approval change")
        ended_row = local.call("GET", ended_route).json
        ctx.expect(ended_row["status"] == "expired"
                   and ended_row["outcome"] == {"status": "expired", "reason": "job ended"},
                   "ended job approval expires", ended_row)
        live_row = local.call("GET", live_route).json
        ctx.expect(live_row["status"] == "needs_owner" and live_row["id"] == live_id,
                   "running job approval stays open", live_row)
        listed = local.call("GET", f"action-approvals?status=needs_owner&job={ended_job}")
        ctx.expect(listed.json == {"action_approvals": []}, "expired approval leaves the queue", listed)
    finally:
        stream.close()

