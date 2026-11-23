"""A Director's jobs into this node's dispatch shelf (protocol 2). Not an op.

job-pull (GET jobs, every tick) and node-listen (the held-open GET work) hand their job lists to take(): for a job
not yet here every object is fetched (GET objects/<sha256>) and verified (sha256 and size), bundle_sha256 is
recomputed over the sorted `path:sha256:mode` lines, the bundle is materialised in a temp dir and renamed to
<state>/jobs/<job_id>/ with job.json written and fsync'd last before the rename. Then (also for a job already here,
which makes a restart safe) a `pending` row {spec, task: absolute paths in the job dir, director_job} is in
<state>/dispatch.jsonl exactly once, under tick's dispatch.lock, and only then POST jobs/<job_id>/accept. A job is
never materialised twice and never re-queued once its row exists, so an accepted job is not run twice, whichever
of the two callers saw it first. A job the Director cancelled (a marker under <state>/node/cancelled/,
_node_control) is skipped: never materialised, queued or accepted.
A job naming a `grant` must carry grants/<id>.yaml hashing to its grant_sha256 (else it fails like a bad object).
The node then enforces it before queueing (_grant.load_file, live_error: active, unexpired, estate = this node's
ESTATE): a refused grant queues no run; the job is closed at once with a terminal row {state error, verdict void,
stop "grant: <remedy>", error class node_config} that tick's outbox channel (_node_spool) delivers.
`meta` schedule_id/slot/schedule_date/schedule_timezone and the grant id travel on the dispatch row.
"""
import fcntl
import hashlib
import json
import os
import re
import shutil
import tempfile
import time
from pathlib import Path

from _director_client import DirectorUnreachable
from _estate_io import append_jsonl, read_jsonl
from _grant import live_error, load_file, remedy
from _job_reason import reason_of
from _node_spool import cancelled, spool_job_terminal
from _roots import jobs_dir, outbox_dir, roots
from _vitals import load_resources_policy, read_vitals, resource_held

JOB_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
META = ("schedule_id", "slot", "schedule_date", "schedule_timezone")
GRANT_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")


def safe_rel(path):
    p = Path(str(path))
    if not str(path) or p.is_absolute() or any(part in ("", ".", "..") for part in p.parts):
        raise ValueError("bundle path %r is not a plain relative path" % (path,))
    return p


def bundle_sha(files):
    lines = "".join("%s:%s:%o\n" % (f["path"], f["sha256"], int(f["mode"])) for f in sorted(files, key=lambda f: f["path"]))
    return hashlib.sha256(lines.encode("utf-8")).hexdigest()


def fsync_dir(path):
    fd = os.open(str(path), os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def materialise(job, client, target):
    files = job.get("files") or []
    for name in ("spec", "task"):
        safe_rel(job.get(name))
    if bundle_sha(files) != job.get("bundle_sha256"):
        raise ValueError("bundle_sha256 does not match the listed files")
    grant = job.get("grant")
    if grant is not None:
        listed = {f["path"]: f["sha256"] for f in files}
        if not isinstance(grant, str) or not GRANT_ID.fullmatch(grant) or ".." in grant \
                or listed.get("grants/%s.yaml" % grant) != job.get("grant_sha256"):
            raise ValueError("grant %r is not carried in the bundle with its grant_sha256" % (grant,))
    target.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix="." + target.name + "-", dir=str(target.parent)))
    try:
        for f in files:
            raw = client.call("GET", "objects/" + f["sha256"])
            if isinstance(raw, dict) or hashlib.sha256(raw).hexdigest() != f["sha256"] or len(raw) != int(f["bytes"]):
                raise ValueError("object %s for %s does not verify" % (f["sha256"], f["path"]))
            dest = stage / safe_rel(f["path"])
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(raw)
            os.chmod(dest, 0o755 if int(f["mode"]) & 0o111 else 0o644)
        if not (stage / job["spec"]).is_file() or not (stage / job["task"] / "goal.md").is_file():
            raise ValueError("bundle lacks its spec or the task's goal.md")
        meta = job.get("meta") if isinstance(job.get("meta"), dict) else {}
        record = {"job_id": job["job_id"], "bundle_sha256": job["bundle_sha256"], "spec": job["spec"],
                  "task": job["task"], "accepted_ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                  "conversation_id": job.get("conversation_id"), "turn_id": job.get("turn_id"),
                  "grant": grant, "meta": {k: str(meta[k]) for k in META if meta.get(k)}}
        with open(stage / "job.json", "w", encoding="utf-8") as out:
            out.write(json.dumps(record, sort_keys=True) + "\n")
            out.flush()
            os.fsync(out.fileno())
        fsync_dir(stage)
        os.rename(stage, target)
        fsync_dir(target.parent)
    except BaseException:
        shutil.rmtree(stage, ignore_errors=True)
        raise


