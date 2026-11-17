"""Client side, step 7 (CLIENT-API.md §12; outline §11): usage added up per run, intent, conversation, project and
source; project and conversation budgets that reserve before a job is offered and settle on its actual charge; and
source budgets seen and set through the Director. Stand-in nodes play the node and a stand-in gateway
(harness/fake_gateway.py) plays the gateway; nothing calls a model or reads a real key or token."""
import json
import threading
import time

from harness import wire
from harness.client import new_key
from harness.run import case
from harness.wire import run_fixture, sha

from cases.client import device, owner, refused
from cases import conversations as conv

C = "client"


def spec(usd, model="fake"):
    return wire.SPEC.replace("usd: 0.25", "usd: %s" % usd).replace("model: fake", "model: %s" % model)


def files_with(c, spec_text):
    files = dict(wire.JOB_FILES)
    files["spec.yaml"] = (spec_text.encode(), 0o644)
    for data, _ in files.values():
        a = c.call("PUT", "objects/" + sha(data), raw=data)
        if a.status != 200:
            raise RuntimeError("PUT objects: %r" % a)
    return [{"path": p, "sha256": sha(d), "mode": m} for p, (d, m) in sorted(files.items())]


def project(ctx, c, name="p", budget=None):
    body = {"name": name}
    if budget is not None:
        body["budget"] = budget
    a = c.call("POST", "projects", body=body, key=new_key("project"))
    ctx.expect(a.status == 201 and a.json["project"]["name"] == name, "POST projects is 201", a)
    return a.json["project"]


def submit(ctx, c, node, project_id=None, usd=1, model="fake", key=None):
    body = {"node": node.name, "spec": "spec.yaml", "task": "tasks/t", "files": files_with(c, spec(usd, model))}
    if project_id:
        body["project_id"] = project_id
    a = c.call("POST", "intents", body=body, key=key or new_key("intent"))
    ctx.expect(a.status == 201, "POST intents is 201", a)
    return a.json["jobs"][0]


def job(c, job_id):
    return c.call("GET", "jobs/" + job_id).json


def budget_of(c, project_id):
    return c.call("GET", "projects/" + project_id).json["project"]["budget"]


def offered_spec(node, job_id):
    f = next(f for f in node.job(job_id)["files"] if f["path"] == "spec.yaml")
    return node.call("GET", "objects/" + f["sha256"]).body.decode()


def offered_usd(node, job_id):
    """The budget.usd in the spec the node is offered."""
    f = next(f for f in node.job(job_id)["files"] if f["path"] == "spec.yaml")
    text = node.call("GET", "objects/" + f["sha256"]).body.decode()
    line = next(l for l in text.splitlines() if "usd" in l)
    return float(line.split("usd:")[1].strip().rstrip("}").split(",")[0])


def report_run(ctx, node, run_id, charged=0, cls="api", model="dspec-api", job_id=None, **usage):
    """The node reports a finalized run with this usage; with `job_id` the envelope names the run's job."""
    run = run_fixture(run_id)
    run["envelope"]["usage"] = wire.usage(charged_usd=charged, est_usd=9.99, source={"class": cls, "model": model},
                                          **usage)
    if job_id:
        run["envelope"]["job_id"] = job_id
    for data in run["files"].values():
        ctx.expect(node.call("PUT", "objects/" + sha(data), raw=data).status == 200, "PUT objects")
    a = node.call("POST", "envelopes", body={"envelope": run["envelope"], "row_line": run["row_line"]})
    ctx.expect((a.json or {}).get("status") == "acked", "run_finalized acked", a)
    return a


def end_job(ctx, node, job_id, run_id=None, state="done"):
    """The node reports the job over, naming its run (or none)."""
    row = json.dumps({"state": state, "director_job": job_id, "run_id": run_id})
    term = {"envelope_version": 1, "event_id": "job_terminal:" + job_id, "job_id": job_id, "files": [],
            "envelope_sha256": sha(b""), "row_sha256": sha(row)}
    a = node.call("POST", "envelopes", body={"envelope": term, "row_line": row})
    ctx.expect((a.json or {}).get("status") == "acked", "job_terminal acked", a)


