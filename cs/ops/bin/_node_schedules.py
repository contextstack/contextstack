"""The node's schedules as protocol 2 reports them (NODE-PROTOCOL.md §9.5). Not an op.

A schedule's `revision` is the count of its rows in <state>/schedules.jsonl: every change appends a row (enable,
hold, a Director's schedule_change), so the revision rises with every change and never needs storing apart.
report() builds the complete set every time, and the firings new or changed since `sent` (firing digests the
caller keeps). A firing is `<schedule_id>@<slot>`, and its state comes from schedule-fires.jsonl and the dispatch
shelf:
  firing_recorded row only    recorded while schedule-tick holds schedule.lock, else uncertain (it crashed between
                              recording the identity and starting the job; schedule-tick never re-fires it blindly)
  fire row {job}              started (queued or running), done, or error (error, cancelled, operator_close)
  deferral {reason: missed}   missed (skipped under `missed: skip`; schedule-tick, missed_grace_minutes)
change() applies a schedule_change control message (§9.5) only at its expect_revision: it appends a row naming the
control_id, so a message met again after a crash is recognised as applied, never applied twice.
"""
import fcntl
import hashlib
import json
import re
import time
from pathlib import Path

from _dispatch import TERMINAL, qid
from _estate_io import append_jsonl, read_jsonl

PLAIN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
SHOWN = ("cron", "timezone", "spec", "task")
MAX = 1000


def rows(state):
    path = Path(state) / "schedules.jsonl"
    return [r for r in read_jsonl(path, strict=False) if isinstance(r.get("id"), str)] if path.is_file() else []


def now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


class schedule_lock:
    def __init__(self, state, block=True):
        self.path, self.block, self.held = Path(state) / "schedule.lock", block, False

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.f = open(self.path, "a")
        try:
            fcntl.flock(self.f.fileno(), fcntl.LOCK_EX if self.block else fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.held = True
        except BlockingIOError:
            pass
        return self

    def __exit__(self, *exc):
        if self.held:
            fcntl.flock(self.f.fileno(), fcntl.LOCK_UN)
        self.f.close()


def change(state, body, cid):
    """(state, outcome, detail, result) for a schedule_change."""
    sid, expect, want = body.get("schedule_id"), body.get("expect_revision"), body.get("set")
    if not isinstance(want, dict) or not want or set(want) - {"enabled", "missed"} \
            or ("enabled" in want and not isinstance(want["enabled"], bool)) \
            or ("missed" in want and want["missed"] not in ("skip", "catch_up")) \
            or not isinstance(expect, int) or isinstance(expect, bool):
        return "refused", "refused", "set must hold enabled (boolean) and/or missed (skip | catch_up)", None
    with schedule_lock(state):
        mine = [r for r in rows(state) if r["id"] == sid]
        if not mine:
            return "refused", "unknown_schedule", "no schedule %r on this node" % (sid,), None
        done = [i for i, r in enumerate(mine) if r.get("control_id") == cid]
        if done:  # applied before a restart: report it, never apply it again
            return "applied", "applied", "applied earlier", {"revision": done[0] + 1}
        if len(mine) != expect:
            return "refused", "revision_mismatch", "schedule %s is at revision %d, not %d" % (sid, len(mine), expect), None
        row = {**mine[-1], **want, "event": "schedule_updated", "ts": now(), "actor_id": "director:control",
               "actor_kind": "automated", "control_id": cid}
        append_jsonl(Path(state) / "schedules.jsonl", row, sort_keys=True, compact=False, sync=True, encoding="utf-8")
    return "applied", "applied", "set %s" % json.dumps(want, sort_keys=True), {"revision": expect + 1}


def schedules(all_rows):
    latest, count = {}, {}
    for r in all_rows:
        latest[r["id"]], count[r["id"]] = r, count.get(r["id"], 0) + 1
    out = []
    for sid, r in latest.items():
        if not PLAIN.fullmatch(sid):
            continue
        s = {"schedule_id": sid, "revision": count[sid], "enabled": r.get("enabled") is True,
             "missed": r.get("missed") if r.get("missed") in ("skip", "catch_up") else "skip",
             "definition": {k: v for k, v in r.items() if k not in SHOWN + ("id", "enabled", "missed", "control_id")}}
        s.update({k: str(r[k]) for k in SHOWN if r.get(k) is not None})
        out.append(s)
    return out[:MAX]


def firings(state, all_rows):
    path, shelf = Path(state) / "schedule-fires.jsonl", Path(state) / "dispatch.jsonl"
    fires = read_jsonl(path, strict=False) if path.is_file() else []
    jobs = {}
    for r in read_jsonl(shelf, strict=False) if shelf.is_file() else ():
        if qid(r) and (qid(r) not in jobs or jobs[qid(r)].get("state") not in TERMINAL):
            jobs[qid(r)] = r
    with schedule_lock(state, block=False) as lock:
        idle = lock.held  # schedule-tick is not between recording a firing and starting its job
    found = {}
    for r in fires:
        sid, slot = str(r.get("id") or ""), str(r.get("slot") or "")
        if not PLAIN.fullmatch(sid) or not slot or r.get("job") == "dry-run":
            continue
        f = found.setdefault((sid, slot), {"firing_id": "%s@%s" % (sid, slot), "schedule_id": sid, "slot": slot,
                                           "state": None, "recorded_at": r.get("ts") or now()})
        if r.get("event") == "firing_recorded":
            f["schedule_revision"] = r.get("schedule_revision")
            f["state"] = f["state"] or "recorded"
        elif r.get("event") == "schedule_deferred" and r.get("reason") == "missed" and f["state"] in (None, "missed"):
            f["state"], f["detail"] = "missed", "skipped: missed while the node was off (missed: skip)"
        elif r.get("job") and r.get("event") != "schedule_deferred":
            row = jobs.get(str(r["job"])) or {}
            f["job_id"], f["run_id"] = str(r["job"]), row.get("run_id")
            f["state"] = {"done": "done", None: "started", "pending": "started", "running": "started"}.get(
                row.get("state"), "error")
            if r.get("catch_up"):
                f["catch_up"] = True
    out = []
    for f in found.values():
        if f["state"] is None:
            continue
        if f["state"] == "recorded" and idle:
            f["state"], f["detail"] = "uncertain", "identity recorded, job not seen: never re-run without an operator"
        out.append({k: v for k, v in f.items() if v is not None})
    return out


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()[:16]


def report(state, sent):
    """(body, digests to remember once the Director accepts it) for POST schedules."""
    all_rows = rows(state)
    fresh = [f for f in firings(state, all_rows) if sent.get(f["firing_id"]) != digest(f)][:MAX]
    return ({"schedules": schedules(all_rows), "firings": fresh},
            {**sent, **{f["firing_id"]: digest(f) for f in fresh}})
