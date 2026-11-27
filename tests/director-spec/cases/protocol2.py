"""Director side, protocol 2 (NODE-PROTOCOL.md §9): the Director's answers to a protocol-2 node, driven through the
stand-in node (harness/node2.py). Rust only: the suite's stand-in Director speaks protocol 1.
Where a case needs a client act (a cancel, a schedule change) it uses the owner's local credential."""
import json
import threading
import time

from harness.client import Client, new_key
from harness.node2 import delta, firing, schedule, stop
from harness.run import case
from harness.wire import H_PROTOCOL, H_SUPPORTED, sha

D, RUST = "director", ("rust",)


def refused(ctx, answer, status, code, what):
    ctx.expect(answer.status == status and (answer.json or {}).get("code") == code
               and all(isinstance((answer.json or {}).get(k), str) for k in ("error", "detail")),
               "%s: want %d %s with {error, code, detail}" % (what, status, code), answer)


def owner(ctx):
    return Client(ctx.target, ctx.target.local_token())


def cancel(ctx, c, job_id):
    a = c.call("POST", "jobs/%s/cancel" % job_id, body={"reason": "spec"}, key=new_key("cancel"))
    ctx.expect(a.status == 202, "cancel requested", a)
    return a.json


def terminal(ctx, node, job_id, state="cancelled"):
    row = json.dumps({"state": state, "director_job": job_id, "run_id": None})
    env = {"envelope_version": 1, "event_id": "job_terminal:" + job_id, "job_id": job_id, "files": [],
           "envelope_sha256": sha(b""), "row_sha256": sha(row)}
    a = node.call("POST", "envelopes", body={"envelope": env, "row_line": row})
    ctx.expect((a.json or {}).get("status") == "acked", "job_terminal acked", a)


def background(fn):
    """Run fn in a thread; returns a getter for its result."""
    box = {}
    t = threading.Thread(target=lambda: box.__setitem__("v", fn()), daemon=True)
    t.start()
    return lambda timeout=60: (t.join(timeout), box.get("v"))[1]


# ---- §9 version 2 ----
@case(D, "protocol2", "p2-answered-in-2", RUST)
def _(ctx):
    """A protocol-2 request is answered in protocol 2; the Director lists 1,2; the five protocol-1 calls still work."""
    n = ctx.node2()
    a = n.call("GET", "jobs")
    ctx.expect(a.status == 200 and a.header(H_PROTOCOL) == "2", "answered in 2", a)
    ctx.expect((a.header(H_SUPPORTED) or "").split(",") == ["1", "2"], "supports 1,2", a)
    jid = n.offer()
    ctx.expect(n.accept(jid).json == {"job_id": jid, "state": "accepted"}, "accept under protocol 2")


@case(D, "protocol2", "p2-calls-need-2", RUST)
def _(ctx):
    """Protocol 2's calls do not exist for a protocol-1 request: each is 404, answered in protocol 1."""
    n = ctx.node()
    for method, path, body in (("GET", "work", None), ("GET", "control", None), ("POST", "control/ctl_x", {}),
                               ("POST", "runs/r1/events", {}), ("POST", "status", {}), ("POST", "schedules", {})):
        a = n.call(method, path, body=body)
        refused(ctx, a, 404, "not_found", "%s %s under protocol 1" % (method, path))
        ctx.expect(a.header(H_PROTOCOL) == "1", "answered in 1", a)


