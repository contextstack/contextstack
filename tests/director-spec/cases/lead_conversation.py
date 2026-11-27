"""Hosted-lead conversations: Director-bundled create, lead marker on reads."""
from harness.client import new_key
from harness.run import case

from cases.client import owner
from cases.conversations import bundle as chat_bundle, created


def pin_lead_node(o, node):
    o.call("PUT", "lead-node", body={"node": node.name})


C = "client"


@case(C, "lead-conversation", "land-policy-and-one-use")
def _(ctx):
    import json
    import sqlite3
    import subprocess
    import sys
    from contextlib import closing
    from pathlib import Path

    from cases.client import accept, finish, lead, refused, submitted
    from cases.policy import device

    sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "cs/ops/bin"))
    try:
        from _gate_job import run_steps, step
    finally:
        sys.path.pop(0)

    node, other, local = ctx.node2(), ctx.node2(), owner(ctx)
    pin_lead_node(local, node)

    def project(name):
        return local.call("POST", "projects", body={"name": name}, key=new_key()).json["project"]["project_id"]

    own_project, other_project = project("landing"), project("other landing")
    member = device(ctx, "member")
    conv = member.call("POST", "conversations", body={
        "node": node.name, "spec": "hosted-lead", "model": "claude-code-sonnet", "project_id": own_project,
    }, key=new_key()).json["conversation"]
    database = ctx.target.data / "director.sqlite"
    sponsor = member.call("GET", "me").json["member"]["id"]
    with closing(sqlite3.connect(database)) as db:
        org, = db.execute("SELECT org_id FROM conversations WHERE conversation_id=?",
                          (conv["conversation_id"],)).fetchone()
    hosted = lead(ctx, local, conv)
    route = "conversations/" + conv["conversation_id"] + "/land"
    commit, base = "1" * 40, "2" * 40

    def done(lane, run_id, *, role=None, artifact=None, project_id=own_project, actor=local,
             reviewed=commit, review_verdict="approve", verdict="pass"):
        meta = {"repo": "fixture", "lane": lane}
        if role:
            meta["agent"] = role
        job = submitted(ctx, actor, node, project_id=project_id, meta=meta)["jobs"][0]["job_id"]
        accept(ctx, node, job)
        row = {"base_sha": reviewed, "verdict": verdict, "lane": {
            "repo": "fixture", "lane": lane, "path": lane, "branch": "lane/" + lane,
            "base": "main", "dirty_at_start": False,
        }}
        if role == "gate":
            row["model"] = "gate"
        elif role == "reviewer":
            row["review_verdict"] = review_verdict
        finish(ctx, node, job, run_id=run_id, files=artifact, row_fields=row)

    def gate_receipt(*, red=False, host_only=False, inconsistent=False):
        steps = [step("fixture check", ["fixture-check"])]
        if host_only:
            steps.append(step("host check", ["fixture-host"], host_only=True))
        gate = run_steps(steps, lambda item, _: subprocess.CompletedProcess(
            item["command"], int(red), stdout="", stderr=""), timeout=30)
        if inconsistent:
            gate["complete"] = True  # The policy must also check uncovered independently.
        return {"gate.json": (json.dumps(gate) + "\n").encode()}

    done("other-project", "land-other", project_id=other_project)
    refused(ctx, hosted.call("POST", route, body={"repo": "fixture", "lane": "other-project"}),
            404, "land_lane_not_found", "lead cannot request a foreign project's lane")

    scenarios = ("green", "incomplete", "uncovered", "no-reviewer", "protected", "tracing",
                 "stale-review", "stale-gate", "latest-red", "lead-review", "member-review", "latest-review-red")
    for lane in scenarios:
        done(lane, "land-writer-" + lane, actor=hosted)
        done(lane, "land-gate-" + lane, role="gate", actor=hosted,
             artifact=gate_receipt(host_only=lane in ("incomplete", "uncovered"),
                                   inconsistent=lane == "uncovered"),
             reviewed=base if lane == "stale-gate" else commit)
        if lane == "latest-red":
            done(lane, "land-gate-red", role="gate",
                 artifact=gate_receipt(red=True), verdict="fail")
        if lane != "no-reviewer":
            done(lane, "land-review-" + lane, role="reviewer",
                 reviewed=base if lane == "stale-review" else commit,
                 actor={"lead-review": hosted, "member-review": member}.get(lane, local))
        if lane == "latest-review-red":
            done(lane, "land-review-red", role="reviewer", review_verdict="changes needed")
        body = {"repo": "fixture", "lane": lane}
        request = hosted.call("POST", route, body=body)
        ctx.expect(request.status == 200 and request.json["kind"] == "land"
                   and request.json["status"] == "pending"
                   and "job_id" not in request.json and "run_id" not in request.json,
                   "land recorded as a conversation approval", request)
        approval = request.json["id"]
        with closing(sqlite3.connect(database)) as db:
            attribution = db.execute(
                "SELECT org_id, requested_by_member_id FROM land_requests WHERE approval_id=?", (approval,),
            ).fetchall()
        ctx.expect(attribution == [(org, sponsor)], "the request records the lead's sponsoring member")
        ctx.expect(hosted.call("POST", route, body=body).json["id"] == approval,
                   "repeat request is coalesced", request)
        offers = node.call("GET", "land-requests")
        ctx.expect(any(r["id"] == approval for r in offers.json["land_requests"]), "node sees request", offers)
        path = "land-requests/" + approval
        evidence = {"commit": commit, "base": base, "protected": lane == "protected", "tracing": lane == "tracing"}
        refused(ctx, other.call("POST", path + "/inspect", body=evidence), 404, "land_lane_not_found",
                "different node cannot submit inspection")
        decision = node.call("POST", path + "/inspect", body=evidence)
        expected = "approved" if lane == "green" else "needs_owner"
        ctx.expect(decision.status == 200 and decision.json["status"] == expected, lane + " policy", decision)
        if expected == "needs_owner":
            attention = local.call("GET", "conversations/" + conv["conversation_id"]).json["conversation"]["attention"]
            ctx.expect(attention["action_approval_id"] == approval, "owner sees Needs you", attention)
            refused(ctx, node.call("POST", path + "/claim", body={}), 409, "conflict", "owner has not approved")
            chosen = local.call("POST", "action-approvals/" + approval + "/decide", body={"decision": "approve"})
            ctx.expect(chosen.json["status"] == "approved" and chosen.json["decided_by"], "owner approved", chosen)
        claim = node.call("POST", path + "/claim", body={})
        ctx.expect(claim.status == 200 and claim.json["claimed_at"]
                   and claim.json["target_run_id"] == "land-writer-" + lane,
                   "claim authorizes only the recorded lane/run", claim)
        refused(ctx, node.call("POST", path + "/claim", body={}), 409, "conflict", "claim consumed")
        if lane == "green":
            busy = {"status": "busy", "claim_revision": claim.json["revision"]}
            released = node.call("POST", path + "/outcome", body=busy)
            ctx.expect(released.status == 200 and not released.json.get("claimed_at"),
                       "preflight contention releases the claim", released)
            reclaimed = node.call("POST", path + "/claim", body={})
            ctx.expect(reclaimed.status == 200 and reclaimed.json["revision"] > claim.json["revision"],
                       "the same approval can be retried", reclaimed)
            replayed = node.call("POST", path + "/outcome", body=busy)
            ctx.expect(replayed.status == 200 and replayed.json.get("claimed_at"),
                       "replaying old contention leaves the new claim intact", replayed)
            run_id = claim.json["target_run_id"]
            failed = dict(state="failed", repo="fixture", lane=lane, main="main", landed_by="hosted-lead",
                          commit=None, reason="fixture flake")
            landed = dict(failed, state="landed", commit=commit, reason=None)
            ctx.expect(node.land(run_id, failed, new_key()).status == 201, "first attempt recorded")
            result = node.land(run_id, landed, new_key())
            ctx.expect(result.status == 201 and result.json["attempts"][0]["state"] == "failed",
                       "retry preserves the first attempt on the run", result)
            finished = node.call("POST", path + "/outcome",
                                 body={"status": "succeeded", "attempts": [failed, landed]})
            ctx.expect(finished.status == 200 and finished.json["outcome"]["status"] == "succeeded",
                       "node recorded its outcome", finished)
            ctx.expect(hosted.call("POST", route, body=body).json["id"] == approval,
                       "completed request is still one use", finished)

    lane = "stale"
    done(lane, "land-writer-stale")
    body = {"repo": "fixture", "lane": lane}

    def request():
        result = hosted.call("POST", route, body=body)
        ctx.expect(result.status == 200 and result.json["status"] == "pending", "a fresh inspection", result)
        return result.json["id"]

    unavailable = request()
    result = node.call("POST", "land-requests/" + unavailable + "/unavailable", body={})
    ctx.expect(result.status == 200 and result.json["outcome"]["status"] == "unavailable",
               "an unpinned request ends without owner attention", result)
    refused(ctx, local.call("POST", "action-approvals/" + unavailable + "/decide", body={"decision": "approve"}),
            409, "conflict", "an unavailable request cannot be approved")
    pending = request()
    ctx.expect(pending != unavailable, "unavailable request does not block a fresh inspection")
    with closing(sqlite3.connect(database)) as db, db:
        db.execute("UPDATE action_approvals SET created_at='2000-01-01' WHERE id=?", (pending,))
    waiting = request()
    ctx.expect(waiting != pending, "stale pending request does not block a fresh inspection")
    result = node.call("POST", "land-requests/" + waiting + "/inspect",
                       body={"commit": commit, "base": base, "protected": False, "tracing": False})
    ctx.expect(result.json["status"] == "needs_owner", "inspection is waiting for the owner", result)
    with closing(sqlite3.connect(database)) as db, db:
        db.execute("UPDATE action_approvals SET reviewed_at='2000-01-01' WHERE id=?", (waiting,))
    fresh = request()
    ctx.expect(fresh != waiting, "stale owner request does not block a fresh inspection")
    evidence = {"commit": commit, "base": base, "protected": False, "tracing": False}
    path = "land-requests/" + fresh
    node.call("POST", path + "/inspect", body=evidence)
    local.call("POST", "action-approvals/" + fresh + "/decide", body={"decision": "approve"})
    claim = node.call("POST", path + "/claim", body={})
    ctx.expect(claim.status == 200, "owner approval permits a claim", claim)
    failure = node.call("POST", path + "/outcome", body={
        "status": "failed", "attempts": [{"state": "refused", "reason": "main base changed"}],
    })
    ctx.expect(failure.status == 200, "refused landing is recorded", failure)
    retry = request()
    ctx.expect(retry != fresh, "refused landing does not block a fresh request")
    inspected = node.call("POST", "land-requests/" + retry + "/inspect", body=evidence)
    ctx.expect(inspected.json["status"] == "needs_owner", "retries after failure need owner approval", inspected)

