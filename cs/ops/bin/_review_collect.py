"""cs review weekly's data collection: the Director's lane jobs and ledger reads. Not an op.

Split out of cs/scripts/cs_review.py (the `cs review weekly` CLI), which puts both scripts/ and
ops/bin on sys.path before importing this module. Resolves the Director the same way cs mcp does,
pulls intents/runs/nodes with the owner credential, filters the window's lane jobs, and normalizes
each job into an entry (verdict, decision, minutes, tokens, stop classes) plus the provider-limit
events by template. The task_type x model table, worst lanes and proposals live in _review_score.py;
Markdown and plist rendering in _review_render.py.

Stdlib only.
"""
import calendar
from datetime import datetime, timezone
import json
import os
import time
import urllib.parse
from pathlib import Path

from cs_mcp import Director, Refused
from _job_reason import redact


def _clean_line(text, token="", limit=180):
    """Single-line redacted summary string."""
    s = redact(str(text or ""))
    if token and len(token) >= 8:
        s = s.replace(token, "[REDACTED]")
    s = " ".join(s.split())
    if limit and len(s) > limit:
        s = s[: limit - 1].rstrip() + "…"
    return s


def resolve_director(director_url=None, credential_file=None, root=None):
    """Resolve Director URL and credential file the same way cs mcp does."""
    base_root = Path(root) if root else Path(os.environ.get("HOME") or os.path.expanduser("~")) / ".contextstack"
    try:
        record = json.loads((base_root / "config/install.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        record = {}
    url = director_url or record.get("director") or "http://127.0.0.1:8780"
    cred = Path(credential_file) if credential_file else base_root / "state/director/local-credential"
    return Director(url, cred)


def fetch_run_file(director, run_id, rel_path):
    """Fetch a raw evidence file from GET runs/{id}/files/{path}; None if absent."""
    if not run_id or not rel_path:
        return None
    q_run = urllib.parse.quote(str(run_id), safe="")
    q_path = "/".join(urllib.parse.quote(part, safe="") for part in str(rel_path).split("/") if part)
    try:
        status, data = director.request("GET", f"runs/{q_run}/files/{q_path}")
    except Refused:
        return None
    if status != 200 or data is None:
        return None
    if isinstance(data, bytes):
        return data.decode("utf-8", errors="replace")
    if isinstance(data, str):
        return data
    return json.dumps(data)


def _is_lane_job(job):
    """True for lane jobs (excludes conversation turns and non-lane system jobs)."""
    if not isinstance(job, dict) or not job.get("job_id"):
        return False
    if job.get("conversation_id"):
        return False
    spec = str(job.get("spec") or "")
    if spec == "specs/director/conversation-turn.yaml":
        return False
    meta = job.get("meta")
    if not isinstance(meta, dict):
        return False
    return bool(
        meta.get("source") == "cs-mcp"
        or meta.get("lane")
        or meta.get("task_type")
        or meta.get("agent")
        or meta.get("model")
    )


def _parse_ts(val):
    """Parse unix epoch seconds or ISO-8601 timestamp string into float seconds; None if invalid."""
    if isinstance(val, (int, float)) and not isinstance(val, bool) and val > 0:
        return float(val)
    if isinstance(val, str) and val.strip():
        s = val.strip()
        try:
            return float(s)
        except ValueError:
            pass
        try:
            dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=timezone.utc)
            return dt.timestamp()
        except ValueError:
            try:
                return float(calendar.timegm(time.strptime(s[:19], "%Y-%m-%dT%H:%M:%S")))
            except ValueError:
                return None
    return None


def _job_timestamp(job, run):
    """Best timestamp (unix seconds) for window filtering."""
    candidates = [
        job.get("finished_at"),
        job.get("accepted_at"),
        job.get("created_at"),
        (run or {}).get("received_at"),
    ]
    nums = [ts for ts in (_parse_ts(x) for x in candidates) if ts is not None and ts > 0]
    return max(nums) if nums else 0.0


def _extract_tokens(run, row):
    """Total tokens for a run from row or usage."""
    if isinstance(row, dict):
        raw_tok = row.get("tokens")
        if isinstance(raw_tok, (int, float)) and not isinstance(raw_tok, bool):
            return int(raw_tok)
        if isinstance(raw_tok, dict):
            if isinstance(raw_tok.get("total"), (int, float)):
                return int(raw_tok["total"])
            in_t = raw_tok.get("input_tokens")
            out_t = raw_tok.get("output_tokens")
            if isinstance(in_t, (int, float)) or isinstance(out_t, (int, float)):
                return int(in_t or 0) + int(out_t or 0)
        if isinstance(row.get("input_tokens"), (int, float)) or isinstance(row.get("output_tokens"), (int, float)):
            return int(row.get("input_tokens") or 0) + int(row.get("output_tokens") or 0)
    usage = (run or {}).get("usage")
    if isinstance(usage, dict):
        in_t = usage.get("input_tokens")
        out_t = usage.get("output_tokens")
        if isinstance(in_t, (int, float)) or isinstance(out_t, (int, float)):
            return int(in_t or 0) + int(out_t or 0)
    return None


def _extract_minutes(job, row):
    """Duration in minutes from row.seconds or job timestamps."""
    if isinstance(row, dict):
        sec = row.get("seconds")
        if isinstance(sec, (int, float)) and not isinstance(sec, bool) and sec >= 0:
            return float(sec) / 60.0
    fin = _parse_ts(job.get("finished_at"))
    start = _parse_ts(job.get("accepted_at") or job.get("created_at"))
    if fin is not None and start is not None and fin >= start > 0:
        return float(fin - start) / 60.0
    return None


def _classify_stop(job, run, row):
    """Return (stop_str, is_stuck, is_provider_limit, is_memory_cap)."""
    term = job.get("terminal_row") if isinstance(job.get("terminal_row"), dict) else {}
    stop = str(row.get("stop") or term.get("stop") or "").strip()
    reason = str(
        row.get("reason")
        or row.get("error")
        or term.get("reason")
        or job.get("error")
        or ""
    ).strip()
    combined = f"{stop} {reason}".lower()

    usage = (run or {}).get("usage") if isinstance((run or {}).get("usage"), dict) else {}
    allowance = usage.get("allowance") if isinstance(usage.get("allowance"), dict) else {}
    allowance_rejected = allowance.get("rejected") is True

    files = (run or {}).get("files") if isinstance((run or {}).get("files"), list) else []
    has_stuck_file = any(isinstance(f, dict) and f.get("path") == "stuck.md" for f in files)

    is_stuck = stop.startswith("stuck:") or "stuck:" in combined or has_stuck_file
    is_limit = (
        stop == "provider_limit"
        or "provider_limit" in combined
        or allowance_rejected
    )
    is_memory = stop == "memory_cap" or "memory_cap" in combined

    if not stop:
        if is_stuck:
            for token in combined.split():
                if token.startswith("stuck:"):
                    stop = token.rstrip(":,.;)")
                    break
            else:
                stop = "stuck:unknown"
        elif is_limit:
            stop = "provider_limit"
        elif is_memory:
            stop = "memory_cap"
        elif job.get("state") == "error" or job.get("error"):
            stop = "error"
        elif (run or {}).get("verdict") == "pass" or row.get("verdict") == "pass":
            stop = "completed"
        else:
            stop = str(job.get("state") or "unknown")
    return stop, is_stuck, is_limit, is_memory


def collect_entries(director, since_days=7.0, now=None):
    """Pull the last N days of lane jobs from the Director and normalize them into review entries.

    Returns a dict with the window (window_days, now_ts, cutoff_ts), the owner credential used for
    redaction (token), the per-job entries, and the provider-limit events by template (sorted by
    template). Scoring the entries into the report is _review_score's job.
    """
    now_ts = float(now if now is not None else time.time())
    days = float(since_days)
    cutoff_ts = now_ts - days * 86400.0
    try:
        token = director.token()
    except Refused:
        token = ""

    intents_doc = director.get("intents")
    runs_doc = director.get("runs")
    try:
        nodes_doc = director.get("nodes")
    except Refused:
        nodes_doc = {"nodes": []}

    raw_jobs = intents_doc.get("jobs") if isinstance(intents_doc, dict) else []
    if not isinstance(raw_jobs, list):
        raw_jobs = []
    raw_runs = runs_doc.get("runs") if isinstance(runs_doc, dict) else []
    if not isinstance(raw_runs, list):
        raw_runs = []
    raw_nodes = nodes_doc.get("nodes") if isinstance(nodes_doc, dict) else []
    if not isinstance(raw_nodes, list):
        raw_nodes = []

    runs_by_id = {}
    runs_by_job = {}
    for r in raw_runs:
        if not isinstance(r, dict):
            continue
        rid = r.get("run_id")
        jid = r.get("job_id")
        if rid:
            runs_by_id[str(rid)] = r
        if jid:
            runs_by_job.setdefault(str(jid), []).append(r)

    # Filter lane jobs in window.
    window_jobs = []
    for job in raw_jobs:
        if not _is_lane_job(job):
            continue
        jid = str(job["job_id"])
        run = None
        if job.get("run_id") and str(job["run_id"]) in runs_by_id:
            run = runs_by_id[str(job["run_id"])]
        elif jid in runs_by_job and runs_by_job[jid]:
            run = runs_by_job[jid][0]
        ts = _job_timestamp(job, run)
        if ts > 0 and ts < cutoff_ts:
            continue
        window_jobs.append((job, run, ts))

    # Map retry relationships across jobs.
    retried_by_map = {}
    for job, _, _ in window_jobs:
        meta = job.get("meta") if isinstance(job.get("meta"), dict) else {}
        parent = meta.get("retry_of")
        if parent:
            retried_by_map.setdefault(str(parent), []).append(str(job["job_id"]))

    entries = []
    limit_events_by_template = {}

    def _record_limit_event(template, job_id=None, message="", resets_at=None, from_job=False, from_node=False):
        tmpl = str(template or "unknown")
        slot = limit_events_by_template.setdefault(
            tmpl,
            {
                "template": tmpl,
                "count": 0,
                "job_stops": 0,
                "active_limits": 0,
                "job_ids": [],
                "resets_at": None,
                "message": "",
            },
        )
        if from_job:
            slot["job_stops"] += 1
            slot["count"] += 1
            if job_id and job_id not in slot["job_ids"]:
                slot["job_ids"].append(job_id)
        if from_node:
            slot["active_limits"] += 1
            if slot["job_stops"] == 0:
                slot["count"] += 1
        if resets_at is not None and resets_at != "":
            if isinstance(resets_at, (int, float)) and not isinstance(resets_at, bool) and resets_at > 0:
                slot["resets_at"] = int(resets_at)
            elif isinstance(resets_at, str) and resets_at.strip():
                slot["resets_at"] = resets_at.strip()
        if message:
            cleaned = _clean_line(message, token=token, limit=160)
            if cleaned and not slot["message"]:
                slot["message"] = cleaned

    for job, run, ts in window_jobs:
        jid = str(job["job_id"])
        rid = str(run["run_id"]) if isinstance(run, dict) and run.get("run_id") else (
            str(job["run_id"]) if job.get("run_id") else None
        )
        meta = job.get("meta") if isinstance(job.get("meta"), dict) else {}
        row = (run.get("row") if isinstance(run, dict) and isinstance(run.get("row"), dict) else None) or (
            job.get("terminal_row") if isinstance(job.get("terminal_row"), dict) else {}
        )
        usage = run.get("usage") if isinstance(run, dict) and isinstance(run.get("usage"), dict) else {}
        source = usage.get("source") if isinstance(usage.get("source"), dict) else {}

        task_type = str(meta.get("task_type") or row.get("task_type") or "untyped")
        model = str(meta.get("model") or source.get("model") or row.get("model") or "unknown")
        agent = str(meta.get("agent") or model)
        agent_sha256 = str(meta.get("agent_sha256") or "") or None
        lane = str(meta.get("lane") or jid)
        repo = str(meta.get("repo") or "") or None
        retry_of = str(meta.get("retry_of") or "") or None
        attempt = int(meta.get("attempt") or (2 if retry_of else 1))
        retried_by = retried_by_map.get(jid, [])

        verdict = str((run or {}).get("verdict") or row.get("verdict") or "").strip() or None
        state = str(job.get("state") or "unknown")
        passed = bool(state == "done" and not job.get("error") and verdict == "pass")

        dec_obj = (run or {}).get("decision") if isinstance((run or {}).get("decision"), dict) else None
        decision = str(dec_obj.get("decision")) if dec_obj and dec_obj.get("decision") in ("approve", "reject") else None
        decision_note = str(dec_obj.get("note") or "") if dec_obj else ""
        if decision == "reject" and not decision_note and rid:
            try:
                full_dec = director.get(f"runs/{urllib.parse.quote(rid, safe='')}/decision")
                if isinstance(full_dec, dict) and full_dec.get("note"):
                    decision_note = str(full_dec["note"])
            except Refused:
                pass

        minutes = _extract_minutes(job, row)
        tokens = _extract_tokens(run, row)
        stop, is_stuck, is_limit, is_memory = _classify_stop(job, run, row)

        reason = str(
            row.get("reason")
            or row.get("error")
            or (job.get("terminal_row") or {}).get("reason")
            or ""
        ).strip()
        err = str(job.get("error") or "").strip()

        if is_limit:
            _record_limit_event(
                model,
                job_id=jid,
                message=reason or err or stop,
                from_job=True,
            )

        categories = []
        if decision == "reject":
            categories.append("rejected")
        if is_stuck:
            categories.append("stuck")
        if retry_of or retried_by or attempt > 1:
            categories.append("retried")
        if state == "error" or bool(err) or verdict in ("fail", "void"):
            categories.append("errored")

        entries.append(
            {
                "job_id": jid,
                "run_id": rid,
                "lane": lane,
                "repo": repo,
                "task_type": task_type,
                "agent": agent,
                "agent_sha256": agent_sha256,
                "model": model,
                "state": state,
                "verdict": verdict,
                "passed": passed,
                "decision": decision,
                "decision_note": decision_note,
                "minutes": minutes,
                "tokens": tokens,
                "stop": stop,
                "is_stuck": is_stuck,
                "is_provider_limit": is_limit,
                "is_memory_cap": is_memory,
                "reason": reason,
                "error": err,
                "retry_of": retry_of,
                "attempt": attempt,
                "retried_by": retried_by,
                "categories": categories,
                "timestamp": ts,
                "run": run,
            }
        )

    # Check active node provider_limits as well.
    for node in raw_nodes:
        if not isinstance(node, dict):
            continue
        status = node.get("status") if isinstance(node.get("status"), dict) else {}
        vitals = (
            node.get("vitals")
            if isinstance(node.get("vitals"), dict)
            else (status.get("vitals") if isinstance(status.get("vitals"), dict) else {})
        )
        node_limits = (
            status.get("provider_limits")
            or vitals.get("provider_limits")
            or node.get("provider_limits")
            or []
        )
        if not isinstance(node_limits, list):
            node_limits = []
        for lim in node_limits:
            if not isinstance(lim, dict) or not lim.get("template"):
                continue
            rec_at = _parse_ts(lim.get("recorded_at"))
            if rec_at is not None and 0 < rec_at < cutoff_ts:
                continue
            _record_limit_event(
                lim["template"],
                message=str(lim.get("message") or ""),
                resets_at=lim.get("resets_at"),
                from_node=True,
            )

    provider_limits = [
        limit_events_by_template[k]
        for k in sorted(limit_events_by_template.keys())
    ]

    return {
        "window_days": days,
        "now_ts": now_ts,
        "cutoff_ts": cutoff_ts,
        "token": token,
        "entries": entries,
        "provider_limits": provider_limits,
    }
