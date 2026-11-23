"""Node side: the node client (job-pull, spool-send) against the stand-in Director (NODE-PROTOCOL.md)."""
import json

from harness.run import case
from harness.wire import H_INSTALLATION, H_PROTOCOL, JOB_FILES, sha, terminal_fixture

N = "node"


def pulled(ctx, rig, rc, out):
    ctx.expect(rc == 0 and out.get("ok") is True, "job-pull exits 0 ok", out)


def nothing_changed(ctx, rig, what):
    ctx.expect(not (rig.state / "jobs").exists() or not any((rig.state / "jobs").iterdir()), what + ": nothing materialised")
    ctx.expect(not (rig.state / "dispatch.jsonl").exists(), what + ": nothing queued")
    ctx.expect(not rig.calls("POST", "jobs/"), what + ": nothing accepted")


# ---- §1, §2: every request ----
@case(N, "protocol", "node-sends-protocol-and-installation")
def _(ctx):
    """Every request carries the bearer token, the installation and a protocol: 2 first; against this protocol-1
    Director the 426 that lists only 1 is followed by the same request in 1, and every request after it is 1."""
    rig = ctx.rig
    rig.director.offer("node-dspec")
    rig.spool_run("nspec-r1")
    rig.op("job-pull")
    rig.op("spool-send")
    calls = rig.director.calls
    ctx.expect(len(calls) >= 4, "the client made its calls", [(c["method"], c["path"]) for c in calls])
    for c in calls:
        h = c["headers"]
        ctx.expect(h.get("authorization") == "Bearer " + rig.token and h.get(H_INSTALLATION.lower()) == rig.installation
                   and h.get(H_PROTOCOL.lower()) == ("2" if c["status"] == 426 else "1"), "%s %s headers" % (c["method"], c["path"]), h)
    probes = [c for c in calls if c["status"] == 426]
    ctx.expect(len(probes) == 2 and calls.index(probes[0]) == 0, "one 426 probe per op, each its first request",
               [(c["method"], c["path"], c["status"]) for c in calls])


@case(N, "protocol", "node-wrong-director")
def _(ctx):
    """Answers naming another installation are not acted on: nothing fetched, queued or accepted; the error says so."""
    rig = ctx.rig
    rig.director.offer("node-dspec")
    rig.director.foreign = True
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect("installation" in (out.get("error") or ""), "error names the installation", out)
    ctx.expect(not rig.calls("GET", "objects/"), "no object fetched")
    nothing_changed(ctx, rig, "foreign")
    rig.spool_run("nspec-r2")
    rc, out, _ = rig.op("spool-send")
    ctx.expect(rc == 0 and out.get("sent") == 0 and out.get("pending") == 1 and "installation" in json.dumps(out.get("failed")),
               "spool-send: entry kept, failure names the installation", out)


@case(N, "protocol", "node-protocol-refused-426")
def _(ctx):
    """A Director that speaks neither of the node's versions (only 3) answers 426; the client reports the versions
    and changes nothing."""
    rig = ctx.rig
    rig.director.offer("node-dspec")
    rig.director.supported = [3]
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect("426" in (out.get("error") or "") and "protocol 3" in out.get("error", ""), "error names 426 and the Director's versions", out)
    nothing_changed(ctx, rig, "426")


@case(N, "protocol", "node-old-director-no-protocol")
def _(ctx):
    """A 200 that does not name protocol 1 (a Director predating it) is not acted on."""
    rig = ctx.rig
    rig.director.offer("node-dspec")
    rig.director.omit_protocol = True
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect("protocol" in (out.get("error") or ""), "error names the protocol", out)
    nothing_changed(ctx, rig, "unversioned")


@case(N, "protocol", "node-installation-unset-421")
def _(ctx):
    """A receiver with no installation is refused 421; the client says to set it from the registration output."""
    rig = ctx.rig
    rig.configure(installation=None)
    rig.director.offer("node-dspec")
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect("421" in (out.get("error") or "") and "receiver.installation" in out.get("error", ""), "421 with the remedy", out)
    ctx.expect(H_INSTALLATION.lower() not in rig.director.calls[0]["headers"], "no installation header sent")
    nothing_changed(ctx, rig, "421")


@case(N, "protocol", "node-installation-invalid-config")
def _(ctx):
    """A malformed installation in policy/spool.json is refused before any request (exit 2)."""
    rig = ctx.rig
    rig.configure(installation="not valid/!")
    rc, out, _ = rig.op("job-pull")
    ctx.expect(rc == 2 and out.get("ok") is False and "installation" in out.get("error", ""), "exit 2 naming installation", out)
    ctx.expect(not rig.director.calls, "no request made")