@case(C, "lead-conversation", "land-rejects-unknown-lane")
def _(ctx):
    n, o = ctx.node2(), owner(ctx)
    pin_lead_node(o, n)
    conv = o.call("POST", "conversations", body={
        "node": n.name, "spec": "hosted-lead", "model": "claude-code-sonnet",
    }, key=new_key("lead-land")).json["conversation"]
    result = o.call("POST", "conversations/%s/land" % conv["conversation_id"],
                    body={"repo": "unknown", "lane": "unknown"})
    ctx.expect(result.status == 404 and result.json.get("code") == "land_lane_not_found",
               "unknown lane cannot be requested", result)


@case(C, "lead-conversation", "lead-bundle-less-create-and-marker")
def _(ctx):
    """POST conversations with spec hosted-lead needs no client bundle; Director installs it; reads carry lead."""
    n, o = ctx.node2(), owner(ctx)
    pin_lead_node(o, n)
    key = new_key("lead-conv")
    body = {"node": n.name, "spec": "hosted-lead", "model": "claude-code-sonnet", "title": "hosted"}
    a = o.call("POST", "conversations", body=body, key=key)
    ctx.expect(a.status == 201, "bundle-less lead create is 201", a)
    conv = a.json["conversation"]
    ctx.expect(conv.get("lead") is True, "lead conversation reads lead: true", conv)
    ctx.expect(conv.get("spec_version") and conv["spec_version"] != "pending", "lead has its bundle at create", conv)
    ordinary = created(ctx, o, n, files=chat_bundle())
    ctx.expect(ordinary.get("lead") is False, "ordinary conversation reads lead: false", ordinary)


