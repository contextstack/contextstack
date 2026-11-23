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
  running                          SIGTERM to the machine launcher, which forwards it to the harness.
                                   The harness cancels its CLI work order through the gateway. After
                                   CS_CANCEL_GRACE_S anything left gets SIGKILL. tick writes the terminal row
                                   {state: cancelled} (or this module does, when tick is gone): stopped
Then spool-send delivers job_terminal (row_line.state cancelled) after the outcome is reported.
schedule_change {schedule_id, expect_revision, set}: applied only at the expected revision (_node_schedules).
Any other kind is refused.

"""
import fcntl
import json
import os
import re
import signal
import subprocess
import time
from pathlib import Path

from _estate_io import write_atomic

from _director_client import DirectorUnreachable
from _dispatch import TERMINAL, qid
from _estate_io import append_jsonl, read_jsonl
from _job_identity import _pid_identity, _signal_pid
from _job_reason import reason_of
from _node_spool import spool_job_terminal
from _roots import outbox_dir

CONTROL_ID = re.compile(r"ctl_[0-9a-z]{26}")  # lowercase base32 (director-rs: a ULID)
JOB_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
GRACE_S = float(os.environ.get("CS_CANCEL_GRACE_S") or 30)
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
        tick = int((running or {}).get("pid") or 0)
        stored = (running or {}).get("pid_identity")
        identity = tuple(stored) if isinstance(stored, list) else stored
        current = _pid_identity(tick) if tick else None
        if current is not None and identity is None:
            return "refused", "refused", "cannot verify the running dispatcher's identity", None
        if current is None or current != identity:
            where = "queued" if rows else "not pulled yet"
            close(state, job_id, cid, rows[-1] if rows else {}, "cancelled: %s; it will not run" % where)
            return "applied", "stopped", "the job was %s; it will never run here" % where, None
    detail = stop_children(tick, identity)
    deadline = time.time() + 30
    while time.time() < deadline and _pid_identity(tick) == identity and not terminal_row(shelf, job_id):
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


def process_rows():
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,pgid="], capture_output=True, text=True, check=True).stdout
    return {pid: (parent, group) for pid, parent, group in
            (map(int, line.split()) for line in out.splitlines())}


def launcher_targets(tick, identity):
    """Bracket membership with identities and pin each member across escalation."""
    if _pid_identity(tick) != identity:
        return []
    rows = process_rows()
    launchers = {pid for pid, (parent, _) in rows.items() if parent == tick}
    candidates = {pid: _pid_identity(pid) for pid, (_, group) in rows.items()
                  if pid in launchers or group in launchers and rows[group][1] == group}
    verified = process_rows()
    if _pid_identity(tick) != identity:
        return []
    groups = {}
    for pid in launchers:
        if verified.get(pid) != rows[pid] or candidates[pid] is None or _pid_identity(pid) != candidates[pid]:
            continue
        # API runners can share tick's group. Signal only the direct child then.
        groups[pid] = {pid: candidates[pid]}
    for pid, birth in candidates.items():
        group = rows[pid][1]
        if (pid not in groups and group in groups and rows[group][1] == group
                and birth is not None and verified.get(pid, (None, None))[1] == group
                and _pid_identity(pid) == birth):
            groups[group][pid] = birth
    return [(pid if rows[pid][1] == pid else None, members) for pid, members in groups.items()]


def signal_launcher(members, sig):
    sent = [_signal_pid(pid, birth, sig) for pid, birth in members.items()]
    return any(sent)


def escalation_members(group, members):
    """Pick up forks during grace while a verified member still anchors the group."""
    if group is None:
        return members

    def anchors(rows):
        return {pid for pid, birth in members.items()
                if rows.get(pid, (None, None))[1] == group and _pid_identity(pid) == birth}

    rows = process_rows()
    before = anchors(rows)
    if not before:
        return members
    candidates = {pid: _pid_identity(pid) for pid, (_, pgid) in rows.items() if pgid == group}
    verified = process_rows()
    if not before.intersection(anchors(verified)):
        return members
    return {**members, **{pid: birth for pid, birth in candidates.items()
                          if pid not in members and verified.get(pid, (None, None))[1] == group
                          and birth is not None and _pid_identity(pid) == birth}}


def stop_children(tick, identity):
    """Keep waiting on verified members even after their launcher exits."""
    stopped = [(group, members) for group, members in launcher_targets(tick, identity)
               if signal_launcher(members, signal.SIGTERM)]
    deadline = time.monotonic() + GRACE_S
    left = [(group, members) for group, members in stopped if signal_launcher(members, 0)]
    while left and time.monotonic() < deadline:
        time.sleep(0.2)
        left = [(group, members) for group, members in left if signal_launcher(members, 0)]
    killed = sum(signal_launcher(escalation_members(group, members), signal.SIGKILL) for group, members in left)
    if not stopped:
        return "running; between trials, no further trial starts"
    detail = "running; SIGTERM to %d process(es)" % len(stopped)
    if killed:
        detail += ", SIGKILL to %d after %gs" % (killed, GRACE_S)
    return detail