# ---- §9.1 control messages ----
@case(D, "control", "control-cancel-stopped", RUST)
def _(ctx):
    """A cancel is a control message: listed, received, applied with outcome stopped; then it leaves the list.
    A repeat of the outcome answers the same record; a different outcome is 409 and the first stands."""
    n, c = ctx.node2(), owner(ctx)
    jid = n.offer()
    n.accept(jid)
    k = cancel(ctx, c, jid)
    listed = n.control().json["control"]
    ctx.expect(len(listed) == 1 and listed[0]["kind"] == "cancel" and listed[0]["body"] == {"job_id": jid}
               and listed[0]["state"] == "requested" and listed[0]["control_id"] == k["control_id"]
               and listed[0]["control_id"].startswith("ctl_"), "GET control lists the cancel", listed)
    cid = listed[0]["control_id"]
    a = n.report(cid, "received")
    ctx.expect(a.status == 200 and a.json["control"]["state"] == "received" and a.json["control"]["received_at"],
               "received", a)
    ctx.expect(n.report(cid, "received").json == a.json, "received twice changes nothing")
    seen = c.call("GET", "cancels/" + k["cancel_id"]).json
    ctx.expect(seen["state"] == "received" and seen["delivered"] is True and seen["pending"] is True, "client sees received", seen)
    a = n.report(cid, "applied", "stopped", detail="runner stopped")
    ctx.expect(a.status == 200 and a.json["control"]["state"] == "applied" and a.json["control"]["outcome"] == "stopped"
               and a.json["control"]["settled_at"], "applied stopped", a)
    ctx.expect(n.report(cid, "applied", "stopped", detail="runner stopped").json == a.json, "the same outcome again")
    b = n.report(cid, "refused", "refused")
    refused(ctx, b, 409, "conflict", "a different outcome")
    ctx.expect(b.json["control"]["outcome"] == "stopped", "the first outcome stands", b)
    ctx.expect(n.control().json["control"] == [], "settled messages leave the list")
    seen = c.call("GET", "cancels/" + k["cancel_id"]).json
    ctx.expect(seen["state"] == "applied" and seen["outcome"] == "stopped" and seen["pending"] is False
               and seen["detail"] == "runner stopped", "client sees the outcome", seen)
    job = c.call("GET", "jobs/" + jid).json
    ctx.expect(job["cancel"] == {"cancel_id": k["cancel_id"], "state": "applied", "outcome": "stopped"}, "the job shows it", job)
    terminal(ctx, n, jid)
    ctx.expect(c.call("GET", "jobs/" + jid).json["state"] == "error", "the job ends as reported")


@case(D, "control", "control-cancel-outcomes", RUST)
def _(ctx):
    """already_finished and refused are recorded as the node says; a report may skip received."""
    n, c = ctx.node2(), owner(ctx)
    for state, outcome in (("applied", "already_finished"), ("refused", "refused")):
        jid = n.offer()
        k = cancel(ctx, c, jid)
        a = n.report(k["control_id"], state, outcome, detail="spec says so")
        ctx.expect(a.status == 200 and a.json["control"]["received_at"] is not None, "settled straight away", a)
        seen = c.call("GET", "cancels/" + k["cancel_id"]).json
        ctx.expect(seen["state"] == state and seen["outcome"] == outcome and seen["delivered"] is True, outcome, seen)


@case(D, "control", "control-settled-by-terminal", RUST)
def _(ctx):
    """A job reported over before its node picked the cancel up: the Director settles it as already_finished."""
    n, c = ctx.node2(), owner(ctx)
    jid = n.offer()
    n.accept(jid)
    k = cancel(ctx, c, jid)
    terminal(ctx, n, jid, state="done")
    seen = c.call("GET", "cancels/" + k["cancel_id"]).json
    ctx.expect(seen["state"] == "applied" and seen["outcome"] == "already_finished" and seen["delivered"] is False,
               "settled by the Director", seen)
    ctx.expect(n.control().json["control"] == [], "no longer listed")


@case(D, "control", "control-report-refusals", RUST)
def _(ctx):
    """Unknown or another node's message is 404; a bad state or an outcome the kind does not allow is 422."""
    n, other, c = ctx.node2("a"), ctx.node2("b"), owner(ctx)
    k = cancel(ctx, c, n.offer())
    cid = k["control_id"]
    refused(ctx, n.report("ctl_nosuch", "received"), 404, "not_found", "unknown")
    refused(ctx, other.report(cid, "received"), 404, "not_found", "another node's")
    for state, outcome in (("done", None), ("applied", None), ("applied", "refused"), ("refused", "stopped")):
        refused(ctx, n.report(cid, state, outcome), 422, "invalid_request", "%s/%s" % (state, outcome))
    refused(ctx, n.call("POST", "control/" + cid, body=b"[]"), 422, "invalid_request", "not an object")
    ctx.expect(n.control().json["control"][0]["state"] == "requested", "nothing changed")


@case(D, "control", "control-v1-node-pending", RUST)
def _(ctx):
    """A cancel to a node that speaks protocol 1 stays requested and pending, and says why."""
    n, c = ctx.node(), owner(ctx)
    jid = n.offer()
    n.call("GET", "jobs")
    k = cancel(ctx, c, jid)
    ctx.expect(k["state"] == "requested" and k["pending"] is True and k["delivered"] is False
               and "protocol 1" in k["detail"], "pending, protocol 1", k)