@case(C, "lead-conversation", "chat-spec-still-not-lead")
def _(ctx):
    """A normal uploaded spec is not a lead conversation."""
    n, o = ctx.node2(), owner(ctx)
    pin_lead_node(o, n)
    conv = created(ctx, o, n, files=chat_bundle())
    got = o.call("GET", "conversations/%s" % conv["conversation_id"]).json["conversation"]
    ctx.expect(got.get("lead") is False, "chat spec is lead: false", got)


@case(C, "lead-conversation", "lead-first-turn-dispatched")
def _(ctx):
    from cases.conversations import post, open_jobs, job_files
    from pathlib import Path
    import sys
    sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "cs/scripts"))
    import cs_lead
    n, o = ctx.node2(), owner(ctx)
    pin_lead_node(o, n)
    status = n.status([], agent_capabilities={
        "templates": ["claude-code-sonnet"], "harness": True, "harness_models": ["local-coder"],
    })
    ctx.expect(status.status == 200, "node status recorded", status)
    a = o.call("POST", "conversations", body={"node": n.name, "spec": "hosted-lead", "model": "claude-code-sonnet"}, key=new_key("lead-first"))
    ctx.expect(a.status == 201, "lead created", a)
    t = post(ctx, o, a.json["conversation"], "Hello lead")
    ctx.expect(t["state"] == "running" and t["job_id"] in open_jobs(n), "first lead turn dispatches immediately", t)
    files = job_files(n, t["job_id"])
    ctx.expect(files["spec.yaml"] == cs_lead.bundle_files("claude-code-sonnet")["spec.yaml"], "Director and Python use the same spec", files["spec.yaml"])


