"""cs review weekly's scoring: the task_type x agent/model table, worst lanes, proposals. Not an op.

Split out of cs/scripts/cs_review.py (the `cs review weekly` CLI), which puts both scripts/ and
ops/bin on sys.path before importing this module. Takes the normalized entries _review_collect.py
gathered from the Director and computes the review report's scored parts: the per task_type and
agent/model table rows (jobs, pass and approve rates, median minutes and tokens, stop counts), the
worst lanes (rejected, stuck, retried, errored) each with a two-line autopsy, and up to three
concrete, checkable proposed changes derived deterministically by rules over the table (never by a
model call).

Stdlib only.
"""
import statistics

import cs_agents
from _review_collect import _clean_line, fetch_run_file


def build_table(entries):
    """Aggregate review entries into the task_type x agent/model table rows (sorted by key)."""
    # Compute task_type x agent/model table.
    grouped = {}
    for e in entries:
        key = (e["task_type"], e["agent"], e["model"])
        cell = grouped.setdefault(
            key,
            {
                "task_type": e["task_type"],
                "agent": e["agent"],
                "model": e["model"],
                "agent_model": f"{e['agent']}/{e['model']}",
                "agent_sha256s": [],
                "jobs": 0,
                "passed": 0,
                "decided": 0,
                "approved": 0,
                "rejected": 0,
                "minutes_list": [],
                "tokens_list": [],
                "stuck_stops": 0,
                "limit_stops": 0,
                "memory_stops": 0,
            },
        )
        cell["jobs"] += 1
        if e["passed"]:
            cell["passed"] += 1
        if e["decision"] in ("approve", "reject"):
            cell["decided"] += 1
            if e["decision"] == "approve":
                cell["approved"] += 1
            else:
                cell["rejected"] += 1
        if e["minutes"] is not None:
            cell["minutes_list"].append(e["minutes"])
        if e["tokens"] is not None:
            cell["tokens_list"].append(e["tokens"])
        if e["is_stuck"]:
            cell["stuck_stops"] += 1
        if e["is_provider_limit"]:
            cell["limit_stops"] += 1
        if e["is_memory_cap"]:
            cell["memory_stops"] += 1
        if e["agent_sha256"] and e["agent_sha256"] not in cell["agent_sha256s"]:
            cell["agent_sha256s"].append(e["agent_sha256"])

    table_rows = []
    for key in sorted(grouped.keys()):
        c = grouped[key]
        jobs_n = c["jobs"]
        pass_rate = round(c["passed"] / jobs_n, 4) if jobs_n > 0 else 0.0
        approve_rate = round(c["approved"] / c["decided"], 4) if c["decided"] > 0 else None
        med_min = round(float(statistics.median(c["minutes_list"])), 2) if c["minutes_list"] else None
        med_tok = int(round(float(statistics.median(c["tokens_list"])))) if c["tokens_list"] else None
        table_rows.append(
            {
                "task_type": c["task_type"],
                "agent": c["agent"],
                "model": c["model"],
                "agent_model": c["agent_model"],
                "agent_sha256s": c["agent_sha256s"],
                "jobs": jobs_n,
                "passed": c["passed"],
                "pass_rate": pass_rate,
                "decided": c["decided"],
                "approved": c["approved"],
                "rejected": c["rejected"],
                "approve_rate": approve_rate,
                "median_minutes": med_min,
                "median_tokens": med_tok,
                "stuck_stops": c["stuck_stops"],
                "limit_stops": c["limit_stops"],
                "provider_limit_stops": c["limit_stops"],
                "memory_stops": c["memory_stops"],
                "memory_cap_stops": c["memory_stops"],
                "stops": {
                    "stuck": c["stuck_stops"],
                    "provider_limit": c["limit_stops"],
                    "memory_cap": c["memory_stops"],
                },
            }
        )
    return table_rows


