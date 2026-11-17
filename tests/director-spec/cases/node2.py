"""Node side, protocol 2 (NODE-PROTOCOL.md §9): the node client (job-pull, spool-send, node-listen) against the stand-in
Director speaking 1 and 2. node-listen runs with "dispatch": false: it never starts tick, so a queued job stays queued
and a "running" job is a dispatch row naming a live process (this suite's own)."""
import json
import os
import shutil
import subprocess
import threading
import time

from harness.run import case
from harness.wire import H_PROTOCOL

N = "node"


def p2(rig):
    rig.director.supported = [1, 2]
    return rig


def control(rig, cid):
    return rig.director.control[cid]


def settled(rig, cid, timeout=20):
    return rig.until(lambda: control(rig, cid)["state"] in ("applied", "refused") and control(rig, cid), timeout,
                     "control %s settled" % cid)


def record(rig, cid):
    return json.loads((rig.state / "node/control" / (cid + ".json")).read_text())


def reports(rig, cid):
    return [json.loads(c["body"])["state"] for c in rig.calls("POST", "control/" + cid) if c["status"] == 200]


def terminal_sent(rig, job_id):
    for c in rig.calls("POST", "envelopes"):
        body = json.loads(c["body"])
        if body["envelope"]["event_id"] == "job_terminal:" + job_id:
            return body
    return None


def queue(rig, job_id):
    rc, out, _ = rig.op("job-pull")
    assert rc == 0 and job_id in out.get("accepted", []), out


def ts(offset=0):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + offset))


def running(rig, job_id, pid=None):
    """job_id as tick leaves it while a trial runs: a running row under a live process (this one, or `pid`)."""
    with open(rig.state / "dispatch.jsonl", "a") as shelf:
        shelf.write(json.dumps({"ts": ts(-5), "id": job_id, "job": job_id, "state": "running", "pid": pid or os.getpid(),
                                "director_job": job_id, "origin": "dispatch", "spec": "spec.yaml"}) + "\n")


# ---- §9: the version ----
@case(N, "protocol2", "node2-speaks-2")
def _(ctx):
    """Against a Director speaking 1,2 every request names 2 and is answered in 2: no probe, no fall back."""
    rig = p2(ctx.rig)
    rig.director.offer("node-dspec")
    rig.spool_run("n2-r1")
    rig.op("job-pull")
    rig.op("spool-send")
    calls = rig.calls(probes=True)
    ctx.expect(len(calls) >= 4 and all(c["headers"].get(H_PROTOCOL.lower()) == "2" and c["status"] == 200 for c in calls),
               "every request in 2, answered", [(c["method"], c["path"], c["status"]) for c in calls])


@case(N, "protocol2", "node2-listen-protocol-1-director")
def _(ctx):
    """node-listen against a protocol-1 Director: one probe, then no protocol-2 call; it stays up and says why."""
    rig = ctx.rig
    rig.listen()
    rig.until(lambda: any(e.get("event") == "protocol" for e in rig.log()), 10, "the protocol note")
    time.sleep(2)
    v2 = [c for c in rig.director.calls if c["headers"].get(H_PROTOCOL.lower()) == "2"]
    ctx.expect(1 <= len(v2) <= 2 and all(c["status"] == 426 for c in v2), "a probe (one per loop), each refused 426",
               [(c["path"], c["status"]) for c in v2])
    ctx.expect(not rig.calls("POST", "status") and not rig.calls("POST", "schedules"), "no status or schedules under 1")
    ctx.expect(rig.listener.poll() is None, "still listening")