@case(C, "lead-conversation", "installed-lead-turn-with-capabilities")
def _(ctx):
    """A node with a capability report gets its hosted lead turn through GET work."""
    from pathlib import Path
    from shutil import copy2
    from cases.conversations import post
    from harness.run import Ctx
    from harness.targets import RustTarget

    repo = Path(__file__).resolve().parents[3]
    release = ctx.target.tmp / "release"
    (release / "bin").mkdir(parents=True)
    binary = release / "bin/cs-director"
    copy2(ctx.target.binary, binary)
    kit = release / "cs"
    kit.symlink_to(repo / "cs", target_is_directory=True)
    installed = RustTarget(release, binary)
    try:
        installed.env["CS_CODE_ROOT"] = str(kit)
        del installed.env["CS_LEAD_BUNDLE_DIR"]
        installed.restart()
        ctx = Ctx(ctx.id, target=installed)
        n, o = ctx.node2(), owner(ctx)
        pin_lead_node(o, n)
        caps = {"templates": ["claude-code-sonnet", "codex"], "harness": False, "harness_models": []}
        status = n.status([], agent_capabilities=caps)
        ctx.expect(status.status == 200, "installed node status recorded", status)
        before, _ = n.work(wait=0, timeout=3)
        created = o.call("POST", "conversations",
                         body={"spec": "hosted-lead", "model": "claude-code-sonnet"}, key=new_key("release-lead"))
        ctx.expect(created.status == 201, "lead created from installed bundle", created)
        turn = post(ctx, o, created.json["conversation"], "Call machines and give the node count.")
        offer, _ = n.work(after=before.json["cursor"], wait=2, timeout=3)
        job = o.call("GET", "jobs/" + turn["job_id"]).json
        ctx.expect(offer.status == 200 and offer.json["changed"] and any(
            (row["job_id"], row.get("turn_id")) == (turn["job_id"], turn["turn_id"])
            for row in offer.json["jobs"]
        ), "installed node sees the hosted turn offer", job.get("budget"))
    finally:
        installed.close()