# ---- §9.2 GET work ----
@case(D, "work", "work-cursor", RUST)
def _(ctx):
    """Without after: answered at once with jobs, control and a cursor. With a stale after: at once, changed.
    With the current cursor and wait=1: held about a second, changed false, same cursor. wait=x is 422."""
    n = ctx.node2()
    jid = n.offer()
    a, took = n.work()
    w = a.json or {}
    ctx.expect(a.status == 200 and w.get("changed") is True and [j["job_id"] for j in w["jobs"]] == [jid]
               and w["control"] == [] and isinstance(w["cursor"], str) and took < 2, "answered at once", a)
    ctx.expect(w["jobs"] == n.call("GET", "jobs").json["jobs"], "jobs exactly as GET jobs")
    a2, took = n.work(after="stale-cursor")
    ctx.expect(a2.json["changed"] is True and a2.json["cursor"] == w["cursor"] and took < 2, "stale after: at once", a2)
    a3, took = n.work(after=w["cursor"], wait=1)
    ctx.expect(a3.status == 200 and a3.json["changed"] is False and a3.json["cursor"] == w["cursor"]
               and 0.8 <= took < 4, "held about a second", (a3, took))
    refused(ctx, n.work(wait="x")[0], 422, "invalid_request", "wait=x")


@case(D, "work", "work-wakes-on-offer", RUST)
def _(ctx):
    """A held request is answered within about a second of a job being offered; the node shows as listening
    while it holds one."""
    n, c = ctx.node2(), owner(ctx)
    cursor = n.work()[0].json["cursor"]
    held = background(lambda: n.work(after=cursor))
    time.sleep(0.5)
    ctx.expect(c.call("GET", "nodes/" + n.name).json.get("listening") is True, "listening while held")
    offered = time.time()
    jid = n.offer()
    a, took = held()
    ctx.expect(a is not None and a.status == 200 and a.json["changed"] is True
               and [j["job_id"] for j in a.json["jobs"]] == [jid], "answered with the job", a)
    ctx.expect(time.time() - offered < 3, "within about a second", time.time() - offered)
    ctx.expect(c.call("GET", "nodes/" + n.name).json.get("listening") is False, "not listening once answered")


@case(D, "work", "work-wakes-on-control", RUST)
def _(ctx):
    """A held request is answered when a control message arrives for the node."""
    n, c = ctx.node2(), owner(ctx)
    jid = n.offer()
    n.accept(jid)
    cursor = n.work()[0].json["cursor"]
    held = background(lambda: n.work(after=cursor))
    time.sleep(0.5)
    k = cancel(ctx, c, jid)
    a, took = held()
    ctx.expect(a is not None and a.json["changed"] is True and [m["control_id"] for m in a.json["control"]] == [k["control_id"]]
               and took < 5, "answered with the cancel", a)


@case(D, "work", "work-held-30s", RUST)
def _(ctx):
    """With nothing new, a held request is answered 30 seconds after it arrived, changed false (§9.2)."""
    n = ctx.node2()
    cursor = n.work()[0].json["cursor"]
    a, took = n.work(after=cursor)
    ctx.expect(a.status == 200 and a.json["changed"] is False and a.json["cursor"] == cursor, "nothing changed", a)
    ctx.expect(29.5 <= took <= 33, "answered at 30 seconds", took)