def _summarize_stuck_md(stuck_md, token=""):
    """Condense stuck.md content into one informative autopsy line."""
    if not stuck_md or not stuck_md.strip():
        return None
    raw_lines = [ln.strip() for ln in stuck_md.splitlines() if ln.strip()]
    if not raw_lines:
        return None
    heading = ""
    details = []
    for ln in raw_lines:
        if ln.startswith("#") and not heading:
            heading = ln.lstrip("#").strip()
            continue
        if ln.startswith("##"):
            continue
        details.append(ln.lstrip("-* ").strip())
    if heading and details:
        text = f"stuck.md: {heading} — {'; '.join(details[:2])}"
    elif heading:
        text = f"stuck.md: {heading}"
    else:
        text = f"stuck.md: {'; '.join(details[:2])}"
    return _clean_line(text, token=token, limit=180)


def _build_autopsy(entry, stuck_md=None, decision_note=None, token=""):
    """Build a deterministic two-line autopsy from the run's reason/decision and stuck.md when present."""
    stop = entry.get("stop") or "unknown"
    verdict = entry.get("verdict") or entry.get("state") or "unknown"
    decision = entry.get("decision")
    reason = entry.get("reason") or ""
    err = entry.get("error") or ""

    primary_why = reason or err or decision_note or f"state={entry.get('state')} verdict={verdict}"
    prefix_parts = [f"stop={stop}", f"verdict={verdict}"]
    if decision:
        prefix_parts.append(f"decision={decision}")
    line1 = _clean_line(f"reason ({', '.join(prefix_parts)}): {primary_why}", token=token, limit=180)

    stuck_line = _summarize_stuck_md(stuck_md, token=token)
    if stuck_line:
        line2 = stuck_line
    elif decision == "reject" and decision_note:
        line2 = _clean_line(
            f"decision note: {decision_note} (agent={entry['agent']}/{entry['model']})",
            token=token,
            limit=180,
        )
    elif entry.get("retry_of") or entry.get("retried_by"):
        retry_parts = []
        if entry.get("retry_of"):
            retry_parts.append(f"retry_of={entry['retry_of']} (attempt {entry.get('attempt', 2)})")
        if entry.get("retried_by"):
            retry_parts.append(f"retried_by={','.join(entry['retried_by'])}")
        line2 = _clean_line(
            f"retry lineage: {'; '.join(retry_parts)} on {entry['agent']}/{entry['model']}",
            token=token,
            limit=180,
        )
    else:
        secondary = err if (err and err != reason) else (decision_note or f"task_type={entry['task_type']}")
        line2 = _clean_line(
            f"context: agent={entry['agent']}/{entry['model']} {secondary}",
            token=token,
            limit=180,
        )
    return [line1, line2]


def build_worst_lanes(director, entries, token=""):
    """Build the worst lanes (rejected, stuck, retried, errored), each with its two-line autopsy."""
    # Build worst lanes with two-line autopsies.
    severity_rank = {"rejected": 0, "stuck": 1, "retried": 2, "errored": 3}
    worst_candidates = [e for e in entries if e["categories"]]
    worst_candidates.sort(
        key=lambda e: (
            min(severity_rank.get(cat, 9) for cat in e["categories"]),
            -float(e.get("timestamp") or 0.0),
            e["job_id"],
        )
    )

    worst_lanes = []
    for e in worst_candidates:
        stuck_md = None
        run_obj = e.get("run")
        files = run_obj.get("files") if isinstance(run_obj, dict) and isinstance(run_obj.get("files"), list) else []
        has_stuck_md = any(isinstance(f, dict) and f.get("path") == "stuck.md" for f in files)
        if (has_stuck_md or e["is_stuck"]) and e.get("run_id"):
            stuck_md = fetch_run_file(director, e["run_id"], "stuck.md")
        autopsy = _build_autopsy(
            e,
            stuck_md=stuck_md,
            decision_note=e.get("decision_note"),
            token=token,
        )
        worst_lanes.append(
            {
                "job_id": e["job_id"],
                "run_id": e["run_id"],
                "lane": e["lane"],
                "repo": e["repo"],
                "task_type": e["task_type"],
                "agent": e["agent"],
                "model": e["model"],
                "agent_model": f"{e['agent']}/{e['model']}",
                "state": e["state"],
                "verdict": e["verdict"],
                "stop": e["stop"],
                "decision": e["decision"],
                "retry_of": e["retry_of"],
                "attempt": e["attempt"],
                "retried_by": e["retried_by"],
                "status": e["categories"][0],
                "categories": e["categories"],
                "has_stuck_md": bool(stuck_md),
                "autopsy": autopsy,
                "autopsy_text": "\n".join(autopsy),
            }
        )
    return worst_lanes


