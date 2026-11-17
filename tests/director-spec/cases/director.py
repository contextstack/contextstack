"""Director side: a Director implementation against NODE-PROTOCOL.md, over HTTP only."""
from harness import wire
from harness.run import case
from harness.wire import H_INSTALLATION, H_PROTOCOL, H_SUPPORTED, bundle_sha, run_fixture, sha, terminal_fixture

D = "director"


def refused(ctx, answer, status, code, what):
    ctx.expect(answer.status == status and (answer.json or {}).get("code") == code
               and all(isinstance((answer.json or {}).get(k), str) for k in ("error", "detail")),
               "%s: want %d %s with {error, code, detail}" % (what, status, code), answer)


def stamped(ctx, answer, node, what, accepted=True):
    ctx.expect(answer.header(H_INSTALLATION) == node.installation, what + ": names the installation", answer)
    ctx.expect(answer.header(H_SUPPORTED) is not None and "1" in answer.header(H_SUPPORTED).split(","),
               what + ": lists the supported versions, 1 among them", answer)
    ctx.expect((answer.header(H_PROTOCOL) == "1") == accepted, what + ": ContextStack-Protocol present exactly when accepted", answer)


def upload(ctx, node, run):
    for data in run["files"].values():
        a = node.call("PUT", "objects/" + sha(data), raw=data)
        ctx.expect(a.status == 200, "PUT objects", a)


def post(node, envelope, row_line):
    return node.call("POST", "envelopes", body={"envelope": envelope, "row_line": row_line})


# ---- §1 authentication ----
@case(D, "auth", "auth-missing-token")
def _(ctx):
    """No token: 401 unauthenticated, still stamped with the installation (§1, §2)."""
    n = ctx.node()
    a = n.call("GET", "jobs", token=None)
    refused(ctx, a, 401, "unauthenticated", "no token")
    stamped(ctx, a, n, "401", accepted=False)


@case(D, "auth", "auth-unknown-token")
def _(ctx):
    """An unknown token is 401, whatever protocol headers it carries."""
    n = ctx.node()
    refused(ctx, n.call("GET", "jobs", token="not-a-node-token"), 401, "unauthenticated", "unknown token")


@case(D, "auth", "auth-before-protocol")
def _(ctx):
    """Authentication comes first: no token and no protocol header is 401, not 426."""
    n = ctx.node()
    refused(ctx, n.call("GET", "jobs", token=None, headers={}), 401, "unauthenticated", "bare probe")


@case(D, "auth", "body-limit", ("rust",))
def body_limit(ctx):
    """An unauthenticated 10 MiB POST is refused before the body is read; a normal route over 1 MiB is 413."""
    import socket
    import time
    import urllib.parse

    url = urllib.parse.urlsplit(ctx.target.url)
    length = 10 * 1024 * 1024
    raw = (
        "POST /api/v1/nodes/me/envelopes HTTP/1.1\r\n"
        "Host: %s\r\n"
        "Content-Type: application/json\r\n"
        "Content-Length: %d\r\n"
        "Connection: close\r\n"
        "\r\n" % (url.hostname, length)
    ).encode()
    started = time.monotonic()
    sock = socket.create_connection((url.hostname, url.port), timeout=2)
    try:
        sock.settimeout(2)
        sock.sendall(raw)
        data = b""
        while b"\r\n" not in data:
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
    finally:
        sock.close()
    elapsed = time.monotonic() - started
    line = data.split(b"\r\n", 1)[0].decode("latin1") if data else ""
    parts = line.split()
    status = int(parts[1]) if len(parts) >= 2 and line.startswith("HTTP/") else 0
    ctx.expect(
        status in (401, 413) and elapsed < 2,
        "unauthenticated 10 MiB refused without reading the body",
        (status, round(elapsed, 3), line[:200]),
    )

    node = ctx.node()
    oversized = node.call("POST", "jobs/no-such/accept", raw=b"x" * (1024 * 1024 + 1))
    refused(ctx, oversized, 413, "payload_too_large", "normal route over 1 MiB")