def accept(node, job_id):
    node.call("POST", "jobs/%s/accept" % job_id, body={"bundle_sha256": node.job(job_id)["bundle_sha256"]})


def finish(ctx, node, job_id, charged=0, cls="api", model="dspec-api", state="done", tag="run", **usage):
    """The node accepts the job, reports a run with this usage, then the job's end."""
    accept(node, job_id)
    run_id = "%s-%s-%s" % (ctx.serial_tag(), tag, job_id[-6:].lower())
    report_run(ctx, node, run_id, charged, cls, model, **usage)
    end_job(ctx, node, job_id, run_id, state)
    return run_id


def until(check, seconds=15):
    """Poll `check` until it answers something true; that answer, or None after `seconds`."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        got = check()
        if got:
            return got
        time.sleep(0.25)
    return None


def open_jobs(node):
    return [j["job_id"] for j in node.call("GET", "jobs").json["jobs"]]


@case(C, "budgets", "usage-rollups-per-level")
def _(ctx):
    """Usage adds up per intent, conversation, project and source, over a day and a month. Each source is shown for
    what it is: pay as you go with dollars charged, a subscription with its reported allowance, local with no cost.
    The API-price estimate a run reports is never shown."""
    n, c = ctx.node2(), owner(ctx)
    p = project(ctx, c, "usage")
    tag = ctx.serial_tag()
    api_model, sub_model = "%s-api" % tag, "%s-sub" % tag
    j1 = submit(ctx, c, n, p["project_id"], usd=1)
    finish(ctx, n, j1["job_id"], charged=0.4, model=api_model, tag="api", cache_read_tokens=50)
    j2 = submit(ctx, c, n, p["project_id"], usd=1)
    allowance = {"via": "rate_limit_event", "rejected": True,
                 "windows": [{"window": "five_hour", "used_pct": 100, "resets_at": "x", "status": "rejected"}]}
    finish(ctx, n, j2["job_id"], charged=0, cls="subscription", model=sub_model, tag="sub", allowance=allowance)
    cv = conv.created(ctx, c, n, project_id=p["project_id"])
    t = conv.post(ctx, c, cv, "hello")
    conv.reply(ctx, n, t["job_id"], "hi")  # a local run (the fixture's usage)
    u = c.call("GET", "usage?by=project&period=day&project=" + p["project_id"])
    ctx.expect(u.status == 200 and u.json["period"] == "day" and len(u.json["groups"]) == 1, "GET usage by project", u)
    g = u.json["groups"][0]
    ctx.expect(g["project_id"] == p["project_id"] and g["runs"] == 3 and g["charged_usd"] == 0.4
               and set(g["kinds"]) == {"pay_as_you_go", "subscription", "local"}, "the project's day", g)
    ctx.expect("est_usd" not in json.dumps(u.json), "no estimate is shown", u.json)
    month = c.call("GET", "usage?by=project&period=month&project=" + p["project_id"]).json
    ctx.expect(month["groups"][0]["runs"] == 3 and month["period"] == "month", "the month holds the day", month)
    by_conv = c.call("GET", "usage?by=conversation&period=day&conversation=" + cv["conversation_id"]).json
    ctx.expect([x["runs"] for x in by_conv["groups"]] == [1] and "local" in by_conv["groups"][0]["kinds"],
               "per conversation", by_conv)
    by_intent = c.call("GET", "usage?by=intent&period=day&project=" + p["project_id"]).json
    ctx.expect(len([x for x in by_intent["groups"] if x["intent_id"]]) == 2, "per intent", by_intent)
    src = c.call("GET", "usage?by=source&period=day&project=" + p["project_id"]).json["groups"]
    api = next(s for s in src if s["source"]["model"] == api_model)
    sub = next(s for s in src if s["source"]["model"] == sub_model)
    local = next(s for s in src if s["kind"] == "local")
    ctx.expect(api["kind"] == "pay_as_you_go" and api["charged_usd"] == 0.4 and api["cache_read_tokens"] == 50
               and api["shown"] == "pay as you go: $0.40 charged", "pay as you go: dollars charged", api)
    ctx.expect(sub["kind"] == "subscription" and "charged_usd" not in sub and sub["allowance_reached"] is True
               and sub["shown"] == "subscription: allowance reached", "a subscription: allowance, no dollars", sub)
    ctx.expect("charged_usd" not in local and local["shown"] == "local: usage only, no cost", "local: no cost", local)
    bad = c.call("GET", "usage?by=planet")
    refused(ctx, bad, 422, "invalid_request", "an unknown grouping")


@case(C, "budgets", "budget-absent-vs-exhausted")
def _(ctx):
    """'No budget set' and 'budget used up' look different. A project without a budget shows none; one whose budget
    is spent shows exhausted, and new work under it waits saying so. Raising the budget lets it start."""
    n, c = ctx.node2(), owner(ctx)
    free = project(ctx, c, "free")
    ctx.expect(free["budget"]["state"] == "none" and free["budget"]["shown"] == "no budget set"
               and free["budget"]["usd"] is None, "absent: no budget set", free)
    j = submit(ctx, c, n, free["project_id"], usd=1)
    ctx.expect(j["state"] == "offered" and j["budget"]["state"] == "free", "no budget: offered as written", j)
    p = project(ctx, c, "capped", budget={"usd": 1})
    ctx.expect(p["budget"]["state"] == "active" and p["budget"]["usd"] == 1.0 and p["budget"]["period"] == "total",
               "a budget set at creation", p)
    j1 = submit(ctx, c, n, p["project_id"], usd=1)
    finish(ctx, n, j1["job_id"], charged=1.0)
    b = budget_of(c, p["project_id"])
    ctx.expect(b["state"] == "exhausted" and b["shown"] == "budget used up" and b["spent_usd"] == 1.0,
               "spent: budget used up", b)
    j2 = submit(ctx, c, n, p["project_id"], usd=1)
    ctx.expect(j2["state"] == "waiting" and j2["budget"]["reason"] == "budget_exhausted"
               and j2["budget"]["detail"].startswith("budget used up") and j2["job_id"] not in open_jobs(n),
               "new work waits, saying the budget is used up", j2)
    a = c.call("PUT", "projects/%s/budget" % p["project_id"], body={"usd": "2.00", "period": "month"})
    ctx.expect(a.status == 200 and a.json["project"]["budget"]["period"] == "month", "PUT budget", a)
    j2 = job(c, j2["job_id"])
    ctx.expect(j2["state"] == "offered" and j2["budget"]["reserved_usd"] == 1.0 and j2["job_id"] in open_jobs(n),
               "raising the budget offers it", j2)
    refused(ctx, c.call("PUT", "projects/%s/budget" % p["project_id"], body={"usd": -1}), 422, "invalid_request",
            "a negative budget")
    refused(ctx, c.call("PUT", "projects/prj_nope/budget", body={"usd": 1}), 404, "not_found", "an unknown project")
    gone = c.call("DELETE", "projects/%s/budget" % p["project_id"])
    ctx.expect(gone.status == 200 and gone.json["project"]["budget"]["state"] == "none", "DELETE budget", gone)


@case(C, "budgets", "small-amounts-shown-honestly")
def _(ctx):
    """A small budget with a little spent is not shown as untouched: $0.02 with $0.001542 spent is '$0.0184 of
    $0.02 left', not '$0.02 of $0.02 left'. A source budget of $0.20 with $0.0017 spent is '$0.198 left'."""
    n, c = ctx.node2(), owner(ctx)
    p = project(ctx, c, "small", budget={"usd": "0.02"})
    ctx.expect(p["budget"]["shown"] == "$0.02 of $0.02 left", "nothing spent: whole cents", p["budget"])
    j = submit(ctx, c, n, p["project_id"], usd=1)
    finish(ctx, n, j["job_id"], charged=0.001542)
    b = budget_of(c, p["project_id"])
    ctx.expect(b["state"] == "active" and b["spent_usd"] == 0.001542 and b["shown"] == "$0.0184 of $0.02 left",
               "a little spent shows in the room left", b)
    g = ctx.target.gateway
    try:
        c.call("PUT", "source-budgets", body={"sources": {"openai": {"usd": 0.2, "period": "day"}}})
        g.spent["openai"] = 0.0017
        rows = {r["source"]: r for r in c.call("GET", "source-budgets").json["budgets"]["sources"]}
        ctx.expect(rows["openai"]["shown"] == "active: $0.198 left", "a source's small spend shows", rows["openai"])
    finally:  # the stand-in gateway is shared by the cases that follow
        g.limits, g.spent = {"sources": {}}, {}


