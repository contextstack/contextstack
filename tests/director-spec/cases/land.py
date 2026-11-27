"""Land records: the node reports landing; clients read land and land_state (CLIENT-API.md §6.4)."""
from harness.client import new_key
from harness.node2 import land_body
from harness.run import case

# Reuse the client work helpers.
from cases.client import accept, finish, refused, submitted, owner


def revision(record):
    return record.get("revision")


C = "client"


def ready_run(ctx):
    n = ctx.node2()
    c = owner(ctx)
    got = submitted(ctx, c, n)
    job_id = got["jobs"][0]["job_id"]
    accept(ctx, n, job_id)
    run_id = finish(ctx, n, job_id)["envelope"]["run_id"]
    return n, c, got, job_id, run_id


@case(C, "land", "land-record")
def land_record(ctx):
    """Record a landing; replay; green then red and green then green are 409; red then green replaces it."""
    n, c, got, job_id, run_id = ready_run(ctx)
    intent_id = got["intent"]["intent_id"]
    key = new_key("land")
    coverage = {"covered": ["probe-a"], "uncovered": ["probe-b"]}
    body = land_body("landed", commit="cafebabe", host_probe_coverage=coverage)
    s = c.stream()
    try:
        s.until(lambda name, _d: name == "hello")
        a = n.land(run_id, body, key)
        ctx.expect(a.status == 201 and a.json["state"] == "landed" and a.json["run_id"] == run_id, "landed", a)
        r = a.json
        ctx.expect(r["commit"] == "cafebabe" and r.get("attempts") == [] and isinstance(r["land_id"], str)
                   and isinstance(revision(r), int), "the record", r)
        run = c.call("GET", "runs/" + run_id).json
        job = c.call("GET", "jobs/" + job_id).json
        ctx.expect((run.get("land") or {}).get("land_id") == r["land_id"], "run carries land", run)
        ctx.expect((run.get("land") or {}).get("attempts") == [], "run land has no earlier attempt", run)
        ctx.expect(job.get("land_state") == "landed" and (job.get("land") or {}).get("state") == "landed", "job", job)
        for land in (r, run.get("land") or {}, job.get("land") or {}):
            ctx.expect(land.get("host_probe_coverage") == coverage, "land carries host probe coverage", land)
        view = c.call("GET", "intents/" + intent_id).json
        j = next(j for j in view["jobs"] if j["job_id"] == job_id)
        ctx.expect(j.get("land_state") == "landed", "intent bundle job", j)
        ev, _ = s.until(lambda name, d: name == "run.land" and d.get("run_id") == run_id, timeout=5)
        ctx.expect(ev is not None and (ev[1].get("land") or {}).get("land_id") == r["land_id"], "run.land", ev)
        replay = n.land(run_id, body, key)
        ctx.expect(replay.status == 200 and replay.headers.get("idempotent-replayed") == "true", "replay", replay)
        conflict = n.land(run_id, land_body("failed", reason="other"), new_key("land2"))
        refused(ctx, conflict, 409, "already_landed", "green then red")
        again = n.land(run_id, land_body("landed", commit="feedface"), new_key("land3"))
        refused(ctx, again, 409, "already_landed", "green then green")
    finally:
        s.close()

    n2 = ctx.node2("f")
    c2 = owner(ctx)
    got2 = submitted(ctx, c2, n2)
    job2 = got2["jobs"][0]["job_id"]
    accept(ctx, n2, job2)
    run2 = finish(ctx, n2, job2, run_id="%s-fail" % ctx.serial_tag())["envelope"]["run_id"]
    s2 = c2.stream()
    try:
        s2.until(lambda name, _d: name == "hello")
        fail_key = new_key("lf")
        failed_body = land_body("failed", reason="regressions failed", host_probe_coverage=coverage)
        f = n2.land(run2, failed_body, fail_key)
        ctx.expect(f.status == 201 and f.json["state"] == "failed" and f.json.get("attempts") == [], "failed", f)
        ev_f, _ = s2.until(
            lambda name, d: name == "run.land" and (d.get("land") or {}).get("state") == "failed", timeout=5)
        ctx.expect(ev_f is not None and (ev_f[1].get("land") or {}).get("land_id") == f.json["land_id"],
                   "run.land for the failure", ev_f)
        ok_key = new_key("lg")
        landed_body = land_body("landed", commit="cafebabe")
        g = n2.land(run2, landed_body, ok_key)
        ctx.expect(g.status == 201 and g.json["state"] == "landed" and g.json["land_id"] != f.json["land_id"],
                   "landed after failed", g)
        ctx.expect("host_probe_coverage" not in g.json, "coverage belongs to the original attempt", g)
        attempt = g.json.get("attempts") or []
        ctx.expect(len(attempt) == 1 and attempt[0].get("state") == "failed"
                   and attempt[0].get("reason") == "regressions failed"
                   and attempt[0].get("land_id") == f.json["land_id"]
                   and attempt[0].get("time") == f.json["landed_at"], "one earlier attempt", g.json)
        run = c2.call("GET", "runs/" + run2).json
        ctx.expect((run.get("land") or {}).get("state") == "landed"
                   and (run.get("land") or {}).get("land_id") == g.json["land_id"]
                   and (run.get("land") or {}).get("attempts") == attempt, "run ends landed", run)
        job = c2.call("GET", "jobs/" + job2).json
        ctx.expect(job.get("land_state") == "landed" and (job.get("land") or {}).get("attempts") == attempt,
                   "job ends landed", job)
        ev_g, _ = s2.until(
            lambda name, d: name == "run.land" and (d.get("land") or {}).get("land_id") == g.json["land_id"],
            timeout=5)
        ctx.expect(ev_g is not None and (ev_g[1].get("land") or {}).get("attempts") == attempt,
                   "run.land for the landing", ev_g)
        replay_fail = n2.land(run2, failed_body, fail_key)
        ctx.expect(replay_fail.status == 200 and replay_fail.headers.get("idempotent-replayed") == "true"
                   and replay_fail.json["state"] == "failed" and replay_fail.json["land_id"] == f.json["land_id"]
                   and replay_fail.json.get("attempts") == []
                   and replay_fail.json.get("host_probe_coverage") == coverage,
                   "failed key still answers the original record", replay_fail)
        replay_ok = n2.land(run2, landed_body, ok_key)
        ctx.expect(replay_ok.status == 200 and replay_ok.headers.get("idempotent-replayed") == "true"
                   and replay_ok.json["state"] == "landed" and replay_ok.json.get("attempts") == attempt,
                   "landed key replays the replacement", replay_ok)
    finally:
        s2.close()


@case(C, "land", "land-wrong-node")
def land_wrong_node(ctx):
    """Another node cannot record a land for a run it does not own."""
    n, _, _, job_id, run_id = ready_run(ctx)
    other = ctx.node2("b")
    refused(ctx, other.land(run_id, land_body("landed"), new_key("x")), 404, "not_found", "wrong node")


@case(C, "land", "land-idempotency-required")
def land_idempotency_required(ctx):
    """Land requires Idempotency-Key."""
    n, _, _, _job_id, run_id = ready_run(ctx)
    h = n.headers()
    refused(ctx, n.call("POST", "runs/%s/land" % run_id, body=land_body("landed"), headers=h), 428,
            "idempotency_key_required", "missing key")