# ---- §4, §5, §6: pulling work ----
@case(N, "jobs", "node-pull-accept")
def _(ctx):
    """An offered job is fetched, verified, materialised, queued once, then accepted with its bundle_sha256."""
    rig = ctx.rig
    job = rig.director.offer("node-dspec", meta={"schedule_id": "nightly"})
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect(out.get("accepted") == [job["job_id"]] and out.get("failed") == [], "accepted", out)
    target = rig.state / "jobs" / job["job_id"]
    for path, (data, mode) in JOB_FILES.items():
        ctx.expect((target / path).read_bytes() == data and ((target / path).stat().st_mode & 0o777) == mode, "materialised " + path)
    rows = [json.loads(line) for line in (rig.state / "dispatch.jsonl").read_text().splitlines()]
    ctx.expect(len(rows) == 1 and rows[0]["director_job"] == job["job_id"] and rows[0]["state"] == "pending"
               and rows[0].get("schedule_id") == "nightly", "one pending row with the job's meta", rows)
    accepts = rig.calls("POST", "jobs/")
    ctx.expect(len(accepts) == 1 and json.loads(accepts[0]["body"]) == {"bundle_sha256": job["bundle_sha256"]},
               "accept echoes bundle_sha256", accepts)
    ctx.expect(rig.director.jobs[job["job_id"]]["state"] == "accepted", "the stand-in recorded the accept")
    gets = [c["path"] for c in rig.calls("GET", "objects/")]
    ctx.expect(sorted(gets) == sorted("objects/" + f["sha256"] for f in job["files"]), "each object fetched once", gets)


@case(N, "jobs", "node-pull-idempotent")
def _(ctx):
    """A second pull of an accepted job fetches nothing, queues nothing new and repeats the harmless accept."""
    rig = ctx.rig
    job = rig.director.offer("node-dspec")
    rig.op("job-pull")
    before = len(rig.calls("GET", "objects/"))
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect(out.get("accepted") == [job["job_id"]] and len(rig.calls("GET", "objects/")) == before, "no refetch", out)
    ctx.expect(len((rig.state / "dispatch.jsonl").read_text().splitlines()) == 1, "still one row")
    ctx.expect(len(rig.calls("POST", "jobs/")) == 2, "accept repeated")


@case(N, "jobs", "node-pull-tampered-object")
def _(ctx):
    """An object whose bytes do not hash to its name: not materialised, not queued, not accepted."""
    rig = ctx.rig
    job = rig.director.offer("node-dspec")
    rig.director.bad_objects.add(job["files"][0]["sha256"])
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect(out.get("accepted") == [] and [f["job_id"] for f in out.get("failed", [])] == [job["job_id"]], "failed", out)
    nothing_changed(ctx, rig, "tampered")


@case(N, "jobs", "node-pull-bad-bundle-sha")
def _(ctx):
    """A job whose bundle_sha256 does not recompute is refused before any object is fetched."""
    rig = ctx.rig
    job = rig.director.offer("node-dspec")
    job["bundle_sha256"] = "0" * 64
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect(out.get("accepted") == [] and "bundle_sha256" in json.dumps(out.get("failed")), "refused", out)
    ctx.expect(not rig.calls("GET", "objects/"), "nothing fetched")
    nothing_changed(ctx, rig, "bad bundle")


@case(N, "jobs", "node-accept-409")
def _(ctx):
    """An accept answered 409 is reported failed for that job, never as accepted."""
    rig = ctx.rig
    job = rig.director.offer("node-dspec")
    rig.director.accept_status = 409
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect(out.get("accepted") == [] and "409" in json.dumps(out.get("failed")), "409 reported", out)
    ctx.expect(rig.director.jobs[job["job_id"]]["state"] == "offered", "still offered on the Director")


@case(N, "jobs", "node-director-down")
def _(ctx):
    """An unreachable Director: job-pull is ok with an error and changes nothing."""
    rig = ctx.rig
    rig.director.offer("node-dspec")
    rig.director.down = True
    rc, out, _ = rig.op("job-pull")
    pulled(ctx, rig, rc, out)
    ctx.expect(out.get("error") and out.get("accepted") == [], "error, nothing accepted", out)
    nothing_changed(ctx, rig, "down")


# ---- §7: reporting ----
@case(N, "envelopes", "node-send-missing-upload-ack")
def _(ctx):
    """missing → the client PUTs exactly the named objects from the run dir → acked → entry moved to acked/."""
    rig = ctx.rig
    run = rig.spool_run("nspec-r3")
    rc, out, _ = rig.op("spool-send")
    ctx.expect(rc == 0 and out.get("sent") == 1 and out.get("failed") == [] and out.get("pending") == 0, "sent", out)
    puts = sorted(c["path"] for c in rig.calls("PUT", "objects/"))
    ctx.expect(puts == sorted("objects/" + sha(b) for b in run["files"].values()), "uploaded exactly the missing objects", puts)
    ctx.expect(all(rig.director.objects[sha(b)] == b for b in run["files"].values()), "with their bytes")
    posts = rig.calls("POST", "envelopes")
    sent = json.loads(posts[-1]["body"])
    ctx.expect(len(posts) == 2 and sent["row_line"] == run["row_line"], "posted twice, the ledger row byte for byte", posts)
    acked = rig.state / "outbox/acked"
    ctx.expect((acked / "nspec-r3.json").is_file() and json.loads((acked / "nspec-r3.ack.json").read_text())["envelope_sha256"]
               == run["envelope"]["envelope_sha256"], "moved to acked/ with its ack")


