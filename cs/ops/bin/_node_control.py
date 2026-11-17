"""Control messages from the Director (NODE-PROTOCOL.md §9.1; Director.Plan step 5). Not an op.

node-listen hands each message from GET work to handle(). The record comes first: <state>/node/control/<id>.json
is written (fsync, rename) before anything is acted on, and again with the outcome before it is reported. A message
met again (listed after a lost answer, after a restart) is never applied twice: its recorded outcome is reported.
Reports go in order, `received` then the outcome (POST control/<id>); a report that does not arrive is retried by
report_pending(). A 409 (the Director holds another outcome) or 404 settles the record's reporting: the first
outcome stands.

cancel {job_id}: the marker <state>/node/cancelled/<job> is written first, so job intake (_node_intake), tick's
admission and every trial skip the job from then on. Then, from the dispatch shelf:
  over already (a terminal row)    already_finished (or stopped, when the row is this cancel's own)
  not pulled, or queued            a terminal row {state: cancelled} under dispatch.lock: stopped
  running                          tick's children are stopped: for a gateway work order (the `executor` op) first
                                   POST <CSGATEWAY_LAB_ENDPOINT>/<job>/cancel (GATEWAY_CANCEL, below), then SIGTERM;
                                   the runner finalizes (terminal_state cancelled, cancel_reason SIGTERM), and after
                                   LAB_CANCEL_GRACE_S (30) anything left gets SIGKILL. tick writes the terminal row
                                   {state: cancelled} (or this module does, when tick is gone): stopped
Then spool-send delivers job_terminal (row_line.state cancelled) after the outcome is reported.
schedule_change {schedule_id, expect_revision, set}: applied only at the expected revision (_node_schedules).
Any other kind is refused.

GATEWAY_CANCEL is the node side of the gateway's cancel call, stubbed in the tests: POST
<endpoint>/<LAB_JOB_ID>/cancel {"reason"} (Bearer CSGATEWAY_AUTH_TOKEN when set) → 200 {"cancelled": bool}; 404 or
no answer means nothing to cancel there. Closing the executor op's request also stops the work order today.
"""
import fcntl
from _node_env import node_env
import json
import os
import re
import signal
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

from _estate_io import write_atomic

from _director_client import DirectorUnreachable
from _dispatch import TERMINAL, qid
from _estate_io import append_jsonl, pid_live, read_jsonl
from _job_reason import reason_of
from _node_spool import spool_job_terminal
from _roots import outbox_dir

CONTROL_ID = re.compile(r"ctl_[0-9a-z]{26}")  # lowercase base32 (director-rs: a ULID)
JOB_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
GRACE_S = float(os.environ.get("LAB_CANCEL_GRACE_S") or 30)
GATEWAY_CANCEL = "{endpoint}/{job_id}/cancel"
META = ("spec", "task", "schedule_id", "slot", "schedule_slot", "schedule_date", "schedule_timezone", "grant", "lane")


def now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def write_json(path, value):
    write_atomic(path, json.dumps(value, sort_keys=True) + "\n")


def read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def record_path(state, cid):
    return Path(state) / "node/control" / (cid + ".json")


def prepare(message, state):
    """The record, written before anything is acted on; for a cancel also the job's marker, so jobs listed in the same
    answer are never queued (node-listen calls this before it takes the answer's jobs). None: not a message."""
    cid = str(message.get("control_id") or "")
    if not CONTROL_ID.fullmatch(cid):
        return None
    path = record_path(state, cid)
    rec = read_json(path)
    if rec is None:
        rec = {"control_id": cid, "kind": message.get("kind"), "body": message.get("body"), "seen_at": now(),
               "outcome": None, "reported": []}
        write_json(record_path(state, rec["control_id"]), rec)
    job = str((rec.get("body") or {}).get("job_id") or "") if isinstance(rec.get("body"), dict) else ""
    if rec.get("kind") == "cancel" and not rec.get("outcome") and JOB_ID.fullmatch(job):
        write_json(Path(state) / "node/cancelled" / job, {"job_id": job, "control_id": cid, "ts": now()})
    return rec