# ---- §2 protocol version and installation ID ----
@case(D, "protocol", "protocol-accepted-headers")
def _(ctx):
    """A protocol-1 request is answered in protocol 1 and names the installation registration returned."""
    n = ctx.node()
    a = n.call("GET", "jobs")
    ctx.expect(a.status == 200, "GET jobs", a)
    stamped(ctx, a, n, "200")
    ctx.expect(wire.INSTALLATION_RE.fullmatch(n.installation or ""), "installation ID shape", n.installation)


@case(D, "protocol", "protocol-installation-stable")
def _(ctx):
    """Every node of one Director is given the same installation ID, and every answer names it."""
    a, b = ctx.node("a"), ctx.node("b")
    ctx.expect(a.installation == b.installation, "one installation per Director", (a.installation, b.installation))
    ctx.expect(b.call("GET", "jobs").header(H_INSTALLATION) == a.installation, "answers name it")


@case(D, "protocol", "protocol-missing-426")
def _(ctx):
    """No ContextStack-Protocol header: 426 protocol_unsupported with the supported list."""
    n = ctx.node()
    a = n.call("GET", "jobs", headers=n.headers(**{H_PROTOCOL: None}))
    refused(ctx, a, 426, "protocol_unsupported", "no version")
    ctx.expect((a.json.get("protocol") or {}).get("supported") and 1 in a.json["protocol"]["supported"]
               and a.json["protocol"].get("node") is None, "body names protocol {node: null, supported}", a)
    stamped(ctx, a, n, "426", accepted=False)


@case(D, "protocol", "protocol-unsupported-426")
def _(ctx):
    """Older (0), newer (99) and non-numeric versions are each refused with 426 on every call."""
    n = ctx.node()
    for version in ("0", "99", "v1", "1.0", ""):
        for method, path, body in (("GET", "jobs", None), ("POST", "envelopes", {})):
            a = n.call(method, path, body=body, headers=n.headers(**{H_PROTOCOL: version}))
            refused(ctx, a, 426, "protocol_unsupported", "version %r on %s %s" % (version, method, path))


@case(D, "protocol", "installation-mismatch-421")
def _(ctx):
    """A missing or different installation ID is 421 installation_mismatch naming the Director's own."""
    n = ctx.node()
    for value in (None, "inst-another-director"):
        a = n.call("GET", "jobs", headers=n.headers(**{H_INSTALLATION: value}))
        refused(ctx, a, 421, "installation_mismatch", "installation %r" % value)
        ctx.expect(a.json.get("installation") == n.installation, "body names this Director's installation", a)


@case(D, "protocol", "protocol-before-installation")
def _(ctx):
    """With both wrong, the version is judged first (426)."""
    n = ctx.node()
    refused(ctx, n.call("GET", "jobs", headers={H_PROTOCOL: "7"}), 426, "protocol_unsupported", "both wrong")


@case(D, "protocol", "refusal-changes-nothing")
def _(ctx):
    """A refused accept (426 or 421) leaves the job offered."""
    n = ctx.node()
    jid = n.offer()
    digest = n.job(jid)["bundle_sha256"]
    for headers in (n.headers(**{H_PROTOCOL: "99"}), n.headers(**{H_INSTALLATION: "inst-x"})):
        n.call("POST", "jobs/%s/accept" % jid, body={"bundle_sha256": digest}, headers=headers)
    a = n.call("POST", "jobs/%s/accept" % jid, body={"bundle_sha256": digest})
    ctx.expect(a.status == 200 and a.json == {"job_id": jid, "state": "accepted"}, "first real accept still moves it", a)


# ---- §4 GET jobs ----
@case(D, "jobs", "jobs-empty")
def _(ctx):
    """A node with no work gets {"jobs": []}."""
    a = ctx.node().call("GET", "jobs")
    ctx.expect(a.status == 200 and a.json == {"jobs": []}, "empty list", a)