@case(N, "envelopes", "node-send-changed-trace")
def _(ctx):
    """A trace changed since listing stays pending and is rejected before upload."""
    rig = ctx.rig
    run_id = "nspec-changed-trace"
    trace = b'{"kind":"run"}\n'
    rig.spool_run(run_id, overrides={"trace.jsonl": trace})
    (rig.state / "runs" / run_id / "trace.jsonl").write_bytes(trace + trace)
    rc, out, _ = rig.op("spool-send")
    ctx.expect(rc == 0 and out.get("pending") == 1 and "trace.jsonl sha256 differs" in json.dumps(out.get("failed")),
               "changed trace stays pending with a hash diagnostic", out)
    ctx.expect(not rig.calls("PUT", "objects/" + sha(trace)), "changed trace bytes were never uploaded")


@case(N, "envelopes", "node-send-carries-usage")
def _(ctx):
    """The envelope posted is the outbox's, usage included unchanged (NODE-PROTOCOL.md §7.2)."""
    rig = ctx.rig
    run = rig.spool_run("nspec-r4")
    rig.op("spool-send")
    sent = json.loads(rig.calls("POST", "envelopes")[0]["body"])["envelope"]
    ctx.expect(sent.get("usage") == run["envelope"]["usage"] and sent["envelope_sha256"] == run["envelope"]["envelope_sha256"],
               "usage as written", sent)


@case(N, "envelopes", "node-send-rejected-or-conflict-kept")
def _(ctx):
    """rejected and conflict answers leave the entry in the outbox, reported failed."""
    rig = ctx.rig
    rig.spool_run("nspec-r5")
    for status in ("rejected", "conflict"):
        rig.director.envelope_answer = {"status": status, "error": "stand-in says " + status}
        rc, out, _ = rig.op("spool-send")
        ctx.expect(rc == 0 and out.get("sent") == 0 and out.get("pending") == 1 and status in json.dumps(out.get("failed")),
                   status + ": kept", out)
    ctx.expect(not (rig.state / "outbox/acked/nspec-r5.json").exists(), "never moved to acked/")


@case(N, "envelopes", "node-send-foreign-ack-kept")
def _(ctx):
    """An ack naming another envelope_sha256 is not taken as this entry's ack."""
    rig = ctx.rig
    run = rig.spool_run("nspec-r6")
    rig.director.envelope_answer = {"status": "acked", "ack": {"event_id": run["envelope"]["event_id"], "envelope_sha256": "f" * 64,
                                                              "verified_at": "2026-11-10T00:00:00Z", "restored": True}}
    rc, out, _ = rig.op("spool-send")
    ctx.expect(out.get("sent") == 0 and out.get("pending") == 1, "kept", out)


@case(N, "envelopes", "node-send-job-terminal")
def _(ctx):
    """A job_terminal entry posts its own row_line and is acked; the job leaves the stand-in's list."""
    rig = ctx.rig
    job = rig.director.offer("node-dspec")
    term = terminal_fixture(job["job_id"])
    outbox = rig.state / "outbox"
    outbox.mkdir(parents=True, exist_ok=True)
    (outbox / ("job_%s.json" % job["job_id"])).write_text(json.dumps(dict(term["envelope"], row_line=term["row_line"])))
    rc, out, _ = rig.op("spool-send")
    ctx.expect(rc == 0 and out.get("sent") == 1, "sent", out)
    body = json.loads(rig.calls("POST", "envelopes")[0]["body"])
    ctx.expect(body["row_line"] == term["row_line"] and "row_line" not in body["envelope"], "row_line beside, not inside", body)
    ctx.expect(rig.director.jobs[job["job_id"]]["state"] == "done", "job done on the Director")


@case(N, "envelopes", "node-send-director-down")
def _(ctx):
    """An unreachable Director: every entry stays, in order, for the next pass."""
    rig = ctx.rig
    rig.spool_run("nspec-r7")
    rig.spool_run("nspec-r8")
    rig.director.down = True
    rc, out, _ = rig.op("spool-send")
    ctx.expect(rc == 0 and out.get("sent") == 0 and out.get("pending") == 2 and len(out.get("failed")) == 1, "kept, stopped at the first", out)