# ---- §9.3 live events ----
@case(D, "events", "run-events-numbered", RUST)
def _(ctx):
    """Events are recorded 1, 2, 3… without gaps: duplicates are dropped and counted, an event past a gap is not
    recorded and the answer names the number wanted; resending from there fills it."""
    n = ctx.node2()
    jid = n.offer()
    run = "%s-run" % ctx.serial_tag()
    a = n.events(run, jid, [delta(1, "Hel"), delta(2, "lo"), delta(3, ",")])
    ctx.expect(a.status == 200 and a.json == {"run_id": run, "confirmed": 3, "accepted": 3, "duplicates": 0, "gap": None},
               "three recorded", a)
    a = n.events(run, jid, [delta(2, "lo"), delta(3, ","), delta(4, " wor")])
    ctx.expect(a.json["confirmed"] == 4 and a.json["accepted"] == 1 and a.json["duplicates"] == 2, "duplicates dropped", a)
    a = n.events(run, jid, [delta(6, "!"), stop(7)])
    ctx.expect(a.json["confirmed"] == 4 and a.json["accepted"] == 0 and a.json["gap"] == {"expected": 5}, "gap noticed", a)
    a = n.events(run, jid, [])
    ctx.expect(a.json["confirmed"] == 4 and a.json["accepted"] == 0, "an empty call asks for confirmed", a)
    a = n.events(run, jid, [delta(5, "ld"), delta(6, "!"), stop(7)])
    ctx.expect(a.json["confirmed"] == 7 and a.json["accepted"] == 3 and a.json["gap"] is None, "resent from 5", a)
    got = owner(ctx).call("GET", "runs/%s/events" % run).json
    ctx.expect([e["seq"] for e in got["events"]] == list(range(1, 8)) and got["confirmed"] == 7
               and "".join(e["payload"].get("text", "") for e in got["events"]) == "Hello, world!"
               and got["events"][1]["payload"] == {"text": "lo"}, "recorded once each, first copy kept", got)


@case(D, "events", "run-events-refusals", RUST)
def _(ctx):
    """Malformed events are 422 and record nothing; another node's job is 404; a run id another job uses is 409."""
    n, other = ctx.node2("a"), ctx.node2("b")
    jid, jid2, theirs = n.offer(), n.offer(), other.offer()
    run = "%s-run" % ctx.serial_tag()
    for what, events in (("seq 0", [dict(delta(1, "x"), seq=0)]), ("bad kind", [dict(delta(1, "x"), kind="Delta")]),
                         ("no partial", [{k: v for k, v in delta(1, "x").items() if k != "partial"}]),
                         ("payload not object", [dict(delta(1, "x"), payload="x")]),
                         ("unsorted", [delta(2, "b"), delta(1, "a")]), ("too many", [delta(i, "x") for i in range(1, 502)])):
        refused(ctx, n.events(run, jid, events), 422, "invalid_request", what)
    refused(ctx, n.call("POST", "runs/%s/events" % run, body={"events": []}), 422, "invalid_request", "no job_id")
    refused(ctx, n.events("a..b", jid, []), 422, "invalid_request", "run id not plain")
    refused(ctx, n.events(run, theirs, [delta(1, "x")]), 404, "not_found", "another node's job")
    ctx.expect(n.events(run, jid, [delta(1, "x")]).json["confirmed"] == 1, "nothing recorded by the refusals")
    refused(ctx, n.events(run, jid2, [delta(2, "x")]), 409, "conflict", "the run belongs to another job")


# ---- §9.4 status ----
@case(D, "status", "status-report", RUST)
def _(ctx):
    """A status report is recorded and replaces the last; a malformed one is 422."""
    n, c = ctx.node2(), owner(ctx)
    active = [{"job_id": "j1", "run_id": "r1", "state": "running", "started_at": "2026-11-10T00:00:00Z"},
              {"job_id": "j2", "state": "queued"}]
    a = n.status(active)
    ctx.expect(a.status == 200 and isinstance(a.json.get("recorded_at"), str), "recorded", a)
    node = c.call("GET", "nodes/" + n.name).json
    st = node.get("status") or {}
    ctx.expect(node["protocol"] == 2 and node["stale"] is False and st.get("node_version") == "director-spec stand-in"
               and [x["job_id"] for x in st.get("active", [])] == ["j1", "j2"] and st["active"][1]["run_id"] is None,
               "the node shows its report", node)
    n.status([])
    ctx.expect(c.call("GET", "nodes/" + n.name).json["status"]["active"] == [], "a report replaces the last")
    for body in ({"active": [{"job_id": "j", "state": "stopped"}]}, {"active": "x"}, [1]):
        refused(ctx, n.call("POST", "status", body=body), 422, "invalid_request", repr(body))