def handle(client, message, state, on_schedule_change=None):
    """Record, report received, act once, record the outcome, report it. Returns the record (None: not a message)."""
    rec = prepare(message, state)
    if rec is None:
        return None
    if rec.get("outcome") and message.get("state") in ("requested", "received"):  # listed: the Director lacks it
        rec["reported"] = ["received"] if message.get("state") == "received" else []
    report(client, state, rec)  # received
    applied_now = False
    if not rec.get("outcome"):
        settled, outcome, detail, result = apply(rec, state, on_schedule_change)
        rec.update(state=settled, outcome=outcome, detail=detail[:4000], result=result, settled_at=now())
        write_json(record_path(state, rec["control_id"]), rec)
        applied_now = True
    report(client, state, rec)
    if applied_now:
        rec["applied_now"] = True
    return rec


def report(client, state, rec):
    """Send what is not yet reported: received, then the outcome. Unreachable: left for report_pending."""
    want = ["received"] + (["outcome"] if rec.get("outcome") else [])
    for step in want:
        if step in rec["reported"] or "settled" in rec["reported"]:
            continue
        body = {"state": "received"} if step == "received" else \
            {"state": rec["state"], "outcome": rec["outcome"], "detail": rec.get("detail") or ""}
        if step == "outcome" and rec.get("result") is not None:
            body["result"] = rec["result"]
        try:
            client.call("POST", "control/" + rec["control_id"], body)
            rec["reported"].append(step)
        except DirectorUnreachable as error:
            if error.status not in (404, 409, 422):
                return False
            rec["reported"].append("settled")  # the Director's record stands; nothing to resend
            rec["report_refused"] = {"status": error.status, "answer": error.answer}
        write_json(record_path(state, rec["control_id"]), rec)
    return True


def report_pending(client, state):
    """Retry every record whose outcome is not yet reported (a lost answer, a Director that was away)."""
    folder = Path(state) / "node/control"
    for path in sorted(folder.glob("ctl_*.json")) if folder.is_dir() else ():
        rec = read_json(path)
        if rec and rec.get("outcome") and "outcome" not in rec["reported"] and "settled" not in rec["reported"]:
            if not report(client, state, rec):
                return


def apply(rec, state, on_schedule_change):
    """(state, outcome, detail, result) for the message; safe to repeat after a crash before the outcome landed."""
    kind, body = rec.get("kind"), rec.get("body") if isinstance(rec.get("body"), dict) else {}
    if kind == "cancel":
        return cancel(state, str(body.get("job_id") or ""), rec["control_id"])
    if kind == "schedule_change":
        from _node_schedules import change
        out = change(state, body, rec["control_id"])
        if out[1] == "applied" and on_schedule_change:
            on_schedule_change()
        return out
    return "refused", "refused", "this node does not know control kind %r" % (kind,), None


# ---- cancel ----
def cancel(state, job_id, cid):
    if not JOB_ID.fullmatch(job_id):
        return "refused", "refused", "job_id %r is not a job id" % (job_id,), None
    marker = Path(state) / "node/cancelled" / job_id
    if not marker.is_file():
        write_json(marker, {"job_id": job_id, "control_id": cid, "ts": now()})
    shelf = Path(state) / "dispatch.jsonl"
    with locked(state):
        rows = [r for r in read_jsonl(shelf, strict=False) if job_id in (qid(r), str(r.get("director_job") or ""))]
        over = next((r for r in rows if r.get("state") in TERMINAL), None)
        if over is not None:
            if over.get("control_id") == cid or over.get("state") == "cancelled":
                return "applied", "stopped", "stopped earlier by this cancel", None
            return "applied", "already_finished", "the job was already %s" % over.get("state"), None
        running = next((r for r in reversed(rows) if r.get("state") == "running"), None)
        if running is None or not pid_live(int(running.get("pid") or 0)):
            where = "queued" if rows else "not pulled yet"
            close(state, job_id, cid, rows[-1] if rows else {}, "cancelled: %s; it will not run" % where)
            return "applied", "stopped", "the job was %s; it will never run here" % where, None
        tick = int(running["pid"])
    detail = stop_children(tick, job_id)
    deadline = time.time() + 30
    while time.time() < deadline and pid_live(tick) and not terminal_row(shelf, job_id):
        time.sleep(0.2)
    if not terminal_row(shelf, job_id):
        with locked(state):
            if not terminal_row(shelf, job_id):
                close(state, job_id, cid, running, "cancelled: running; the dispatcher did not close it")
    if (terminal_row(shelf, job_id) or {}).get("state") == "done":
        return "applied", "already_finished", "the run finished before it could be stopped", None
    return "applied", "stopped", detail, None


