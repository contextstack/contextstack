"""Hosted-lead conversations: Director-bundled create, lead marker on reads."""
from harness.client import new_key
from harness.run import case

from cases.client import owner
from cases.conversations import bundle as chat_bundle, created


def pin_lead_node(o, node):
    o.call("PUT", "lead-node", body={"node": node.name})


C = "client"
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
    models = o.call("GET", "models").json.get("models") or []
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