@case(D, "status", "status-vitals", RUST)
def _(ctx):
    """Optional vitals and held on a status report are stored and returned on GET nodes."""
    n, c = ctx.node2(), owner(ctx)
    vitals = {"memory_total_bytes": 36_000_000_000, "memory_available_bytes": 2_000_000_000,
              "memory_pressure": "normal", "swap_total_bytes": 8_000_000_000, "swap_used_bytes": 7_500_000_000,
              "disk_free_bytes": 20_000_000_000}
    held = {"reason": "swap_high"}
    a = n.status([], vitals=vitals, held=held)
    ctx.expect(a.status == 200, "recorded", a)
    node = c.call("GET", "nodes/" + n.name).json
    st = node.get("status") or {}
    ctx.expect(st.get("vitals") == vitals and st.get("held") == held, "vitals and held on GET nodes", st)
    ctx.expect(n.status([], vitals=vitals).status == 200, "held cleared when omitted")
    st2 = c.call("GET", "nodes/" + n.name).json.get("status") or {}
    ctx.expect("held" not in st2 and st2.get("vitals") == vitals, "held gone, vitals remain", st2)


@case(D, "status", "status-provider-limits", RUST)
def _(ctx):
    """Optional provider_limits on a status report are kept (unknown row fields dropped) and shown on GET nodes."""
    n, c = ctx.node2(), owner(ctx)
    row = {"template": "cursor-sol", "message": "usage limit", "resets_at": "2099-01-01T00:00:00Z",
           "recorded_at": "2026-11-14T00:00:00Z"}
    a = n.call("POST", "status", body={"active": [], "provider_limits": [dict(row, extra="x")]})
    ctx.expect(a.status == 200, "recorded", a)
    st = c.call("GET", "nodes/" + n.name).json.get("status") or {}
    ctx.expect(st.get("provider_limits") == [row], "provider_limits on GET nodes", st)
    n.status([])
    st2 = c.call("GET", "nodes/" + n.name).json.get("status") or {}
    ctx.expect("provider_limits" not in st2, "omitted when the node sends none", st2)
    for body in ({"active": [], "provider_limits": "x"}, {"active": [], "provider_limits": [{"message": "m"}]}):
        refused(ctx, n.call("POST", "status", body=body), 422, "invalid_request", repr(body))


# ---- §9.5 schedules ----
@case(D, "schedules", "schedules-report", RUST)
def _(ctx):
    """Schedules replace the node's set (missing ones are removed); missed defaults to skip; firings keep their
    identity and their latest report."""
    n, c = ctx.node2(), owner(ctx)
    a = n.schedules([schedule("nightly"), schedule("hourly", missed="catch_up")],
                    [firing("nightly", "2026-11-10T02:00")])
    ctx.expect(a.status == 200 and a.json["schedules"] == 2 and a.json["firings"] == 1, "recorded", a)
    listed = {s["schedule_id"]: s for s in c.call("GET", "schedules?node=" + n.name).json["schedules"]}
    ctx.expect(set(listed) == {"nightly", "hourly"} and listed["nightly"]["missed"] == "skip"
               and listed["hourly"]["missed"] == "catch_up" and listed["nightly"]["schedule_revision"] == 1
               and listed["nightly"]["last_firing"]["firing_id"] == "nightly@2026-11-10T02:00", "listed", listed)
    n.schedules([schedule("nightly")], [firing("nightly", "2026-11-10T02:00", state="done", job_id="d-1")])
    got = c.call("GET", "schedules/%s/nightly" % n.name).json
    ctx.expect(len(got["firings"]) == 1 and got["firings"][0]["state"] == "done" and got["firings"][0]["job_id"] == "d-1",
               "one firing per identity, its latest report", got)
    gone = c.call("GET", "schedules/%s/hourly" % n.name).json["schedule"]
    ctx.expect(gone["removed_at"] is not None, "missing from the report: removed", gone)
    for body in ({"firings": []}, {"schedules": [schedule("x", missed="later")]},
                 {"schedules": [schedule("x", revision=0)]}, {"schedules": [], "firings": [firing("x", "s", state="ran")]}):
        refused(ctx, n.call("POST", "schedules", body=body), 422, "invalid_request", repr(body)[:60])