@case(D, "jobs", "jobs-shape")
def _(ctx):
    """An offered job is listed with every field; files sorted; bundle_sha256 recomputes; meta is an object."""
    n = ctx.node()
    jid = n.offer()
    a = n.call("GET", "jobs")
    ctx.expect(a.status == 200 and (a.header("content-type") or "").startswith("application/json"), "GET jobs JSON", a)
    jobs = a.json["jobs"]
    ctx.expect(len(jobs) == 1 and set(jobs[0]) == {"job_id", "spec", "task", "trials", "bundle_sha256", "files", "grant",
                                                   "grant_sha256", "meta"}, "exactly the protocol-1 fields", jobs)
    j = jobs[0]
    ctx.expect(j["job_id"] == jid and j["spec"] == "spec.yaml" and j["task"] == "tasks/t" and j["trials"] == 1
               and j["grant"] is None and j["grant_sha256"] is None and j["meta"] == {} and b'"meta":{}' in a.body.replace(b" ", b""),
               "values (meta an object, not a list)", j)
    paths = [f["path"] for f in j["files"]]
    ctx.expect(paths == sorted(paths) and paths == sorted(wire.JOB_FILES), "files sorted, the whole bundle", paths)
    ctx.expect(all(set(f) == {"path", "sha256", "bytes", "mode"} for f in j["files"])
               and {f["path"]: f["mode"] for f in j["files"]} == {p: m for p, (_, m) in wire.JOB_FILES.items()}
               and all(f["sha256"] == sha(wire.JOB_FILES[f["path"]][0]) and f["bytes"] == len(wire.JOB_FILES[f["path"]][0]) for f in j["files"]),
               "file entries {path, sha256, bytes, mode}", j["files"])
    ctx.expect(j["bundle_sha256"] == bundle_sha(j["files"]), "bundle_sha256 over sorted path:sha256:mode lines", j)


@case(D, "jobs", "jobs-budget-in-spec")
def _(ctx):
    """The job's dollar budget travels inside the frozen spec.yaml (§4), byte for byte."""
    n = ctx.node()
    jid = n.offer()
    spec = next(f for f in n.job(jid)["files"] if f["path"] == "spec.yaml")
    a = n.call("GET", "objects/" + spec["sha256"])
    ctx.expect(a.status == 200 and b"usd: 0.25" in a.body and sha(a.body) == spec["sha256"], "spec.yaml carries budget.usd", a)


@case(D, "jobs", "jobs-scoped-to-node")
def _(ctx):
    """A node sees only its own jobs."""
    a, b = ctx.node("a"), ctx.node("b")
    a.offer()
    ctx.expect(b.call("GET", "jobs").json == {"jobs": []}, "b sees none of a's jobs")


# ---- §5 accept ----
@case(D, "accept", "accept-idempotent")
def _(ctx):
    """Accept moves offered to accepted; a repeat is the same answer; the job stays listed."""
    n = ctx.node()
    jid = n.offer()
    digest = n.job(jid)["bundle_sha256"]
    for attempt in (1, 2):
        a = n.call("POST", "jobs/%s/accept" % jid, body={"bundle_sha256": digest})
        ctx.expect(a.status == 200 and a.json == {"job_id": jid, "state": "accepted"}, "accept #%d" % attempt, a)
    ctx.expect([j["job_id"] for j in n.call("GET", "jobs").json["jobs"]] == [jid], "accepted job still listed")


@case(D, "accept", "accept-wrong-bundle-409")
def _(ctx):
    """A bundle_sha256 that is not the job's is 409 conflict and changes nothing."""
    n = ctx.node()
    jid = n.offer()
    refused(ctx, n.call("POST", "jobs/%s/accept" % jid, body={"bundle_sha256": "0" * 64}), 409, "conflict", "wrong sha")
    a = n.call("POST", "jobs/%s/accept" % jid, body={"bundle_sha256": n.job(jid)["bundle_sha256"]})
    ctx.expect(a.status == 200 and a.json["state"] == "accepted", "a correct accept afterwards works", a)


@case(D, "accept", "accept-missing-bundle-422")
def _(ctx):
    """Accept without bundle_sha256 is 422 invalid_request."""
    n = ctx.node()
    jid = n.offer()
    refused(ctx, n.call("POST", "jobs/%s/accept" % jid, body={}), 422, "invalid_request", "no bundle_sha256")