def _pick_alternative_model(agent, current_model, task_type, table_rows, limited_templates, agents_dir=None):
    """Choose a concrete target model from table performance or profile fallbacks."""
    best_model = None
    best_score = (-1.0, -1.0, 0)
    for r in table_rows:
        cand = r["model"]
        if not cand or cand == current_model or cand in limited_templates:
            continue
        if r["agent"] != agent and r["task_type"] != task_type:
            continue
        app = r["approve_rate"] if r["approve_rate"] is not None else r["pass_rate"]
        score = (app, r["pass_rate"], r["jobs"])
        if score > best_score and r["pass_rate"] >= 0.5 and r["stuck_stops"] == 0:
            best_score = score
            best_model = cand
    if best_model:
        return best_model

    profile = agent if agent and agent != current_model else ""
    if profile:
        try:
            prof_doc = cs_agents.load(profile)
            for cand in prof_doc.get("fallback") or []:
                if isinstance(cand, str) and cand != current_model and cand not in limited_templates:
                    return cand
        except Exception:
            pass

    default_map = {
        "cursor-codex": "codex",
        "cursor-sol": "codex",
        "cursor-agent": "codex",
        "cursor-composer": "codex",
        "codex": "claude-sonnet",
        "claude-sonnet": "codex",
    }
    return default_map.get(current_model, "codex")


