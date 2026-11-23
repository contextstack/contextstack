"""A run's live output to the Director (NODE-PROTOCOL.md §9.3). Not an op.

The Rust runner appends <run_dir>/events.jsonl while a model reply streams (harness/src/runner.rs `Events`): one line
per event, {"seq", "kind": "model.output", "payload": {"turn", "partial", "delta", "discard"?}}, seq 1, 2, 3, …
across the run. Each line maps to exactly one protocol event with the same seq, so the runner's file is the
durable queue and the node keeps only `confirmed` per run (<state>/node/events/<run_id>.json):
  partial true, delta d      {"kind": "delta", "partial": true, "payload": {"text": d, "turn": t}}
  partial false              {"kind": "stop", "partial": true, "payload": {"turn": t, "reason": "end_turn"}}
  partial false, discard     {"kind": "stop", "partial": true, "payload": {"turn": t, "reason": "discard",
                              "discard": true}}: the turn's deltas are void, as the runner says
  any other runner kind      passed through (kind and payload) when the kind is a protocol kind
send() posts what is past `confirmed`, at most 500 per call, sorted; the answer's `confirmed` is kept, so a gap,
a lost answer or a restart resends from confirmed + 1. A run is done once terminal.json exists and confirmed
covers the file; node-listen pumps live runs every second and spool-send flushes a run before its envelope.
A run belongs to a job by its frozen stimulus (stimulus/manifest.json source.job_id).
"""
import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path

from _estate_io import write_atomic

from _director_client import DirectorUnreachable
from _job_reason import redact_obj

KIND = re.compile(r"[a-z][a-z0-9_.]{0,63}")
BATCH, PAYLOAD_MAX = 500, 64 * 1024


def mark_path(state, run_id):
    return Path(state) / "node/events" / (run_id + ".json")


def read_mark(state, run_id):
    try:
        return json.loads(mark_path(state, run_id).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def write_mark(state, run_id, mark):
    path = mark_path(state, run_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    write_atomic(path, json.dumps(mark, sort_keys=True) + "\n")


def job_of(run_dir):
    try:
        manifest = json.loads((Path(run_dir) / "stimulus/manifest.json").read_text(encoding="utf-8"))
        return str((manifest.get("source") or {}).get("job_id") or "") or None
    except (OSError, ValueError, AttributeError):
        return None


def protocol_event(line):
    """One runner line as a protocol event, or None for a line that is not one."""
    if not isinstance(line, dict):
        return None
    seq, kind, payload = line.get("seq"), line.get("kind"), line.get("payload")
    if not isinstance(seq, int) or isinstance(seq, bool) or seq < 1 or not isinstance(payload, dict):
        return None
    if kind == "model.output":
        if payload.get("partial"):
            kind, out = "delta", {"text": str(payload.get("delta") or ""), "turn": payload.get("turn")}
        else:
            kind, out = "stop", {"turn": payload.get("turn"), "reason": "discard" if payload.get("discard") else "end_turn"}
            if payload.get("discard"):
                out["discard"] = True
    elif isinstance(kind, str) and KIND.fullmatch(kind):
        out = payload
    else:
        kind, out = "runner.event", {"kind": str(kind)[:64]}
    at = next((line[key] for key in ("at", "timestamp", "ts")
               if isinstance(line.get(key), str) and line[key]), None)
    source = "adapter_receive" if at is None or line.get("timestamp_source") == "adapter_receive" else "producer"
    if at is None:
        at = datetime.now(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")
    out = redact_obj(out)
    if len(json.dumps(out)) > PAYLOAD_MAX:
        out = {"turn": out.get("turn"), "text": str(out.get("text") or "")[:PAYLOAD_MAX // 2], "truncated": True}
    return {"seq": seq, "kind": kind, "partial": True, "at": at, "timestamp_source": source, "payload": out}


def lines(run_dir):
    """The runner's complete lines as protocol events, in seq order (a half-written last line waits)."""
    try:
        raw = (Path(run_dir) / "events.jsonl").read_bytes()
    except OSError:
        return []
    out = []
    for text in raw.split(b"\n")[:-1]:
        try:
            event = protocol_event(json.loads(text))
        except ValueError:
            event = None
        if event:
            out.append(event)
    return sorted(out, key=lambda e: e["seq"])


def send(client, state, run_dir, job_id=None):
    """Post the run's events past `confirmed`; returns True when the run is done (finished and all confirmed)."""
    run_dir = Path(run_dir)
    run_id, mark = run_dir.name, read_mark(state, run_dir.name)
    if mark.get("done"):
        return True
    job_id = job_id or mark.get("job_id") or job_of(run_dir)
    if not job_id:
        return False
    finished = (run_dir / "terminal.json").is_file()  # read before the file: nothing appended after it is lost
    events, confirmed = lines(run_dir), int(mark.get("confirmed") or 0)
    while True:
        batch = [e for e in events if e["seq"] > confirmed][:BATCH]
        if not batch:
            break
        try:
            answer = client.call("POST", "runs/%s/events" % run_id, {"job_id": job_id, "events": batch})
        except DirectorUnreachable as error:
            if client.protocol < 2:  # a protocol-1 Director (the client fell back): no live events there
                return False
            if error.status in (404, 409, 422):  # not this node's job, another's run, a body it refuses: stop
                write_mark(state, run_id, {**mark, "job_id": job_id, "done": True, "refused": error.status})
                return True
            raise
        got = int(answer.get("confirmed") or 0) if isinstance(answer, dict) else confirmed
        if got == confirmed:  # nothing recorded: the next pass tries again
            break
        confirmed = got  # higher: go on; lower (the Director holds less than the node thought): resend from there
    done = finished and confirmed >= (events[-1]["seq"] if events else 0)
    write_mark(state, run_id, {"job_id": job_id, "confirmed": confirmed, "done": done})
    return done


def live_runs(state, active):
    """{run_id: (run_dir, job_id)} for the active jobs' runs since each started, plus earlier runs not yet done."""
    runs, found = Path(state) / "runs", {}
    marks = Path(state) / "node/events"
    for mark in marks.glob("*.json") if marks.is_dir() else ():
        m = read_mark(state, mark.stem)
        if not m.get("done") and m.get("job_id") and (runs / mark.stem).is_dir():
            found[mark.stem] = (runs / mark.stem, m["job_id"])
    starts = {a["job_id"]: compact(a.get("started_at")) for a in active if a.get("state") == "running"}
    if not starts or not runs.is_dir():
        return found
    earliest = min(starts.values())
    for d in runs.iterdir():
        if d.name[:15] < earliest or d.name in found or (d / "terminal.json").is_file() and read_mark(state, d.name).get("done"):
            continue
        job = job_of(d)
        if job in starts and d.name[:15] >= starts[job]:
            found[d.name] = (d, job)
    return found


def compact(ts):
    """2026-11-10T19:00:05Z -> 20261110-190000: a run dir name's prefix, a minute early for clock slack."""
    t = str(ts or "")
    if not re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", t):
        return "00000000-000000"
    minute = int(t[14:16])
    hour, minute = (int(t[11:13]), minute - 1) if minute else (max(int(t[11:13]) - 1, 0), 59)
    return "%s%s%s-%02d%02d00" % (t[:4], t[5:7], t[8:10], hour, minute)