@case(D, "accept", "accept-foreign-or-unknown-404")
def _(ctx):
    """Another node's job, or no such job, is 404 not_found; the other job stays offered."""
    a, b = ctx.node("a"), ctx.node("b")
    jid = a.offer()
    digest = a.job(jid)["bundle_sha256"]
    refused(ctx, b.call("POST", "jobs/%s/accept" % jid, body={"bundle_sha256": digest}), 404, "not_found", "foreign job")
    refused(ctx, b.call("POST", "jobs/NOSUCHJOB/accept", body={"bundle_sha256": digest}), 404, "not_found", "unknown job")
    # still offered: a's first accept is the one that moves it
    ctx.expect(a.call("POST", "jobs/%s/accept" % jid, body={"bundle_sha256": digest}).json == {"job_id": jid, "state": "accepted"}, "a accepts")


# ---- §6 objects ----
@case(D, "objects", "object-get-bundle-file")
def _(ctx):
    """A file of this node's bundle is served as its exact bytes, application/octet-stream."""
    n = ctx.node()
    jid = n.offer()
    for f in n.job(jid)["files"]:
        a = n.call("GET", "objects/" + f["sha256"])
        ctx.expect(a.status == 200 and sha(a.body) == f["sha256"] and len(a.body) == f["bytes"]
                   and (a.header("content-type") or "").startswith("application/octet-stream"), "GET " + f["path"], a)


@case(D, "objects", "object-get-scoped-404")
def _(ctx):
    """Objects outside this node's bundles are 404: another node's, merely uploaded, absent, malformed name."""
    a, b = ctx.node("a"), ctx.node("b")
    jid = a.offer()
    theirs = a.job(jid)["files"][0]["sha256"]
    stray = b"uploaded by b, in no bundle %s\n" % ctx.id.encode()
    ctx.expect(b.call("PUT", "objects/" + sha(stray), raw=stray).status == 200, "b uploads")
    for what, name in (("another node's bundle file", theirs), ("only uploaded", sha(stray)), ("absent", "e" * 64)):
        refused(ctx, b.call("GET", "objects/" + name), 404, "not_found", what)
    ctx.expect(b.call("GET", "objects/NOT-A-SHA").status == 404, "malformed name 404")


@case(D, "objects", "object-put")
def _(ctx):
    """PUT stores bytes only when they hash to the name; a repeat says stored false."""
    n = ctx.node()
    data = b"evidence %s\n" % ctx.id.encode()
    refused(ctx, n.call("PUT", "objects/" + sha(b"other"), raw=data), 422, "invalid_request", "wrong hash")
    for stored in (True, False):
        a = n.call("PUT", "objects/" + sha(data), raw=data)
        ctx.expect(a.status == 200 and a.json == {"sha256": sha(data), "stored": stored}, "stored %s" % stored, a)


# ---- §7 envelopes ----
@case(D, "envelopes", "envelope-missing-then-acked")
def _(ctx):
    """Missing objects are named; after upload the run is acked; a repeat is the identical answer."""
    n = ctx.node()
    run = run_fixture("dspec-%s-r1" % ctx.serial_tag())
    a = post(n, run["envelope"], run["row_line"])
    ctx.expect(a.status == 200 and a.json.get("status") == "missing"
               and sorted(a.json.get("missing") or []) == sorted({sha(b) for b in run["files"].values()}), "missing lists every object", a)
    upload(ctx, n, run)
    acked = post(n, run["envelope"], run["row_line"])
    ack = (acked.json or {}).get("ack") or {}
    ctx.expect(acked.status == 200 and acked.json.get("status") == "acked" and ack.get("event_id") == run["envelope"]["event_id"]
               and ack.get("envelope_sha256") == run["envelope"]["envelope_sha256"] and ack.get("restored") is True
               and wire.UTC_RE.fullmatch(ack.get("verified_at") or ""), "ack {event_id, envelope_sha256, verified_at, restored}", acked)
    again = post(n, run["envelope"], run["row_line"])
    ctx.expect(again.status == 200 and again.json == acked.json, "repeat: identical answer", (acked, again))