@case(C, "lead-conversation", "client-bundle-may-not-claim-lead")
def _(ctx):
    """Only the Director builds a hosted-lead bundle; an uploaded spec with id hosted-lead is refused."""
    from cases.conversations import create_body, SPEC
    n, o = ctx.node2(), owner(ctx)
    pin_lead_node(o, n)
    spec = (SPEC % "dspec-model").replace("id: ", "id: hosted-lead\nx_was_id: ", 1)
    a = o.call("POST", "conversations", body=create_body(o, n, files=chat_bundle(spec=spec)), key=new_key("lead-claim"))
    ctx.expect(a.status == 422 and (a.json or {}).get("code") == "lead_bundle_reserved",
               "uploaded hosted-lead bundle is refused", a)




@case(C, "lead-conversation", "unsupported-lead-model-refused")
def _(ctx):
    """POST hosted-lead with a non-claude-code model is 422 lead_model_unsupported."""
    n, o = ctx.node2(), owner(ctx)
    pin_lead_node(o, n)
    a = o.call(
        "POST",
        "conversations",
        body={"node": n.name, "spec": "hosted-lead", "model": "codex"},
        key=new_key("lead-bad-model"),
    )
    ctx.expect(a.status == 422 and (a.json or {}).get("code") == "lead_model_unsupported",
               "unsupported lead model is refused", a)
    # A restricted template shares the claude-code prefix but has no lead tools.
    b = o.call("POST", "conversations", body={"node": n.name, "spec": "hosted-lead", "model": "claude-code-opus-judge"},
               key=new_key("lead-judge-model"))
    ctx.expect(b.status == 422 and (b.json or {}).get("code") == "lead_model_unsupported",
               "a restricted judge template cannot lead", b)


@case(C, "lead-conversation", "models-carry-lead-flag")
def _(ctx):
    """GET models rows include lead; claude-code-sonnet may host a lead."""
    o = owner(ctx)
    catalog = o.call("GET", "models").json
    models = catalog["models"]
    by_name = {m.get("name"): m for m in models}
    ctx.expect("claude-code-sonnet" in by_name, "claude-code-sonnet is listed", by_name)
    ctx.expect(by_name["claude-code-sonnet"].get("lead") is True,
               "claude-code-sonnet is a lead model", by_name["claude-code-sonnet"])
    ctx.expect(by_name.get("codex", {}).get("lead") is False,
               "codex is not a lead model", by_name.get("codex"))
    names = [m.get("name") for m in models]
    ctx.expect(len(names) == len(set(names)), "each model name is listed once", names)