# ---- §9.2 the held-open request ----
@case(N, "work", "node2-work-held-open")
def _(ctx):
    """One GET work is always open; an offered job is queued and accepted within seconds, through the held request;
    later requests carry the cursor; job-pull afterwards queues nothing twice."""
    rig = p2(ctx.rig)
    rig.listen()
    rig.until(lambda: len(rig.calls("GET", "work")) >= 2, 10, "a held work request")
    time.sleep(0.5)
    offered = time.time()
    jid = rig.director.offer("node-dspec")["job_id"]
    rig.until(lambda: rig.director.jobs[jid]["state"] == "accepted", 10, "the job accepted")
    ctx.expect(time.time() - offered < 4, "accepted within seconds of the offer", time.time() - offered)
    works = rig.calls("GET", "work")
    ctx.expect(works[0]["path"] == "work" and all("after=" in c["path"] for c in works[1:]),
               "the first request without after, the rest with the cursor", [c["path"] for c in works])
    rig.op("job-pull")
    pending = [r for r in rig.dispatch_rows() if r.get("director_job") == jid and r.get("state") == "pending"]
    ctx.expect(len(pending) == 1, "queued exactly once", rig.dispatch_rows())


@case(N, "work", "node2-work-backoff")
def _(ctx):
    """While the Director is down the request is retried with a backoff; once it answers, the first request has no after."""
    rig = p2(ctx.rig)
    rig.director.down = True
    rig.listen()
    rig.until(lambda: len(rig.director.calls) >= 2, 10, "retries while down")
    n = len(rig.director.calls)
    time.sleep(2)
    ctx.expect(len(rig.director.calls) - n <= 4, "backed off, not a busy loop", len(rig.director.calls) - n)
    rig.director.down = False
    first = rig.until(lambda: next((c for c in rig.director.calls if c["status"] == 200 and c["path"].startswith("work")), None),
                      40, "an answered work request")
    ctx.expect(first["path"] == "work", "asked again without after", first["path"])


@case(N, "work", "node2-listen-reaps-tick")
def _(ctx):
    """A job offered through the held request starts tick at once, and node-listen reaps it when it exits: a tick left
    a zombie keeps its pid looking alive, so a running row it left would never be closed. Here node-listen runs from
    a copy of the node client whose tick only records its pid and exits."""
    rig = p2(ctx.rig)
    client = rig.tmp.parent / (rig.tmp.name + "-code") / "ops/bin"  # its code root lies outside the state root
    shutil.copytree(rig.client, client, ignore=shutil.ignore_patterns("__pycache__"))
    pids = rig.tmp / "tick-pids"
    (client / "tick").write_text("import os\nopen(%r, 'a').write('%%d\\n' %% os.getpid())\n" % str(pids))
    rig.listen(dispatch=True, client=client)
    rig.until(lambda: len(rig.calls("GET", "work")) >= 1, 10, "a held work request")
    jid = rig.director.offer("node-dspec")["job_id"]
    pid = int(rig.until(lambda: pids.is_file() and pids.read_text().split(), 15, "tick started")[0])
    ctx.expect(any(e.get("event") == "tick" and e.get("pid") == pid for e in rig.log()), "the kick is logged with its pid",
               rig.log()[-5:])

    def stat():
        return subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    try:
        rig.until(lambda: stat() == "", 5, "tick %d reaped (ps stat: %r)" % (pid, stat()))
        reaped = True
    except AssertionError:
        reaped = False
    ctx.expect(reaped, "the exited tick is reaped, not left a zombie", stat())
    ctx.expect(rig.director.jobs[jid]["state"] == "accepted", "the job was taken", rig.director.jobs[jid]["state"])


# ---- §9.1 control: cancel ----
@case(N, "control", "node2-cancel-queued")
def _(ctx):
    """A queued job: recorded before acting, received then stopped (a lost answer is retried), a cancelled terminal row,
    and job_terminal (row_line.state cancelled) after the outcome."""
    rig = p2(ctx.rig)
    jid = rig.director.offer("node-dspec")["job_id"]
    queue(rig, jid)
    rig.director.lose = {"control/": 1}
    cid = rig.director.cancel("node-dspec", jid)
    rig.listen()
    c = settled(rig, cid)
    ctx.expect(c["state"] == "applied" and c["outcome"] == "stopped", "applied stopped", c)
    ctx.expect(reports(rig, cid)[:1] == ["received"] and reports(rig, cid)[-1] == "applied", "received first", reports(rig, cid))
    rec = record(rig, cid)
    ctx.expect(rec["outcome"] == "stopped" and "outcome" in rec["reported"], "the record holds the outcome", rec)
    rows = [r for r in rig.dispatch_rows() if r.get("director_job") == jid]
    ctx.expect([r["state"] for r in rows] == ["pending", "cancelled"], "a cancelled terminal row", rows)
    sent = rig.until(lambda: terminal_sent(rig, jid), 15, "job_terminal sent")
    ctx.expect(json.loads(sent["row_line"])["state"] == "cancelled", "row_line.state cancelled", sent)
    applied_at = max(i for i, x in enumerate(rig.director.calls) if x["path"] == "control/" + cid)
    sent_at = min(i for i, x in enumerate(rig.director.calls) if x["path"] == "envelopes")
    ctx.expect(applied_at < sent_at, "the outcome before job_terminal")
    ctx.expect(rig.director.jobs[jid]["state"] == "error", "the job is over on the Director")