class locked:
    def __init__(self, state):
        self.path = Path(state) / "dispatch.lock"

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.f = open(self.path, "a")
        fcntl.flock(self.f.fileno(), fcntl.LOCK_EX)

    def __exit__(self, *exc):
        fcntl.flock(self.f.fileno(), fcntl.LOCK_UN)
        self.f.close()


def terminal_row(shelf, job_id):
    return next((r for r in read_jsonl(shelf, strict=False)
                 if r.get("state") in TERMINAL and job_id in (qid(r), str(r.get("director_job") or ""))), None)


def close(state, job_id, cid, prior, stop):
    """The cancelled terminal row (caller holds dispatch.lock), spooled as job_terminal like tick's own."""
    shelf = Path(state) / "dispatch.jsonl"
    row = {"ts": now(), "id": job_id, "job": job_id, "state": "cancelled", "pid": 0, "code": 1,
           "origin": "director-cancel", "verdict": "void", "stop": stop, "director_job": job_id, "control_id": cid,
           "error": {"class": "cancelled", "code": "director_cancel", "source": "node-listen", "retryable": False,
                     "detail": stop}}
    row["reason"] = reason_of(row)
    row.update({k: prior[k] for k in META if prior.get(k)})
    append_jsonl(shelf, row, sort_keys=False, compact=False, sync=True, encoding="utf-8")
    spool_job_terminal(outbox_dir(), shelf, row)


def children(pid):
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,command="], capture_output=True, text=True).stdout
    found = []
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) >= 2 and parts[1] == str(pid) and parts[0].isdigit():
            found.append((int(parts[0]), parts[2] if len(parts) > 2 else ""))
    return found


def gateway_cancel(job_id):
    """The gateway's work-order cancel (GATEWAY_CANCEL above); returns a word for the detail."""
    endpoint = os.environ.get("CSGATEWAY_LAB_ENDPOINT", "http://127.0.0.1:9200/v1/ai/lab-execute").rstrip("/")
    request = urllib.request.Request(GATEWAY_CANCEL.format(endpoint=endpoint, job_id=job_id), method="POST",
                                     data=json.dumps({"reason": "director cancel"}).encode(),
                                     headers={"Content-Type": "application/json"})
    token = node_env().get("CSGATEWAY_AUTH_TOKEN")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(request, timeout=5) as answer:
            return "gateway cancelled" if json.loads(answer.read() or b"{}").get("cancelled") else "gateway had none"
    except urllib.error.HTTPError as error:
        return "gateway had none" if error.code == 404 else "gateway cancel HTTP %d" % error.code
    except (OSError, ValueError) as error:
        return "gateway cancel unavailable (%s)" % error


def stop_children(tick, job_id):
    """SIGTERM tick's children (a gateway work order is cancelled there first), SIGKILL what outlives the grace."""
    notes, kids = [], children(tick)
    if any("/executor" in cmd for _, cmd in kids):
        notes.append(gateway_cancel(job_id))
    for pid, _ in kids:
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass
    deadline = time.time() + GRACE_S
    while time.time() < deadline and any(pid_live(pid) for pid, _ in kids):
        time.sleep(0.2)
    left = [pid for pid, _ in kids if pid_live(pid)]
    for pid in left:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    notes.append("SIGTERM to %d process(es)%s" % (len(kids), ", SIGKILL to %d after %ds" % (len(left), GRACE_S) if left else ""))
    return "running; " + "; ".join(notes) if kids else "running; between trials, no further trial starts"