@case(C, "budgets", "two-jobs-room-for-one")
def _(ctx):
    """Two jobs started together under a project with room for one: one is offered with the reservation in its spec,
    the other is created but waits, with a reason the client can show, and its node is not offered it."""
    n, c = ctx.node2(), owner(ctx)
    p = project(ctx, c, "room", budget={"usd": 5})
    jobs, errors = [], []

    def go():
        try:
            jobs.append(submit(ctx, c, n, p["project_id"], usd=5))
        except Exception as e:  # noqa: BLE001
            errors.append(e)

    threads = [threading.Thread(target=go) for _ in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    ctx.expect(not errors and len(jobs) == 2, "both submitted", errors)
    jobs = [job(c, j["job_id"]) for j in jobs]
    states = sorted(j["state"] for j in jobs)
    ctx.expect(states == ["offered", "waiting"], "one offered, one waiting", states)
    first = next(j for j in jobs if j["state"] == "offered")
    second = next(j for j in jobs if j["state"] == "waiting")
    ctx.expect(first["budget"]["state"] == "held" and first["budget"]["reserved_usd"] == 5.0
               and offered_usd(n, first["job_id"]) == 5.0, "the first reserves $5, and its spec says so", first)
    kept = "".join(l for l in offered_spec(n, first["job_id"]).splitlines(keepends=True) if not l.startswith("intent:"))
    ctx.expect(kept == spec(5).replace("usd: 5,", "usd: 5.0,"),
               "the reservation edits only budget.usd, in the spec's inline style", kept)
    ctx.expect(second["budget"]["reason"] == "budget_reserved" and "reserved" in second["budget"]["detail"]
               and second["job_id"] not in open_jobs(n), "the second waits, and says why", second)
    b = budget_of(c, p["project_id"])
    ctx.expect(b["state"] == "reserved" and b["reserved_usd"] == 5.0 and b["waiting_job_ids"] == [second["job_id"]],
               "the project shows its room spoken for", b)


@case(C, "budgets", "settle-releases")
def _(ctx):
    """When the run reports its actual charge, the reservation settles and the difference is released to the next
    waiting job. A job that ends with no run costs $0."""
    n, c = ctx.node2(), owner(ctx)
    p = project(ctx, c, "settle", budget={"usd": 5})
    a = submit(ctx, c, n, p["project_id"], usd=5)
    b = submit(ctx, c, n, p["project_id"], usd=5)
    ctx.expect(b["state"] == "waiting", "the second waits", b)
    finish(ctx, n, a["job_id"], charged=1.25)
    a = job(c, a["job_id"])
    ctx.expect(a["budget"]["state"] == "settled" and a["budget"]["charged_usd"] == 1.25
               and a["budget"]["released_usd"] == 3.75 and a["budget"]["charge_basis"] == "reported",
               "settled on the actual charge", a["budget"])
    b = job(c, b["job_id"])
    ctx.expect(b["state"] == "offered" and b["budget"]["reserved_usd"] == 3.75 and offered_usd(n, b["job_id"]) == 3.75,
               "the release goes to the waiting job", b)
    budget = budget_of(c, p["project_id"])
    ctx.expect(budget["spent_usd"] == 1.25 and budget["reserved_usd"] == 3.75 and budget["remaining_usd"] == 0,
               "the project's figures", budget)
    # An error with no run: the node reports the job over and nothing was charged.
    n.call("POST", "jobs/%s/accept" % b["job_id"], body={"bundle_sha256": n.job(b["job_id"])["bundle_sha256"]})
    term = wire.terminal_fixture(b["job_id"], state="failed")
    ctx.expect(n.call("POST", "envelopes", body=term).json["status"] == "acked", "job_terminal acked")
    b = job(c, b["job_id"])
    ctx.expect(b["budget"]["charged_usd"] == 0 and b["budget"]["charge_basis"] == "no_run", "an error costs $0", b)
    budget = budget_of(c, p["project_id"])
    ctx.expect(budget["state"] == "active" and budget["remaining_usd"] == 3.75, "released in full", budget)


@case(C, "budgets", "restart-keeps-reservations")
def _(ctx):
    """Reservations survive a Director restart with jobs in flight: after it, the held amounts, the waiting job and
    its reason are as they were, and settling still releases to it."""
    n, c = ctx.node2(), owner(ctx)
    p = project(ctx, c, "restart", budget={"usd": 3})
    j1 = submit(ctx, c, n, p["project_id"], usd=2)
    j2 = submit(ctx, c, n, p["project_id"], usd=2)
    j3 = submit(ctx, c, n, p["project_id"], usd=2)
    ctx.expect([j["budget"]["reserved_usd"] for j in (j1, j2)] == [2.0, 1.0] and j3["state"] == "waiting",
               "$2, then the $1 left, then a wait", [j1["budget"], j2["budget"], j3["budget"]])
    n.call("POST", "jobs/%s/accept" % j1["job_id"], body={"bundle_sha256": n.job(j1["job_id"])["bundle_sha256"]})
    before = [job(c, j["job_id"]) for j in (j1, j2, j3)]
    ctx.target.restart()
    after = [job(c, j["job_id"]) for j in (j1, j2, j3)]
    keep = lambda j: (j["state"], j["budget"]["state"], j["budget"]["reserved_usd"], j["budget"]["reason"])  # noqa: E731
    ctx.expect([keep(j) for j in after] == [keep(j) for j in before], "the same after a restart",
               [keep(j) for j in after])
    b = budget_of(c, p["project_id"])
    ctx.expect(b["reserved_usd"] == 3.0 and b["waiting_job_ids"] == [j3["job_id"]], "the project's reservations", b)
    finish(ctx, n, j1["job_id"], charged=0.5)
    j3 = job(c, j3["job_id"])
    ctx.expect(j3["state"] == "offered" and j3["budget"]["reserved_usd"] == 1.5, "settling still releases", j3)


@case(C, "budgets", "conversation-budget-and-cancel-waiting")
def _(ctx):
    """A conversation's budget applies to its turns. With nothing left the turn's job waits; stopping the
    conversation ends it at once, without the node ever seeing it, and it costs nothing."""
    n, c = ctx.node2(), owner(ctx)
    cv = conv.created(ctx, c, n)
    ctx.expect(cv["budget"]["state"] == "none", "a conversation shows its budget", cv)
    a = c.call("PUT", "conversations/%s/budget" % cv["conversation_id"], body={"usd": 0})
    ctx.expect(a.status == 200 and a.json["conversation"]["budget"]["state"] == "exhausted", "a $0 budget", a)
    t = conv.post(ctx, c, cv, "hello")
    j = job(c, t["job_id"])
    ctx.expect(j["state"] == "waiting" and j["budget"]["reason"] == "budget_exhausted"
               and j["budget"]["conversation_id"] == cv["conversation_id"] and t["job_id"] not in open_jobs(n),
               "the turn's job waits", j)
    s = c.call("POST", "conversations/%s/stop" % cv["conversation_id"], key=new_key("stop"))
    ctx.expect(s.status == 202 and s.json["cancel"]["state"] == "applied" and s.json["cancel"]["outcome"] == "stopped",
               "stop ends the waiting job at once", s)
    j = job(c, t["job_id"])
    ctx.expect(j["state"] == "error" and j["budget"]["state"] == "settled" and j["budget"]["charged_usd"] == 0,
               "it never ran and cost nothing", j)
    ctx.expect(conv.turn(c, t["turn_id"])["state"] == "stopped", "the turn is stopped")
    ctx.expect(not n.control().json.get("control"), "the node is never asked", n.control().json)


@case(C, "budgets", "source-budgets-through-the-director")
def _(ctx):
    """Source budgets: the gateway's sources are shown labelled ('off, set a budget to use' is not 'budget used up');
    only the owner may set them, which the Director does with the gateway's owner token from its file, never
    logging it."""
    c, phone = owner(ctx), device(ctx)
    g = ctx.target.gateway
    a = phone.call("GET", "source-budgets")
    ctx.expect(a.status == 200, "any client reads source budgets", a)
    rows = {r["source"]: r for r in a.json["budgets"]["sources"]}
    ctx.expect(rows["openai"]["state"] == "off" and rows["openai"]["shown"] == "off, set a budget to use",
               "a source with no budget is off", rows)
    puts = len([s for s in g.seen if s[0] == "PUT"])
    refused(ctx, phone.call("PUT", "source-budgets", body={"sources": {"openai": {"usd": 5}}}), 403, "owner_only",
            "a device setting a source budget")
    ctx.expect(len([s for s in g.seen if s[0] == "PUT"]) == puts, "nothing reached the gateway")
    a = c.call("PUT", "source-budgets", body={"sources": {"openai": {"usd": 5, "period": "day"}}})
    ctx.expect(a.status == 200, "the owner sets a source budget", a)
    rows = {r["source"]: r for r in a.json["budgets"]["sources"]}
    ctx.expect(rows["openai"]["state"] == "active" and rows["openai"]["shown"] == "active: $5.00 left", "active", rows)
    ctx.expect(g.seen[-1] == ("PUT", g.owner_token), "the gateway got its owner token", g.seen[-1][0])
    ctx.expect(all(t is None for m, t in g.seen if m == "GET"), "a read never carries the token")
    g.spent["openai"] = 5.0
    rows = {r["source"]: r for r in c.call("GET", "source-budgets").json["budgets"]["sources"]}
    ctx.expect(rows["openai"]["state"] == "exhausted" and rows["openai"]["shown"].startswith("budget used up"),
               "a spent source budget is used up, not off", rows["openai"])
    refused(ctx, c.call("PUT", "source-budgets", raw=b"[1]"), 422, "invalid_request", "a document the gateway refuses")
    ctx.target.log.flush()
    log = (ctx.target.tmp / "server.log").read_bytes()
    ctx.expect(g.owner_token.encode() not in log, "the token is never logged")


@case(C, "budgets", "reservation-terminal-without-run")
def _(ctx):
    """A job that ends naming a run whose report never comes no longer holds its reservation: it closes at once,
    counting the whole reservation (what it spent is unknown), so nothing is freed on a guess. A job that ends with
    no run at all costs $0."""
    n, c = ctx.node2(), owner(ctx)
    p = project(ctx, c, "terminal", budget={"usd": 4})
    a = submit(ctx, c, n, p["project_id"], usd=2)
    accept(n, a["job_id"])
    end_job(ctx, n, a["job_id"], run_id="%s-never" % ctx.serial_tag())
    a = job(c, a["job_id"])
    ctx.expect(a["budget"]["state"] == "settled" and a["budget"]["charge_basis"] == "presumed"
               and a["budget"]["settled_by"] == "terminal" and a["budget"]["charged_usd"] == 2.0
               and a["budget"]["released_usd"] == 0, "closed on its end, counted in full", a["budget"])
    b = budget_of(c, p["project_id"])
    ctx.expect(b["reserved_usd"] == 0 and b["spent_usd"] == 2.0 and b["remaining_usd"] == 2.0,
               "nothing is held for it any more", b)
    z = submit(ctx, c, n, p["project_id"], usd=1)
    accept(n, z["job_id"])
    end_job(ctx, n, z["job_id"], state="failed")
    z = job(c, z["job_id"])
    ctx.expect(z["budget"]["charge_basis"] == "no_run" and z["budget"]["charged_usd"] == 0
               and z["budget"]["settled_by"] == "ended", "an end with no run costs $0", z["budget"])


@case(C, "budgets", "reservation-stale-node-and-late-report")
def _(ctx):
    """A node silent past --reservation-stale-after has its jobs' reservations closed, counted in full. When its
    report arrives late (naming the job, as each trial's run does), the real charge counts instead and the rest is
    released once, to the waiting job; a repeat changes nothing."""
    c = owner(ctx)
    ctx.target.restart(["--reservation-stale-after", "1"])
    try:
        n = ctx.node2()
        p = project(ctx, c, "stale", budget={"usd": 3})
        a = submit(ctx, c, n, p["project_id"], usd=3)
        w = submit(ctx, c, n, p["project_id"], usd=3)
        ctx.expect(w["state"] == "waiting", "the second waits", w)
        accept(n, a["job_id"])
        closed = until(lambda: (lambda j: j if j["budget"]["state"] == "settled" else None)(job(c, a["job_id"])))
        ctx.expect(closed and closed["budget"]["settled_by"] == "node_stale"
                   and closed["budget"]["charge_basis"] == "presumed" and closed["budget"]["charged_usd"] == 3.0,
                   "a stale node's reservation closes, counted in full", closed and closed["budget"])
        ctx.expect(job(c, w["job_id"])["state"] == "waiting", "nothing is freed on a guess")
        run_id = "%s-late" % ctx.serial_tag()
        report_run(ctx, n, run_id, charged=0.5, job_id=a["job_id"])
        late = job(c, a["job_id"])
        ctx.expect(late["budget"]["charged_usd"] == 3.0 and late["budget"]["charge_basis"] == "presumed"
                   and run_id in late["run_ids"], "a report while the job runs on: linked, still presumed", late)
        end_job(ctx, n, a["job_id"], run_id=run_id)
        late = job(c, a["job_id"])
        ctx.expect(late["budget"]["charged_usd"] == 0.5 and late["budget"]["charge_basis"] == "reported"
                   and late["budget"]["released_usd"] == 2.5 and late["budget"]["corrected_at"],
                   "the late report counts the real charge", late["budget"])
        ctx.expect(c.call("GET", "runs/" + run_id).json["job_id"] == a["job_id"], "the run names its job")
        w = job(c, w["job_id"])
        ctx.expect(w["state"] == "offered" and w["budget"]["reserved_usd"] == 2.5, "released once, to the waiting job", w)
        report_run(ctx, n, run_id, charged=0.5, job_id=a["job_id"])  # the same envelope again
        b = budget_of(c, p["project_id"])
        ctx.expect(b["spent_usd"] == 0.5 and b["reserved_usd"] == 2.5 and b["remaining_usd"] == 0,
                   "no double release", b)
    finally:
        ctx.target.restart()


@case(C, "budgets", "reservation-timeout")
def _(ctx):
    """The last resort: a reservation older than --reservation-timeout closes though its node is heard from, counted
    in full; the run's report, when it comes, settles it on the real charge."""
    c = owner(ctx)
    ctx.target.restart(["--reservation-timeout", "2"])
    try:
        n = ctx.node2()
        p = project(ctx, c, "timeout", budget={"usd": 2})
        a = submit(ctx, c, n, p["project_id"], usd=2)
        accept(n, a["job_id"])

        def closed():
            n.status([{"job_id": a["job_id"], "run_id": None, "state": "running", "started_at": None}])
            j = job(c, a["job_id"])
            return j if j["budget"]["state"] == "settled" else None

        j = until(closed)
        ctx.expect(j and j["budget"]["settled_by"] == "timeout" and j["budget"]["charged_usd"] == 2.0,
                   "closed by the timeout, counted in full", j and j["budget"])
        run_id = "%s-run" % ctx.serial_tag()
        report_run(ctx, n, run_id, charged=0.25)
        end_job(ctx, n, a["job_id"], run_id=run_id)
        j = job(c, a["job_id"])
        ctx.expect(j["budget"]["charged_usd"] == 0.25 and j["budget"]["released_usd"] == 1.75,
                   "settled on the real charge", j["budget"])
    finally:
        ctx.target.restart()


@case(C, "budgets", "reservation-cancel")
def _(ctx):
    """A cancel the node applies closes the reservation: a job it never started ends and costs $0; one it stopped
    while running is counted in full until its report says otherwise."""
    n, c = ctx.node2(), owner(ctx)
    p = project(ctx, c, "cancel", budget={"usd": 2})
    a = submit(ctx, c, n, p["project_id"], usd=1)
    b = submit(ctx, c, n, p["project_id"], usd=1)
    accept(n, b["job_id"])
    for j in (a, b):
        r = c.call("POST", "jobs/%s/cancel" % j["job_id"], key=new_key("cancel"))
        ctx.expect(r.status == 202, "cancel requested", r)
    for m in n.control().json["control"]:
        n.report(m["control_id"], "applied", outcome="stopped")
    a, b = job(c, a["job_id"]), job(c, b["job_id"])
    ctx.expect(a["state"] == "error" and a["budget"]["charged_usd"] == 0 and a["budget"]["settled_by"] == "ended",
               "never started: over, $0", a)
    ctx.expect(b["budget"]["charge_basis"] == "presumed" and b["budget"]["settled_by"] == "cancel"
               and b["budget"]["charged_usd"] == 1.0, "stopped while running: counted in full", b["budget"])


@case(C, "budgets", "source-budgets-per-source")
def _(ctx):
    """One source's budget is set or turned off without sending the whole limits.json: the Director reads the
    gateway's budgets, changes the one entry and writes them back, keeping every other entry. Owner only. A change
    made by another writer between the Director's reads is kept, not overwritten."""
    c, phone = owner(ctx), device(ctx)
    g = ctx.target.gateway
    g.limits, g.spent = {"sources": {"anthropic": {"usd": 3, "period": "month"}}}, {}
    a = c.call("PUT", "source-budgets/openai", body={"usd": "5", "period": "day"})
    ctx.expect(a.status == 200, "PUT one source", a)
    ctx.expect(g.limits["sources"] == {"anthropic": {"usd": 3.0, "period": "month"},
                                       "openai": {"usd": 5.0, "period": "day"}}, "the others are kept", g.limits)
    rows = {r["source"]: r for r in a.json["budgets"]["sources"]}
    ctx.expect(rows["openai"]["state"] == "active", "answered with the new report", rows)
    refused(ctx, phone.call("PUT", "source-budgets/openai", body={"usd": 1, "period": "day"}), 403, "owner_only",
            "a device")
    refused(ctx, phone.call("DELETE", "source-budgets/openai"), 403, "owner_only", "a device turning one off")
    refused(ctx, c.call("PUT", "source-budgets/openai", body={"usd": 1, "period": "week"}), 422, "invalid_request",
            "a period the gateway does not have")
    refused(ctx, c.call("PUT", "source-budgets/a%20b", body={"usd": 1, "period": "day"}), 422, "invalid_request",
            "a malformed source")
    # The shared budget, then a source turned off under it: it gets $0, so the shared budget does not cover it.
    a = c.call("PUT", "source-budgets/%2A", body={"usd": 10, "period": "month"})
    ctx.expect(a.status == 200 and a.json["budgets"]["total"]["usd"] == 10, "the shared budget", a)
    a = c.call("DELETE", "source-budgets/anthropic")
    rows = {r["source"]: r for r in a.json["budgets"]["sources"]}
    ctx.expect(a.status == 200 and rows["anthropic"]["state"] == "off"
               and g.limits["sources"]["anthropic"]["usd"] == 0, "off under a shared budget", g.limits)
    ctx.expect(rows["openrouter"]["state"] == "active", "the shared budget still covers the rest", rows)
    a = c.call("DELETE", "source-budgets/*")
    ctx.expect(a.status == 200 and a.json["budgets"]["total"] is None
               and g.limits["sources"] == {"openai": {"usd": 5.0, "period": "day"}}, "no shared budget", g.limits)
    # Another writer adds a source between the Director's two reads: the Director reads again and keeps it.
    raced = []

    def other_writer():
        if not raced:
            raced.append(1)
            g.limits = {"sources": dict(g.limits["sources"], openrouter={"usd": 1.0, "period": "day"})}

    g.after_get = other_writer
    try:
        a = c.call("PUT", "source-budgets/anthropic", body={"usd": 2, "period": "day"})
    finally:
        g.after_get = None
    ctx.expect(a.status == 200 and raced and set(g.limits["sources"]) == {"openai", "openrouter", "anthropic"},
               "the other writer's change is kept", g.limits)
    ctx.expect(g.seen[-1] == ("PUT", g.owner_token), "written with the owner token", g.seen[-1][0])