@case(N, "control", "node2-cancel-never-pulled")
def _(ctx):
    """A job cancelled before it was pulled is never materialised, queued or accepted, and its job_terminal says cancelled."""
    rig = p2(ctx.rig)
    jid = rig.director.offer("node-dspec")["job_id"]
    cid = rig.director.cancel("node-dspec", jid)
    rig.listen()
    c = settled(rig, cid)
    ctx.expect(c["outcome"] == "stopped" and "not pulled" in (c.get("detail") or ""), "stopped, never pulled", c)
    rig.op("job-pull")
    ctx.expect(not (rig.state / "jobs" / jid).exists(), "never materialised")
    ctx.expect(not rig.calls("POST", "jobs/%s/accept" % jid), "never accepted")
    ctx.expect([r["state"] for r in rig.dispatch_rows() if r.get("director_job") == jid] == ["cancelled"], "only the cancelled row")
    rig.until(lambda: terminal_sent(rig, jid), 15, "job_terminal sent")


@case(N, "control", "node2-cancel-already-finished")
def _(ctx):
    """A job already over on the node: already_finished, nothing appended."""
    rig = p2(ctx.rig)
    jid = rig.director.offer("node-dspec")["job_id"]
    queue(rig, jid)
    with open(rig.state / "dispatch.jsonl", "a") as shelf:
        shelf.write(json.dumps({"ts": ts(), "id": jid, "job": jid, "state": "done", "pid": 0, "director_job": jid}) + "\n")
    cid = rig.director.cancel("node-dspec", jid)
    rig.listen()
    c = settled(rig, cid)
    ctx.expect(c["outcome"] == "already_finished", "already_finished", c)
    ctx.expect([r["state"] for r in rig.dispatch_rows() if r.get("director_job") == jid] == ["pending", "done"], "nothing appended")


@case(N, "control", "node2-cancel-running")
def _(ctx):
    """A running job: its dispatcher's children get SIGTERM (a stand-in dispatcher, `sh` waiting on a `sleep`), the node
    closes the job when the dispatcher exits without doing so, and reports stopped."""
    rig = p2(ctx.rig)
    jid = rig.director.offer("node-dspec")["job_id"]
    queue(rig, jid)
    tick = subprocess.Popen(["/bin/sh", "-c", "sleep 300 & wait"])
    threading.Thread(target=tick.wait, daemon=True).start()  # reaped when it exits, as tick's parent would
    try:
        rig.until(lambda: subprocess.run(["pgrep", "-P", str(tick.pid)], capture_output=True).stdout.strip(), 5, "the child")
        running(rig, jid, tick.pid)
        cid = rig.director.cancel("node-dspec", jid)
        rig.listen()
        c = settled(rig, cid, timeout=60)
    finally:
        tick.kill()
        tick.wait()
    ctx.expect(c["outcome"] == "stopped" and "SIGTERM to 1 process" in c["detail"], "stopped: SIGTERM to the trial", c)
    ctx.expect((rig.state / "node/cancelled" / jid).is_file(), "the marker every later trial checks")
    ctx.expect(any(r.get("state") == "cancelled" for r in rig.dispatch_rows() if r.get("director_job") == jid), "closed cancelled")


