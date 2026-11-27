"""Client side, step 5 (CLIENT-API.md §4.3, §4.4, §5.1, §6.2, §6.3, §8.1): what a client sees of control messages,
live output, node status and schedules, over HTTP and the event stream. A protocol-2 stand-in node plays the node."""
from harness.client import new_key
from harness.node2 import delta, schedule, stop
from harness.run import case

from cases.client import finish, owner, refused, submitted

C = "client"


def changes(s, kind, ident, until, timeout=5):
    """Change events for one record until until(record) holds; returns the records seen, in order."""
    seen = []
    e, _ = s.until(lambda name, d: name == "change" and d["kind"] == kind and d["id"] == ident
                   and (seen.append(d["record"]) or until(d["record"])), timeout)
    return seen if e else None


@case(C, "live", "live-output-on-stream")
def _(ctx):
    """A run's live events reach the stream as run_event in seq order before the run is reported; the job lists
    the live run; GET runs/{id}/events catches up; once reported, the run says how many events it sent."""
    n, c = ctx.node2(), owner(ctx)
    job = submitted(ctx, c, n)["jobs"][0]
    n.accept(job["job_id"])
    s = c.stream()
    try:
        s.until(lambda name, _d: name == "hello")
        run = "%s-run" % ctx.serial_tag()  # the run id finish() reports below
        n.events(run, job["job_id"], [delta(1, "Hel"), delta(2, "lo")])
        n.events(run, job["job_id"], [delta(3, "!"), stop(4)])
        got, listed = [], False
        while len(got) < 4:
            ev = s.next()
            ctx.expect(ev is not None, "the stream carries the events", got)
            if ev[0] == "change" and ev[1]["kind"] == "job" and run in ev[1]["record"]["live_run_ids"]:
                listed = True
            if ev[0] == "run_event" and ev[1]["run_id"] == run:
                ctx.expect(ev[2] is None, "run_event has no id line", ev)
                got.append(ev[1])
        ctx.expect(listed, "the job lists the live run")
        ctx.expect([g["seq"] for g in got] == [1, 2, 3, 4] and got[0]["payload"] == {"text": "Hel"}
                   and got[3]["kind"] == "stop" and all(g["partial"] and g["job_id"] == job["job_id"]
                                                         and g["node"] == n.name for g in got), "in seq order", got)
        caught = c.call("GET", "runs/%s/events?after=2" % run).json
        ctx.expect([x["seq"] for x in caught["events"]] == [3, 4] and caught["finished"] is False
                   and caught["confirmed"] == 4, "catch up from a seq", caught)
    finally:
        s.close()
    refused(ctx, c.call("GET", "runs/no-such-run/events"), 404, "not_found", "unknown run")
    refused(ctx, c.call("GET", "runs/%s/events?after=x" % run), 422, "invalid_request", "after=x")
    finish(ctx, n, job["job_id"])
    after = c.call("GET", "runs/%s/events" % run).json
    ctx.expect(after["finished"] is True, "a reported run is finished", after)


@case(C, "live", "live-output-replaced-by-result")
def _(ctx):
    """When the run that sent live events is reported, its run record carries the count and its events say finished."""
    n, c = ctx.node2(), owner(ctx)
    job = submitted(ctx, c, n)["jobs"][0]
    n.accept(job["job_id"])
    run_id = "%s-run" % ctx.serial_tag()  # the id finish() reports
    n.events(run_id, job["job_id"], [delta(1, "partial"), stop(2)])
    finish(ctx, n, job["job_id"])
    run = c.call("GET", "runs/" + run_id).json
    ctx.expect(run.get("events") == 2 and run.get("job_id") == job["job_id"], "the run counts its live events", run)
    ev = c.call("GET", "runs/%s/events" % run_id).json
    ctx.expect(ev["finished"] is True and ev["confirmed"] == 2, "finished: the result replaces the partial output", ev)