@case(D, "envelopes", "envelope-null-row")
def _(ctx):
    """row_sha256 null: the run is acked without a row."""
    n = ctx.node()
    run = run_fixture("dspec-%s-r2" % ctx.serial_tag())
    run["envelope"]["row_sha256"] = None
    upload(ctx, n, run)
    ctx.expect(post(n, run["envelope"], None).json.get("status") == "acked", "acked")


@case(D, "envelopes", "envelope-repeat-not-rejudged")
def _(ctx):
    """An acked event repeated with the same envelope_sha256 gets the same ack even if row_line now differs:
    the row is judged once, when the event settles, and is never recorded twice (§7)."""
    n = ctx.node()
    run = run_fixture("dspec-%s-r4" % ctx.serial_tag())
    upload(ctx, n, run)
    first = post(n, run["envelope"], run["row_line"])
    ctx.expect(first.json.get("status") == "acked", "first ack", first)
    again = post(n, run["envelope"], run["row_line"] + " ")
    ctx.expect(again.json == first.json, "the same answer", again)


@case(D, "envelopes", "envelope-conflict")
def _(ctx):
    """An acked event re-sent with another envelope_sha256, or by another node, is conflict."""
    a, b = ctx.node("a"), ctx.node("b")
    rid = "dspec-%s-r3" % ctx.serial_tag()
    run = run_fixture(rid)
    upload(ctx, a, run)
    ctx.expect(post(a, run["envelope"], run["row_line"]).json.get("status") == "acked", "first ack")
    other = run_fixture(rid, overrides={"transcript.md": b"# another transcript\n"})
    upload(ctx, a, other)
    c = post(a, other["envelope"], other["row_line"])
    ctx.expect(c.status == 200 and c.json.get("status") == "conflict" and isinstance(c.json.get("error"), str), "other sha: conflict", c)
    c = post(b, run["envelope"], run["row_line"])
    ctx.expect(c.json.get("status") == "conflict", "same event from another node: conflict", c)


def spoilers():
    def edit(fn):
        def spoil(run):
            fn(run)
            return run
        return spoil

    def drop(key):
        return edit(lambda r: r["envelope"].pop(key))

    def set_(key, value):
        return edit(lambda r: r["envelope"].__setitem__(key, value))

    def usage(**over):
        return edit(lambda r: r["envelope"].__setitem__("usage", wire.usage(**over)))

    return {
        "not an object": lambda r: dict(r, envelope=[1, 2]),
        "envelope_version 2": set_("envelope_version", 2),
        "bad event_id": set_("event_id", "run_done:x"),
        "run_id differs": set_("run_id", "another-run"),
        "run_id with a path": edit(lambda r: r["envelope"].update(event_id="run_finalized:../x", run_id="../x")),
        "envelope_sha256 differs": set_("envelope_sha256", "a" * 64),
        "object bytes differ": edit(lambda r: r["envelope"]["files"][0].__setitem__("bytes", r["envelope"]["files"][0]["bytes"] + 1)),
        "absolute path": edit(lambda r: r["envelope"]["files"][0].__setitem__("path", "/etc/passwd")),
        "dot-dot path": edit(lambda r: r["envelope"]["files"][0].__setitem__("path", "a/../b")),
        "path twice": edit(lambda r: r["envelope"]["files"].append(dict(r["envelope"]["files"][0]))),
        "row_line does not hash": edit(lambda r: r.__setitem__("row_line", r["row_line"] + " ")),
        "row_line not an object": edit(lambda r: (r.__setitem__("row_line", "[1]"), r["envelope"].__setitem__("row_sha256", sha("[1]")))),
        "usage missing": drop("usage"),
        "usage a list": set_("usage", [1]),
        "usage tokens negative": usage(input_tokens=-1),
        "usage tokens a float": usage(output_tokens=1.5),
        "usage charge negative": usage(charged_usd=-0.01),
        "usage est a string": usage(est_usd="0.01"),
        "usage key missing": edit(lambda r: r["envelope"]["usage"].pop("est_basis")),
        "usage source class unknown": usage(source={"class": "free", "model": None}),
        "usage source missing model": usage(source={"class": "api"}),
    }


