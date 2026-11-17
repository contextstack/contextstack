#!/usr/bin/env python3
"""Landing vocabulary for operator_close reasons. Used by close and land.

Reasons: model_miss | factory_gap | unseated.
Evidence: model_miss needs >=1 run id whose ledger verdict is not pass;
factory_gap needs a run id or a refusal receipt; unseated is the only
legal reason for hand work with no seat. Third same-cause factory_gap
per grant per wave refuses with the escalate clause.
"""
from __future__ import annotations

from _estate_io import state_dir

import json
import re
from pathlib import Path

REASONS = ("model_miss", "factory_gap", "unseated")
PASS_VERDICTS = {"pass"}
ESCALATE = "same refusal twice means the fix was wrong — stop and escalate."
RULE_MODEL_MISS = "model_miss requires ≥1 run id whose verdict is not pass"
RULE_FACTORY_GAP = "factory_gap requires a run id or a refusal receipt"
RULE_UNSEATED = "unseated is the only legal reason for hand work with no seat"


def _rows(path):
    path = Path(path)
    if not path.is_file():
        return []
    out = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(row, dict):
            out.append(row)
    return out


def _run_ids(raw):
    if raw is None or raw == "":
        return []
    if isinstance(raw, list):
        return [str(x).strip() for x in raw if str(x).strip()]
    text = str(raw).strip()
    if not text:
        return []
    return [p.strip() for p in re.split(r"[\s,]+", text) if p.strip()]


def _verdict_for_run(lab, run_id):
    """Return a ledger verdict for run_id, or None if unknown."""
    lab = Path(lab)
    for row in _rows(state_dir(lab) / "ledger.jsonl"):
        run = str(row.get("run") or "")
        if not run:
            continue
        parts = [p.strip() for p in run.split(",") if p.strip()]
        if run_id == run or run_id in parts or run.endswith(run_id) or run_id in run:
            verdict = str(row.get("verdict") or "").strip()
            if verdict:
                return verdict
    runs = state_dir(lab) / "runs"
    if runs.is_dir():
        for child in runs.iterdir():
            if not child.is_dir():
                continue
            if run_id == child.name or run_id in child.name:
                meta = child / "result.json"
                if meta.is_file():
                    try:
                        data = json.loads(meta.read_text(encoding="utf-8"))
                    except json.JSONDecodeError:
                        data = {}
                    verdict = str(data.get("verdict") or "").strip()
                    if verdict:
                        return verdict
    return None


def _refusal_exists(lab, refusal_id):
    if not refusal_id:
        return False
    rid = str(refusal_id).strip()
    for row in _rows(state_dir(lab) / "refusals.jsonl"):
        if str(row.get("id") or "") == rid:
            return True
    return False


def _dispatch_exists(lab, run_id):
    for row in _rows(state_dir(lab) / "dispatch.jsonl"):
        if str(row.get("id") or "") == run_id:
            return True
    return False


def _factory_gap_count(lab, grant, cause, wave):
    """Count prior factory_gap landings with the same grant/cause/wave."""
    grant = str(grant or "").strip()
    cause = str(cause or "").strip()
    wave = str(wave or "").strip()
    if not (grant and cause and wave):
        return 0
    n = 0
    for row in _rows(state_dir(lab) / "ledger.jsonl"):
        if str(row.get("verdict") or "") != "operator_close":
            continue
        reason = str(row.get("close_reason") or "").strip()
        if reason != "factory_gap":
            if "factory_gap" not in str(row.get("stop") or ""):
                continue
        if str(row.get("grant") or "").strip() != grant:
            continue
        if str(row.get("cause") or "").strip() != cause:
            continue
        if str(row.get("wave") or "").strip() != wave:
            continue
        n += 1
    return n


def validate_operator_close(lab, payload):
    """'' when legal; else refusal text quoting the rule and naming missing evidence."""
    reason = str(payload.get("reason") or "").strip()
    run_ids = _run_ids(payload.get("run_ids") or payload.get("run_id") or payload.get("run"))
    refusal_id = str(payload.get("refusal_id") or "").strip()
    grant = str(payload.get("grant") or "").strip()
    cause = str(payload.get("cause") or "").strip()
    wave = str(payload.get("wave") or "").strip()
    has_seat = bool(run_ids)

    if not reason:
        return "close: operator_close requires reason (%s)" % ", ".join(REASONS)
    if reason not in REASONS:
        return "close: reason must be one of %s" % ", ".join(REASONS)

    if reason == "unseated":
        if has_seat:
            return "close: %s — refused (run ids present: %s)" % (
                RULE_UNSEATED, ",".join(run_ids))
        return ""

    if not has_seat and not refusal_id:
        return "close: %s — refused (reason %s has no seat and no refusal receipt)" % (
            RULE_UNSEATED, reason)

    if reason == "model_miss":
        if not run_ids:
            return "close: %s — refused (missing run id)" % RULE_MODEL_MISS
        non_pass = []
        for rid in run_ids:
            verdict = _verdict_for_run(lab, rid)
            if verdict is None:
                continue
            if verdict not in PASS_VERDICTS:
                non_pass.append((rid, verdict))
        if not non_pass:
            return "close: %s — refused (no non-pass verdict among run ids %s)" % (
                RULE_MODEL_MISS, ",".join(run_ids))
        return ""

    if reason == "factory_gap":
        if not run_ids and not refusal_id:
            return "close: %s — refused (missing run id and refusal receipt)" % RULE_FACTORY_GAP
        if refusal_id and not _refusal_exists(lab, refusal_id):
            return "close: %s — refused (refusal receipt %s not found)" % (
                RULE_FACTORY_GAP, refusal_id)
        if run_ids and not refusal_id:
            missing = [rid for rid in run_ids
                       if _verdict_for_run(lab, rid) is None
                       and not _dispatch_exists(lab, rid)
                       and not any((state_dir(lab) / "runs").glob("*" + rid + "*"))]
            if missing:
                return "close: %s — refused (run id not found: %s)" % (
                    RULE_FACTORY_GAP, ",".join(missing))
        if grant and cause and wave:
            prior = _factory_gap_count(lab, grant, cause, wave)
            if prior >= 2:
                return "close: %s" % ESCALATE
        return ""

    return "close: unknown reason %s" % reason


def close_ledger_fields(payload):
    """Extra ledger columns for a validated operator_close."""
    fields = {"close_reason": str(payload.get("reason") or "").strip()}
    for key in ("grant", "cause", "wave", "refusal_id"):
        if payload.get(key):
            fields[key] = str(payload[key]).strip()
    run_ids = _run_ids(payload.get("run_ids") or payload.get("run_id") or payload.get("run"))
    if run_ids:
        fields["run"] = ",".join(run_ids)
    return fields