@case(C, "lead-conversation", "lead-wake-turn-recorded")
def _(ctx):
    """The session host records a lane wake-up as a done turn (nudge plus reply, no job)."""
    from cases.conversations import job_files, post, thread, reply
    from cases.client import lead
    n, o = ctx.node2(), owner(ctx)
    pin_lead_node(o, n)
    conv = o.call(
        "POST",
        "conversations",
        body={"node": n.name, "spec": "hosted-lead", "model": "claude-code-sonnet"},
        key=new_key("lead-wake"),
    ).json["conversation"]
    l = lead(ctx, o, conv)
    t = post(ctx, o, conv, "Start something")
    reply(ctx, n, t["job_id"], "I will report when it finishes.")
    settled = l.call("GET", "turns/%s" % t["turn_id"]).json
    ctx.expect(settled["state"] == "done", "queue is idle before wake", settled)
    ctx.expect(o.call("POST", f"conversations/{conv['conversation_id']}/stop",
                      key=new_key()).status == 202, "hold the queue for a wake")
    queued = post(ctx, o, conv, "What finished?")
    ctx.expect(queued["state"] == "queued", "person turn waits", queued)
    body = {
        "nudge": "Lane lane-2 job %s ended: done, verdict pass" % t["job_id"],
        "reply": "Lane lane-2 finished green.",
        "jobs": [t["job_id"]],
    }
    a = l.call(
        "POST",
        "conversations/%s/wake-turns" % conv["conversation_id"],
        body=body,
        key=new_key("wake-1"),
    )
    ctx.expect(a.status == 201 and a.json["turn"]["state"] == "done", "wake turn is recorded", a)
    w = a.json["turn"]
    ctx.expect(w["reply"] == body["reply"] and w["origin"] == "wake" and w["message"] == "", "reply is stored as a wake turn", a)
    ctx.expect(w["detail"] == {"nudge": body["nudge"], "jobs": [t["job_id"]]}, "wake detail carries nudge and jobs", a)
    ctx.expect(settled.get("origin") == "person", "a posted turn is origin person", settled)
    other = o.call("POST", "conversations/%s/wake-turns" % conv["conversation_id"], body=body, key=new_key("wake-o"))
    ctx.expect(other.status == 403, "only the conversation's lead records wake turns", other)
    th = thread(l, conv)
    ctx.expect([turn["turn_id"] for turn in th["turns"]] == [t["turn_id"], w["turn_id"], queued["turn_id"]],
               "wake precedes the waiting person turn in the thread", th)
    ctx.expect(o.call("POST", f"conversations/{conv['conversation_id']}/resume",
                      key=new_key()).status == 200, "resume the queue")
    running = l.call("GET", f"turns/{queued['turn_id']}").json
    ctx.expect(running["state"] == "running", "waiting turn starts", running)
    context = job_files(n, running["job_id"])["conversation/context.md"]
    ctx.expect(body["reply"].encode() in context, "waiting turn receives the wake reply", context)

@case(C, "lead-conversation", "lead-wake-turn-jobs-scope")
def _(ctx):
    """Each job named in wake-turn jobs must be visible to the lead credential."""
    from cases.client import lead, submitted, accept, finish, refused
    n, o = ctx.node2(), owner(ctx)
    pin_lead_node(o, n)
    attached = submitted(ctx, o, n)
    foreign = submitted(ctx, o, n)["jobs"][0]["job_id"]
    accept(ctx, n, foreign)
    finish(ctx, n, foreign)
    conv = o.call(
        "POST",
        "conversations",
        body={
            "node": n.name,
            "spec": "hosted-lead",
            "model": "claude-code-sonnet",
            "intent_id": attached["intent"]["intent_id"],
        },
        key=new_key("lead-wake-scope"),
    ).json["conversation"]
    l = lead(ctx, o, conv, attached["intent"]["intent_id"])
    own = attached["jobs"][0]["job_id"]
    ok = l.call(
        "POST",
        "conversations/%s/wake-turns" % conv["conversation_id"],
        body={"nudge": "nudge", "reply": "reply", "jobs": [own]},
        key=new_key("wake-ok"),
    )
    ctx.expect(ok.status == 201, "visible job is accepted", ok)
    bad = l.call(
        "POST",
        "conversations/%s/wake-turns" % conv["conversation_id"],
        body={"nudge": "nudge", "reply": "reply", "jobs": [foreign]},
        key=new_key("wake-bad"),
    )
    refused(ctx, bad, 422, "invalid_request", "foreign job in wake-turn jobs")