@case(D, "envelopes", "envelope-rejected")
def _(ctx):
    """Each malformed or unverifiable run envelope is 200 rejected with an error, and records no ack."""
    n = ctx.node()
    for i, (label, spoil) in enumerate(spoilers().items()):
        rid = "dspec-%s-rej-%d" % (ctx.serial_tag(), i)
        run = run_fixture(rid)
        upload(ctx, n, run)
        bad = spoil(run)
        a = post(n, bad["envelope"], bad["row_line"])
        ctx.expect(a.status == 200 and (a.json or {}).get("status") == "rejected" and isinstance(a.json.get("error"), str),
                   "rejected: " + label, a)
        good = run_fixture(rid)
        ok = post(n, good["envelope"], good["row_line"])
        ctx.expect(ok.json.get("status") == "acked", "after %s, the good envelope still acks (nothing recorded)" % label, ok)


@case(D, "envelopes", "envelope-not-restorable")
def _(ctx):
    """A run without terminal.json naming it, transcript.md, check.json, or stimulus/manifest.json is rejected."""
    n = ctx.node()
    tag = ctx.serial_tag()
    cases = {"no terminal.json": dict(drop=("terminal.json",)), "no transcript.md": dict(drop=("transcript.md",)),
             "no check.json": dict(drop=("check.json",)), "stimulus without manifest": dict(drop=("stimulus/manifest.json",)),
             "terminal names another run": dict(overrides={"terminal.json": b'{"run_id": "someone-else"}'})}
    for i, (label, kw) in enumerate(cases.items()):
        run = run_fixture("dspec-%s-nr%d" % (tag, i), **kw)
        upload(ctx, n, run)
        a = post(n, run["envelope"], run["row_line"])
        ctx.expect(a.json.get("status") == "rejected", label, a)


@case(D, "envelopes", "envelope-usage-nulls")
def _(ctx):
    """Usage with every figure null ("not reported") and any of the three source classes is accepted."""
    n = ctx.node()
    for i, cls in enumerate(("api", "subscription", None)):
        run = run_fixture("dspec-%s-u%d" % (ctx.serial_tag(), i))
        run["envelope"]["usage"] = {"input_tokens": None, "output_tokens": None, "est_usd": None, "est_basis": None,
                                    "charged_usd": None, "source": {"class": cls, "model": None}, "future_key": 1}
        upload(ctx, n, run)
        ctx.expect(post(n, run["envelope"], run["row_line"]).json.get("status") == "acked", "class %r" % cls)


@case(D, "envelopes", "job-terminal")
def _(ctx):
    """job_terminal acks once, takes the job off GET jobs, repeats identically; files or a missing row are rejected."""
    n = ctx.node()
    jid = n.offer()
    term = terminal_fixture(jid)
    bad = dict(term["envelope"], files=[{"path": "x", "sha256": sha(b"x"), "bytes": 1}])
    ctx.expect(post(n, bad, term["row_line"]).json.get("status") == "rejected", "files on job_terminal rejected")
    ctx.expect(post(n, dict(term["envelope"], row_sha256=None), None).json.get("status") == "rejected", "row required")
    first = post(n, term["envelope"], term["row_line"])
    ctx.expect(first.json.get("status") == "acked", "acked", first)
    ctx.expect(post(n, term["envelope"], term["row_line"]).json == first.json, "repeat identical")
    ctx.expect(n.call("GET", "jobs").json == {"jobs": []}, "job left the list")


@case(D, "envelopes", "job-terminal-foreign")
def _(ctx):
    """job_terminal for another node's job is rejected and that job stays listed for its own node."""
    a, b = ctx.node("a"), ctx.node("b")
    jid = a.offer()
    term = terminal_fixture(jid)
    ctx.expect(post(b, term["envelope"], term["row_line"]).json.get("status") == "rejected", "rejected")
    ctx.expect([j["job_id"] for j in a.call("GET", "jobs").json["jobs"]] == [jid], "still a's offered job")


@case(D, "envelopes", "envelope-garbage-body")
def _(ctx):
    """A body that is not JSON is still a 200 RPC answer: rejected."""
    n = ctx.node()
    a = n.call("POST", "envelopes", body=b"{not json")
    ctx.expect(a.status == 200 and a.json.get("status") == "rejected", "rejected", a)