@case(C, "control", "cancel-states-on-stream")
def _(ctx):
    """Each step of a cancel is a new revision of the cancel and its job on the stream: requested, received,
    applied with its outcome."""
    n, c = ctx.node2(), owner(ctx)
    job_id = submitted(ctx, c, n)["jobs"][0]["job_id"]
    n.accept(job_id)
    s = c.stream()
    try:
        s.until(lambda name, _d: name == "hello")
        k = c.call("POST", "jobs/%s/cancel" % job_id, body={}, key=new_key()).json
        ctx.expect(k["state"] == "requested" and k["pending"] is True and k["node"] == n.name, "requested", k)
        n.report(k["control_id"], "received")
        seen = changes(s, "cancel", k["cancel_id"], lambda r: r["state"] == "received")
        ctx.expect(seen, "received on the stream")
        n.report(k["control_id"], "applied", "stopped")
        seen = changes(s, "cancel", k["cancel_id"], lambda r: r["state"] == "applied")
        ctx.expect(seen and seen[-1]["outcome"] == "stopped" and seen[-1]["revision"] > k["revision"], "applied", seen)
        seen = changes(s, "job", job_id, lambda r: (r["cancel"] or {}).get("outcome") == "stopped")
        ctx.expect(seen, "the job shows the outcome")
    finally:
        s.close()


@case(C, "fleet", "node-status-and-stale")
def _(ctx):
    """GET nodes shows each node's protocol, status and staleness: a node never seen is stale, never stopped."""
    seen_node, never, c = ctx.node2("seen"), ctx.node("never"), owner(ctx)
    seen_node.status([{"job_id": "j", "state": "running", "run_id": "r"}])
    nodes = {x["name"]: x for x in c.call("GET", "nodes").json["nodes"]}
    a, b = nodes[seen_node.name], nodes[never.name]
    ctx.expect(a["stale"] is False and a["protocol"] == 2 and a["status"]["active"][0]["state"] == "running"
               and isinstance(a["revision"], int), "a reporting node", a)
    ctx.expect(b["stale"] is True and b["last_seen_at"] is None and b["protocol"] is None and b["status"] is None
               and "stopped" not in str(b), "never seen: stale", b)
    refused(ctx, c.call("GET", "nodes/no-such-node"), 404, "not_found", "unknown node")


@case(C, "fleet", "schedules-in-one-place")
def _(ctx):
    """Every node's schedules are listed together; a change to a node that has not picked it up shows pending;
    keys replay; bad changes are refused."""
    a, b, c = ctx.node2("a"), ctx.node2("b"), owner(ctx)
    a.schedules([schedule("nightly", revision=5)])
    b.schedules([schedule("hourly")])
    every = {s["schedule_key"] for s in c.call("GET", "schedules").json["schedules"]}
    ctx.expect({a.name + "/nightly", b.name + "/hourly"} <= every, "both nodes' schedules", every)
    key = new_key()
    body = {"expect_revision": 5, "set": {"enabled": False}}
    x = c.call("POST", "schedules/%s/nightly/changes" % a.name, body=body, key=key)
    ctx.expect(x.status == 202 and x.json["pending"] is True and x.json["state"] == "requested"
               and x.json["schedule_key"] == a.name + "/nightly", "pending until the node picks it up", x)
    again = c.call("POST", "schedules/%s/nightly/changes" % a.name, body=body, key=key)
    ctx.expect(again.status == 200 and again.json["change_id"] == x.json["change_id"]
               and again.header("idempotent-replayed") == "true", "the same key replays", again)
    s = c.call("GET", "schedules/%s/nightly" % a.name).json
    ctx.expect(s["schedule"]["pending_change_ids"] == [x.json["change_id"]] and s["schedule"]["enabled"] is True
               and [ch["change_id"] for ch in s["changes"]] == [x.json["change_id"]], "the schedule shows it pending", s)
    refused(ctx, c.call("POST", "schedules/%s/nope/changes" % a.name, body=body, key=new_key()), 404, "not_found", "unknown")
    for bad in ({"expect_revision": 5, "set": {}}, {"expect_revision": 5, "set": {"cron": "60 * * * *"}}, {"set": {"enabled": True}}):
        refused(ctx, c.call("POST", "schedules/%s/nightly/changes" % a.name, body=bad, key=new_key()), 422,
                "invalid_request", repr(bad))
    refused(ctx, c.call("POST", "schedules/%s/nightly/changes" % a.name, body=body), 428, "idempotency_key_required", "no key")