def ensure_queued(state, target):
    """The job's one pending row in the local shelf, appended under tick's dispatch lock."""
    record = json.loads((target / "job.json").read_text(encoding="utf-8"))
    shelf, lock = state / "dispatch.jsonl", state / "dispatch.lock"
    state.mkdir(parents=True, exist_ok=True)
    with open(lock, "a") as held:
        fcntl.flock(held.fileno(), fcntl.LOCK_EX)
        if any(str(r.get("director_job") or "") == record["job_id"] for r in read_jsonl(shelf, strict=False)):
            return False
        row = {"ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "id": record["job_id"],
               "spec": str(target / record["spec"]), "state": "pending", "pid": 0, "lane": "production",
               "note": "director job", "task": str(target / record["task"]),
               "director_job": record["job_id"], "origin": "director", **(record.get("meta") or {}),
               "conversation_id": record.get("conversation_id"), "turn_id": record.get("turn_id")}
        refusal = grant_refusal(target, record.get("grant"))
        if record.get("grant"):
            row["grant"] = record["grant"]
        if refusal:
            row.update(job=record["job_id"], state="error", code=1, verdict="void", stop="grant: " + refusal,
                       error={"class": "node_config", "code": "grant", "source": "job-pull", "retryable": False,
                              "detail": refusal[:512]})
            row["reason"] = reason_of(row)
        append_jsonl(shelf, row, sort_keys=False, compact=False, sync=True, encoding="utf-8")
        if refusal:
            spool_job_terminal(outbox_dir(), shelf, row)
        return True


def grant_refusal(target, gid):
    """'' when the job carries no grant or the node accepts it; else the remedy line (node enforces)."""
    if not gid:
        return ""
    grant, error = load_file(target / "grants" / (gid + ".yaml"), gid)
    if not error and not (grant or {}).get("estate"):
        error = "grant estate required"  # a node accepts only authority issued for it by name
    error = error or live_error(grant)
    return remedy(error, grant=grant or {}, gid=gid) if error else ""


def intake_blocked(state):
    try:
        found = roots()
        held = resource_held(read_vitals(state, found["config"]), load_resources_policy(found["config"]))
    except (OSError, ValueError, TypeError):
        held = None
    return held


def take(client, offered, state):
    """Queue and accept each offered job once; returns (accepted ids, newly queued ids, failures)."""
    if not offered:
        return [], [], []
    # This is the same state directory that holds dispatch.jsonl.
    state.mkdir(parents=True, exist_ok=True)
    with (state / "restart-intake.lock").open("a") as intake:
        try:
            fcntl.flock(intake, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except BlockingIOError:
            return [], [], []
        return _take(client, offered, state)


def _take(client, offered, state):
    accepted, queued, failed = [], [], []
    if intake_blocked(state):
        return accepted, queued, failed
    for job in offered:
        jid = str(job.get("job_id") or "")
        try:
            if not JOB_ID.fullmatch(jid):
                raise ValueError("job_id must match %s" % JOB_ID.pattern)
            if cancelled(state, jid):
                continue
            target = jobs_dir() / jid
            if not (target / "job.json").is_file():
                materialise(job, client, target)
            if ensure_queued(state, target):
                queued.append(jid)
            client.call("POST", "jobs/%s/accept" % jid, {"bundle_sha256": job.get("bundle_sha256")})
            accepted.append(jid)
        except DirectorUnreachable as error:
            failed.append({"job_id": jid, "error": str(error)})
            break
        except (OSError, ValueError, KeyError, TypeError) as error:
            failed.append({"job_id": jid, "error": "%s: %s" % (type(error).__name__, error)})
    return accepted, queued, failed