@case(N, "control", "node2-control-never-twice")
def _(ctx):
    """A settled message listed again (a Director that lost it, a restart) is reported from the record, never re-applied."""
    rig = p2(ctx.rig)
    jid = rig.director.offer("node-dspec")["job_id"]
    queue(rig, jid)
    cid = rig.director.cancel("node-dspec", jid)
    rig.listen()
    settled(rig, cid)
    rig.stop_listening()
    rig.director.requeue(cid)
    rig.listen()
    c = settled(rig, cid)
    ctx.expect(c["outcome"] == "stopped" and c["detail"] == record(rig, cid)["detail"], "the recorded outcome again", c)
    ctx.expect([r["state"] for r in rig.dispatch_rows() if r.get("director_job") == jid] == ["pending", "cancelled"],
               "applied once", rig.dispatch_rows())


@case(N, "control", "node2-control-unknown-kind")
def _(ctx):
    """A kind the node does not know is refused with outcome refused."""
    rig = p2(ctx.rig)
    with rig.director.lock:
        cid = rig.director.issue("node-dspec", "reboot", {})
    rig.listen()
    c = settled(rig, cid)
    ctx.expect(c["state"] == "refused" and c["outcome"] == "refused", "refused", c)


# ---- §9.3 events, §9.4 status ----
@case(N, "events", "node2-events-and-status")
def _(ctx):
    """A running job's run: the runner's events.jsonl as delta/stop events with the runner's seq, at most 500 a call, a lost
    answer resent from confirmed+1; status names the job running with its run, then nothing once it is over."""
    rig = p2(ctx.rig)
    jid = rig.director.offer("node-dspec")["job_id"]
    queue(rig, jid)
    running(rig, jid)
    run_id = time.strftime("%Y%m%d-%H%M%S", time.gmtime()) + ".000000001-dspec-fixture-t-t1"
    run = rig.state / "runs" / run_id
    (run / "stimulus").mkdir(parents=True)
    (run / "stimulus/manifest.json").write_text(json.dumps({"source": {"job_id": jid}}))
    lines = [{"seq": 1, "kind": "model.output", "payload": {"turn": 1, "partial": True, "delta": "Hel"}},
             {"seq": 2, "kind": "model.output", "payload": {"turn": 1, "partial": True, "delta": "lo"}},
             {"seq": 3, "kind": "model.output", "payload": {"turn": 1, "partial": False, "delta": ""}}]
    (run / "events.jsonl").write_text("".join(json.dumps(x) + "\n" for x in lines))
    rig.listen()
    held = rig.until(lambda: len((rig.director.events.get(run_id) or {}).get("events", {})) == 3
                     and rig.director.events[run_id]["events"], 15, "three events")
    ctx.expect([held[s]["kind"] for s in (1, 2, 3)] == ["delta", "delta", "stop"] and held[1]["payload"]["text"] == "Hel"
               and held[3]["payload"] == {"turn": 1, "reason": "end_turn"} and all(e["partial"] for e in held.values()),
               "mapped with the runner's seq", held)
    status = rig.until(lambda: [s for s in rig.director.status.get("node-dspec", []) if s["active"]
                                and s["active"][0].get("run_id") == run_id], 10, "status naming the run")
    ctx.expect(status[-1]["active"][0]["state"] == "running" and status[-1]["active"][0]["job_id"] == jid, "running", status[-1])
    rig.director.lose = {"runs/": 1}
    more = [{"seq": 4 + i, "kind": "model.output", "payload": {"turn": 2, "partial": True, "delta": "w%d " % i}} for i in range(700)]
    more.append({"seq": 704, "kind": "model.output", "payload": {"turn": 2, "partial": False, "delta": "", "discard": True}})
    with open(run / "events.jsonl", "a") as out:
        out.write("".join(json.dumps(x) + "\n" for x in more))
    (run / "terminal.json").write_text(json.dumps({"run_id": run_id}))
    rig.until(lambda: len(rig.director.events[run_id]["events"]) == 704, 20, "all 704 events")
    sizes = [len(json.loads(c["body"])["events"]) for c in rig.calls("POST", "runs/")]
    ctx.expect(max(sizes) <= 500, "at most 500 a call", sizes)
    ctx.expect(rig.director.events[run_id]["events"][704]["payload"] == {"turn": 2, "reason": "discard", "discard": True},
               "the discard kept", rig.director.events[run_id]["events"][704])
    rig.until(lambda: json.loads((rig.state / "node/events" / (run_id + ".json")).read_text()).get("done"), 10, "the run done")
    with open(rig.state / "dispatch.jsonl", "a") as shelf:
        shelf.write(json.dumps({"ts": ts(), "id": jid, "job": jid, "state": "done", "pid": 0, "director_job": jid}) + "\n")
    rig.until(lambda: rig.director.status["node-dspec"][-1]["active"] == [], 10, "an empty status once it is over")


