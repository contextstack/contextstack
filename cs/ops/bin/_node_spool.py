"""A director job's terminal dispatch row as an outbox event (CS phase 6). Not an op.

tick (every terminal row of a director job) and job-pull (a job refused before it could run, e.g. by
its grant) write `job_terminal:<job>` here, so the director learns every outcome through the same
at-least-once channel as run envelopes. First terminal wins: an existing entry, pending or acked,
is never overwritten.
"""
import hashlib
import json
import os
import re
from pathlib import Path

from _estate_io import estate_row, write_atomic

JOB_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")


def spool_job_terminal(outbox, shelf, row):
    job = str(row.get("director_job") or "")
    if not JOB_ID.fullmatch(job):
        return False
    name = "job_%s.json" % job
    if (outbox / name).is_file() or (outbox / "acked" / name).is_file():
        return False
    line = json.dumps(estate_row(shelf, row))
    entry = {"envelope_version": 1, "event_id": "job_terminal:" + job, "kind": "job_terminal", "job_id": job,
             "run_id": row.get("run_id"), "files": [], "envelope_sha256": hashlib.sha256(b"").hexdigest(),
             "row_sha256": hashlib.sha256(line.encode("utf-8")).hexdigest(), "row_line": line}
    outbox.mkdir(parents=True, exist_ok=True)
    write_atomic(outbox / name, json.dumps(entry, sort_keys=True) + "\n")
    return True


def cancelled(state, job_id):
    """True once the Director cancelled this job here: _node_control writes <state>/node/cancelled/<job> before it
    acts, and job intake, tick's admission and every trial check it, so a cancelled job never starts again."""
    return bool(JOB_ID.fullmatch(str(job_id or ""))) and (Path(state) / "node/cancelled" / str(job_id)).is_file()