@case(D, "schedules", "schedule-change-applied", RUST)
def _(ctx):
    """A client's change travels as a schedule_change naming the revision it expects; the node applies it, reports
    the new revision, and the change counts once confirmed."""
    n, c = ctx.node2(), owner(ctx)
    n.schedules([schedule("nightly", revision=3)])
    a = c.call("POST", "schedules/%s/nightly/changes" % n.name, body={"expect_revision": 3, "set": {"enabled": False}},
               key=new_key("sch"))
    ctx.expect(a.status == 202 and a.json["state"] == "requested" and a.json["pending"] is True, "requested", a)
    change = a.json
    msg = n.control().json["control"]
    ctx.expect(len(msg) == 1 and msg[0]["kind"] == "schedule_change" and msg[0]["body"] ==
               {"schedule_id": "nightly", "expect_revision": 3, "set": {"enabled": False}}, "the node is told", msg)
    refused(ctx, n.report(msg[0]["control_id"], "applied", "applied"), 422, "invalid_request", "applied without result")
    r = n.report(msg[0]["control_id"], "applied", "applied", result={"revision": 4})
    ctx.expect(r.status == 200, "applied", r)
    n.schedules([schedule("nightly", revision=4, enabled=False)])
    got = c.call("GET", "schedule-changes/" + change["change_id"]).json
    ctx.expect(got["state"] == "applied" and got["outcome"] == "applied" and got["result_revision"] == 4
               and got["pending"] is False, "confirmed", got)
    s = c.call("GET", "schedules/%s/nightly" % n.name).json["schedule"]
    ctx.expect(s["enabled"] is False and s["schedule_revision"] == 4 and s["pending_change_ids"] == [], "the schedule", s)


@case(D, "schedules", "schedule-change-older-loses", RUST)
def _(ctx):
    """An older change cannot overwrite a newer one: the Director refuses a stale expect_revision (409), and of
    two changes from the same revision the node refuses the second as revision_mismatch."""
    n, c = ctx.node2(), owner(ctx)
    n.schedules([schedule("nightly", revision=2)])
    a = c.call("POST", "schedules/%s/nightly/changes" % n.name, body={"expect_revision": 1, "set": {"enabled": False}},
               key=new_key())
    refused(ctx, a, 409, "schedule_revision_mismatch", "stale revision")
    ctx.expect(a.json["schedule"]["schedule_revision"] == 2, "names the current schedule", a)
    first = c.call("POST", "schedules/%s/nightly/changes" % n.name, body={"expect_revision": 2, "set": {"enabled": False}},
                   key=new_key()).json
    second = c.call("POST", "schedules/%s/nightly/changes" % n.name, body={"expect_revision": 2, "set": {"missed": "catch_up"}},
                    key=new_key()).json
    n.report(first["control_id"], "applied", "applied", result={"revision": 3})
    n.report(second["control_id"], "refused", "revision_mismatch", detail="schedule is at 3")
    got = c.call("GET", "schedule-changes/" + second["change_id"]).json
    ctx.expect(got["state"] == "refused" and got["outcome"] == "revision_mismatch", "the second is refused", got)


def create_schedule(ctx, n, c, **over):
    body = {"node": n.name, "name": "Nightly", "cron": "0 2 * * *", "timezone": "America/New_York",
            "spec": "spec.yaml", "task": "tasks/t", "missed": "skip"}
    body.update(over)
    key = new_key("schedule-create")
    answer = c.call("POST", "schedules", body=body, key=key)
    ctx.expect(answer.status == 202 and answer.json["pending"] and answer.json["kind"] == "create",
               "creation is pending at the node", answer)
    replay = c.call("POST", "schedules", body=body, key=key)
    ctx.expect(replay.status == 200 and replay.header("idempotent-replayed") == "true"
               and replay.json["change_id"] == answer.json["change_id"], "creation replays", replay)
    return answer.json