# ---- §9.5 schedules ----
SCHEDULES = [
    {"id": "nightly", "cron": "0 2 * * *", "timezone": "UTC", "spec": "specs/a.yaml", "task": "tasks/a", "enabled": True, "lane": "shadow"},
    {"id": "hourly", "cron": "0 * * * *", "timezone": "UTC", "spec": "specs/b.yaml", "task": "tasks/b", "enabled": False},
    {"id": "nightly", "cron": "0 2 * * *", "timezone": "UTC", "spec": "specs/a.yaml", "task": "tasks/a", "enabled": True,
     "lane": "shadow", "event": "schedule_updated", "missed": "catch_up"},
]
FIRES = [
    {"event": "firing_recorded", "id": "nightly", "slot": "2026-11-09T02:00:00Z", "schedule_revision": 2, "ts": "2026-11-09T02:00:01Z"},
    {"id": "nightly", "slot": "2026-11-09T02:00:00Z", "job": "d-1", "ts": "2026-11-09T02:00:02Z"},
    {"event": "firing_recorded", "id": "nightly", "slot": "2026-11-10T02:00:00Z", "schedule_revision": 2, "ts": "2026-11-10T02:00:01Z"},
    {"event": "schedule_deferred", "id": "hourly", "slot": "2026-11-10T05:00:00Z", "reason": "missed", "ts": "2026-11-10T07:00:00Z"},
]


@case(N, "schedules", "node2-schedules-report-and-change")
def _(ctx):
    """The complete set with revisions (row counts) and firings (done, uncertain, missed); a schedule_change applies only
    at its revision, raises it and resends; a stale one is revision_mismatch, an unknown schedule unknown_schedule."""
    rig = p2(ctx.rig)
    (rig.state / "schedules.jsonl").write_text("".join(json.dumps(r) + "\n" for r in SCHEDULES))
    (rig.state / "schedule-fires.jsonl").write_text("".join(json.dumps(r) + "\n" for r in FIRES))
    (rig.state / "dispatch.jsonl").write_text(json.dumps({"ts": ts(), "id": "d-1", "job": "d-1", "state": "done", "pid": 0}) + "\n")
    rig.listen()
    first = rig.until(lambda: rig.director.schedules.get("node-dspec"), 10, "a schedules report")[0]
    got = {s["schedule_id"]: s for s in first["schedules"]}
    ctx.expect(got["nightly"]["revision"] == 2 and got["nightly"]["missed"] == "catch_up" and got["hourly"]["revision"] == 1
               and got["hourly"]["enabled"] is False and got["hourly"]["missed"] == "skip", "the set with revisions", got)
    states = {f["firing_id"]: f["state"] for f in first["firings"]}
    ctx.expect(states == {"nightly@2026-11-09T02:00:00Z": "done", "nightly@2026-11-10T02:00:00Z": "uncertain",
                          "hourly@2026-11-10T05:00:00Z": "missed"}, "firings", states)
    stale = rig.director.schedule_change("node-dspec", "nightly", 1, {"enabled": False})
    ctx.expect(settled(rig, stale)["outcome"] == "revision_mismatch", "a stale change refused", control(rig, stale))
    cid = rig.director.schedule_change("node-dspec", "nightly", 2, {"enabled": False})
    c = settled(rig, cid)
    ctx.expect(c["outcome"] == "applied" and c["result"] == {"revision": 3}, "applied at revision 2, now 3", c)
    last = rig.until(lambda: [r for r in rig.director.schedules["node-dspec"]
                              if any(s["schedule_id"] == "nightly" and s["revision"] == 3 for s in r["schedules"])], 10, "resent")
    nightly = next(s for s in last[-1]["schedules"] if s["schedule_id"] == "nightly")
    ctx.expect(nightly["enabled"] is False and len(last[-1]["schedules"]) == 2, "the complete set, changed", last[-1])
    ctx.expect(last[-1]["firings"] == [], "firings already reported are not resent", last[-1]["firings"])
    unknown = rig.director.schedule_change("node-dspec", "nosuch", 1, {"missed": "skip"})
    ctx.expect(settled(rig, unknown)["outcome"] == "unknown_schedule", "unknown_schedule", control(rig, unknown))