def generate_proposals(table_rows, provider_limits, entries, agents_dir=None):
    """Generate up to three concrete, checkable proposals by deterministic rules over the table."""
    proposals = []
    details = []
    seen_keys = set()
    limited_templates = {p["template"] for p in provider_limits if p.get("count", 0) > 0}

    # Aggregate by (agent, model) for profile-wide reject rules, and check per-row too.
    by_agent_model = {}
    for r in table_rows:
        key = (r["agent"], r["model"])
        agg = by_agent_model.setdefault(
            key,
            {
                "agent": r["agent"],
                "model": r["model"],
                "task_types": [],
                "jobs": 0,
                "passed": 0,
                "decided": 0,
                "approved": 0,
                "rejected": 0,
                "stuck_stops": 0,
                "limit_stops": 0,
                "memory_stops": 0,
            },
        )
        agg["task_types"].append(r["task_type"])
        agg["jobs"] += r["jobs"]
        agg["passed"] += r["passed"]
        agg["decided"] += r["decided"]
        agg["approved"] += r["approved"]
        agg["rejected"] += r["rejected"]
        agg["stuck_stops"] += r["stuck_stops"]
        agg["limit_stops"] += r["limit_stops"]
        agg["memory_stops"] += r["memory_stops"]

    # Rule 1: High reject count / low approve rate on a profile/model.
    reject_candidates = sorted(
        by_agent_model.values(),
        key=lambda a: (a["rejected"], -a["approved"], a["jobs"]),
        reverse=True,
    )
    for agg in reject_candidates:
        if len(proposals) >= 3:
            break
        rej = agg["rejected"]
        dec = agg["decided"]
        if rej < 2 and not (rej >= 1 and dec > 0 and (agg["approved"] / dec) < 0.5):
            continue
        agent = agg["agent"]
        model = agg["model"]
        task_type = agg["task_types"][0] if agg["task_types"] else "untyped"
        target = _pick_alternative_model(agent, model, task_type, table_rows, limited_templates, agents_dir=agents_dir)
        denom = dec if dec >= rej else agg["jobs"]
        text = f"{agent} profile: move from {model} to {target} — {rej}/{denom} rejects came from it"
        key = ("reject", agent, model)
        if key not in seen_keys:
            seen_keys.add(key)
            proposals.append(text)
            details.append(
                {
                    "rule": "high_reject_rate",
                    "agent": agent,
                    "task_type": task_type,
                    "from_model": model,
                    "to_model": target,
                    "rejected": rej,
                    "decided": denom,
                    "proposal": text,
                }
            )

    # Rule 2: Stuck or low-pass-rate concentration on a (task_type, agent, model) row.
    # Also check if a retry on another model succeeded.
    retry_success_model = {}
    entries_by_job = {e["job_id"]: e for e in entries}
    for e in entries:
        parent_id = e.get("retry_of")
        if parent_id and parent_id in entries_by_job and e.get("passed"):
            parent = entries_by_job[parent_id]
            retry_success_model[(parent["task_type"], parent["agent"], parent["model"])] = e["model"]

    stuck_candidates = sorted(
        table_rows,
        key=lambda r: (r["stuck_stops"], r["jobs"] - r["passed"], -r["pass_rate"], r["jobs"]),
        reverse=True,
    )
    for r in stuck_candidates:
        if len(proposals) >= 3:
            break
        agent = r["agent"]
        model = r["model"]
        task_type = r["task_type"]
        if ("reject", agent, model) in seen_keys:
            continue
        stuck = r["stuck_stops"]
        failed = r["jobs"] - r["passed"]
        if stuck < 1 and not (failed >= 2 and r["pass_rate"] <= 0.5):
            continue
        target = retry_success_model.get((task_type, agent, model)) or _pick_alternative_model(
            agent, model, task_type, table_rows, limited_templates, agents_dir=agents_dir
        )
        if stuck > 0:
            text = (
                f"{agent} profile ({task_type}): move from {model} to {target} — "
                f"{stuck}/{r['jobs']} jobs stopped on stuck:* (pass rate {int(round(r['pass_rate'] * 100))}%)"
            )
            rule_name = "stuck_concentration"
        else:
            text = (
                f"{agent} profile ({task_type}): move from {model} to {target} — "
                f"{failed}/{r['jobs']} jobs failed (pass rate {int(round(r['pass_rate'] * 100))}%)"
            )
            rule_name = "low_pass_rate"
        key = ("stuck_or_fail", task_type, agent, model)
        if key not in seen_keys:
            seen_keys.add(key)
            proposals.append(text)
            details.append(
                {
                    "rule": rule_name,
                    "agent": agent,
                    "task_type": task_type,
                    "from_model": model,
                    "to_model": target,
                    "stuck_stops": stuck,
                    "jobs": r["jobs"],
                    "proposal": text,
                }
            )

    # Rule 3: Provider-limit events by template.
    for lim in sorted(provider_limits, key=lambda x: (x["count"], x["job_stops"]), reverse=True):
        if len(proposals) >= 3:
            break
        if lim["count"] < 1:
            continue
        tmpl = lim["template"]
        # Find an agent using this template if any.
        matching_agents = [r["agent"] for r in table_rows if r["model"] == tmpl]
        agent = matching_agents[0] if matching_agents else ""
        target = _pick_alternative_model(
            agent, tmpl, "untyped", table_rows, limited_templates - {tmpl}, agents_dir=agents_dir
        )
        label = f"{agent} profile ({tmpl})" if agent and agent != tmpl else f"{tmpl} template"
        text = (
            f"{label}: route default from {tmpl} to {target} — "
            f"{lim['count']} provider_limit event(s) recorded"
        )
        key = ("provider_limit", tmpl)
        if key not in seen_keys:
            seen_keys.add(key)
            proposals.append(text)
            details.append(
                {
                    "rule": "provider_limit",
                    "agent": agent or tmpl,
                    "task_type": None,
                    "from_model": tmpl,
                    "to_model": target,
                    "events": lim["count"],
                    "proposal": text,
                }
            )

    # Rule 4: Memory cap stops (if we still have fewer than 3 proposals).
    for r in sorted(table_rows, key=lambda x: (x["memory_stops"], x["jobs"]), reverse=True):
        if len(proposals) >= 3:
            break
        if r["memory_stops"] < 1:
            continue
        agent = r["agent"]
        model = r["model"]
        task_type = r["task_type"]
        text = (
            f"{agent} profile ({task_type}): raise memory_mb budget or split scope on {model} — "
            f"{r['memory_stops']}/{r['jobs']} jobs stopped on memory_cap"
        )
        key = ("memory_cap", task_type, agent, model)
        if key not in seen_keys:
            seen_keys.add(key)
            proposals.append(text)
            details.append(
                {
                    "rule": "memory_cap",
                    "agent": agent,
                    "task_type": task_type,
                    "from_model": model,
                    "to_model": model,
                    "memory_stops": r["memory_stops"],
                    "jobs": r["jobs"],
                    "proposal": text,
                }
            )

    return proposals[:3], details[:3]
