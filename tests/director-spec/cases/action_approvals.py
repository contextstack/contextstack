"""Action approval routes, ownership and one-shot claims."""
import json
import time

from cases.client import accept, device, finish, lead, owner, refused, submitted
from cases.conversations import created, post
from harness.run import case


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
    ctx.expect(attention["action_approval_id"] == approval, "conversation attention includes approval", attention)
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