@case(N, "control", "node2-cancel-gateway-work-order")
def _(ctx):
    """A running job whose trial is a gateway work order (the `executor` op): the gateway's cancel call
    (POST <CSGATEWAY_LAB_ENDPOINT>/<job>/cancel, a stub here) comes first, then SIGTERM to the op."""
    import http.server
    rig = p2(ctx.rig)
    jid = rig.director.offer("node-dspec")["job_id"]
    queue(rig, jid)
    seen = []

    class Gateway(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            seen.append((self.path, self.rfile.read(int(self.headers.get("Content-Length") or 0))))
            body = b'{"cancelled": true}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):
            pass

    gateway = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    threading.Thread(target=gateway.serve_forever, daemon=True).start()
    executor = rig.tmp / "bin/executor"
    executor.parent.mkdir(parents=True, exist_ok=True)
    executor.write_text("#!/usr/bin/env python3\nimport time\ntime.sleep(300)\n")
    executor.chmod(0o755)
    tick = subprocess.Popen(["/bin/sh", "-c", '"%s" & wait' % executor])
    threading.Thread(target=tick.wait, daemon=True).start()
    try:
        rig.until(lambda: subprocess.run(["pgrep", "-P", str(tick.pid)], capture_output=True).stdout.strip(), 5, "the op")
        running(rig, jid, tick.pid)
        cid = rig.director.cancel("node-dspec", jid)
        rig.listen(CSGATEWAY_LAB_ENDPOINT="http://127.0.0.1:%d/v1/ai/lab-execute" % gateway.server_address[1])
        c = settled(rig, cid, timeout=60)
    finally:
        tick.kill()
        tick.wait()
        gateway.shutdown()
    ctx.expect([p for p, _ in seen] == ["/v1/ai/lab-execute/%s/cancel" % jid], "the gateway's cancel call", seen)
    ctx.expect(c["outcome"] == "stopped" and "gateway cancelled" in c["detail"] and "SIGTERM" in c["detail"], "stopped", c)


@case(N, "envelopes", "node2-run-envelope-names-job")
def _(ctx):
    """A run of a Director job: its run_finalized envelope carries job_id (additive), from the run's frozen stimulus."""
    rig = p2(ctx.rig)
    jid = rig.director.offer("node-dspec")["job_id"]
    queue(rig, jid)
    rig.spool_run("n2-job-run", overrides={"stimulus/manifest.json": json.dumps({"source": {"job_id": jid}}).encode()})
    rig.spool_run("n2-local-run", overrides={"stimulus/manifest.json": json.dumps({"source": {"job_id": "local-1"}}).encode()})
    rc, out, _ = rig.op("spool-send")
    ctx.expect(rc == 0 and out.get("sent") == 2, "both sent", out)
    sent = {json.loads(c["body"])["envelope"]["event_id"]: json.loads(c["body"])["envelope"] for c in rig.calls("POST", "envelopes")}
    ctx.expect(sent["run_finalized:n2-job-run"].get("job_id") == jid, "the Director job's run names it", sent["run_finalized:n2-job-run"])
    ctx.expect("job_id" not in sent["run_finalized:n2-local-run"], "local work names no job", sent["run_finalized:n2-local-run"])