@case(D, "schedules", "schedule-lifecycle", RUST)
def _(ctx):
    """Create, edit and delete travel through control and reconcile once the node reports its ledger."""
    n, c = ctx.node2(), owner(ctx)
    bad = {"node": n.name, "name": "Invalid", "cron": "0 24 * * *", "timezone": "UTC",
           "spec": "spec.yaml", "task": "tasks/t"}
    refused(ctx, c.call("POST", "schedules", body=bad, key=new_key()), 422, "invalid_request", "bad cron")
    refused(ctx, c.call("POST", "schedules", body=dict(bad, cron="0 2 * * *", timezone="Mars/Olympus"),
                        key=new_key()), 422, "invalid_request", "bad timezone")
    created = create_schedule(ctx, n, c)
    sid, path = created["schedule_id"], "schedules/" + created["schedule_key"]
    pending = c.call("GET", path).json["schedule"]
    ctx.expect(pending["schedule_revision"] == 0 and pending["enabled"] is False, "pending definition", pending)

    def apply(change, kind, revision, snapshot):
        message, = n.control().json["control"]
        body = message["body"]
        ctx.expect(message["control_id"] == change["control_id"] and body["schedule_id"] == sid
                   and body["expect_revision"] == revision - 1 and body.get("kind") == kind
                   and body["set"] == change["set"], "node receives the requested change", message)
        ctx.expect(n.report(message["control_id"], "applied", "applied", result={"revision": revision}).status == 200,
                   "node settles the change")
        ctx.expect(n.schedules([snapshot]).status == 200, "node reports the new ledger revision")
        detail = c.call("GET", path).json["schedule"]
        ctx.expect(detail["schedule_revision"] == revision and not detail["pending_change_ids"],
                   "Director shows the confirmed revision", detail)
        return detail

    snapshot = schedule(sid, timezone="America/New_York",
                        definition={"director_managed": True, "name": "Nightly"})
    n.schedules([])
    detail = apply(created, "create", 1, snapshot)
    ctx.expect(detail["name"] == "Nightly" and detail["enabled"] is True, "created schedule is active", detail)
    fields = {"cron": "*/15 8-17 * * 1-5", "timezone": "Europe/Paris", "missed": "catch_up", "enabled": False}
    edit = c.call("POST", path + "/changes", body={"expect_revision": 1, "set": fields}, key=new_key())
    ctx.expect(edit.status == 202, "edit queued", edit)
    snapshot = dict(snapshot, revision=2, **fields)
    detail = apply(edit.json, None, 2, snapshot)
    ctx.expect(all(detail[k] == v for k, v in fields.items()), "cron, zone, policy and enabled changed", detail)
    key = new_key("delete")
    deletion = c.call("DELETE", path, body={"expect_revision": 2}, key=key)
    ctx.expect(deletion.status == 202, "deletion queued", deletion)
    tombstone = dict(snapshot, revision=3, definition={**snapshot["definition"], "deleted": True})
    detail = apply(deletion.json, "delete", 3, tombstone)
    ctx.expect(detail["removed_at"] and detail["enabled"] is False, "deleted schedule is a tombstone", detail)
    n.schedules([tombstone])
    ctx.expect(c.call("GET", path).json["schedule"] == detail, "a repeated tombstone keeps its original timestamp")
    ctx.expect(c.call("DELETE", path, body={"expect_revision": 2}, key=key).status == 200, "delete replays")


@case(D, "schedules", "schedule-budget-and-usage", RUST)
def _(ctx):
    """The first firing reserves schedule money, the second waits, and finalized usage is attributed."""
    from cases.budgets import finish, offered_usd

    n, c = ctx.node2(), owner(ctx)
    created = create_schedule(ctx, n, c, budget={"usd": 0.1})
    sid, path = created["schedule_id"], "schedules/" + created["schedule_key"]
    command = ("job", "create", "--node", n.name, "--bundle", str(ctx.target.bundle), "--meta", "schedule_id=" + sid)
    first = ctx.target.admin(*command)["job_id"]
    second = ctx.target.admin(*command)["job_id"]
    ctx.expect(offered_usd(n, first) == 0.1, "the scheduled spec carries its reservation")
    held = c.call("GET", "jobs/" + second).json
    ctx.expect(held["state"] == "waiting" and held["budget"]["reason"] == "budget_reserved",
               "the next firing waits for its schedule", held)
    finish(ctx, n, first, charged=0.1)
    held = c.call("GET", "jobs/" + second).json
    ctx.expect(held["state"] == "waiting" and held["budget"]["reason"] == "budget_exhausted",
               "settled spend exhausts the schedule", held)
    budget = c.call("GET", path).json["schedule"]["budget"]
    ctx.expect(budget["spent_usd"] == 0.1 and budget["waiting_job_ids"] == [second], "budget shows usage and waiters", budget)
    usage = c.call("GET", "usage?by=schedule&period=all&node=" + n.name).json
    group, = usage["groups"]
    ctx.expect(usage["by"] == "schedule_id" and group["schedule_id"] == sid and group["runs"] == 1
               and group["charged_usd"] == 0.1, "usage groups scheduled jobs", usage)
