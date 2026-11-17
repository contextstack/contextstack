"""A coding-agent run's terminal record, in the Rust runner's shapes, so finalize-run closes it as it closes a runner
run. Not an op.

The executor appends its own ledger row first (row transcript "ledger/runs/<id>"), then writes check.json and
terminal.json and calls finalize-run, whose row step finds that row by transcript and appends nothing; it writes
the run_finalized envelope (messages.jsonl, events.jsonl, transcript, evidence ...) and finalized.json. The
transcript footer goes on before publish-evidence hashes the transcript, so finalize-run never appends to it.
"""
import json
import os
import subprocess
import time
from pathlib import Path

from _estate_io import spec_intent, write_atomic
from _job_reason import redact

CLIP = 8 << 10
FOOTER = "\n---\nstop %s · verdict **%s** · turns %d · tools %d · cost %s\n"  # finalize-run FOOTER; amount or "unmetered"


def _count(value):
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else 0


def clip(text):
    raw = (text or "").encode("utf-8", "replace")
    if len(raw) > CLIP:
        raw = raw[:CLIP] + b"\n[truncated]"
    return raw.decode("utf-8", "replace")


def check_json(run_dir, stimulus_sha, window, exit_code=None, output="", verdict=None):
    """check.json as finalize-run's run_check writes it; exit_code None is a check that did not run (skipped,
    with the run's verdict), so finalize-run never runs it afterwards. Written once."""
    path = Path(run_dir) / "check.json"
    if path.is_file():
        return
    if exit_code is None:
        value = {"verdict": verdict or "fail", "exit_code": 0, "output": "", "skipped": True}
    else:
        value = {"verdict": "pass" if exit_code == 0 else "fail", "exit_code": exit_code,
                 "output": clip(redact(output)), "skipped": False}
    value.update(window=int(window), stimulus_sha=stimulus_sha or "")
    write_atomic(path, (json.dumps(value, sort_keys=True) + "\n").encode("utf-8"))


def footer(run_dir, stop, verdict, row):
    """The transcript's footer line, before publish-evidence records the transcript's hash."""
    path = Path(run_dir) / "transcript.md"
    body = path.read_bytes() if path.is_file() else b""
    if "cost_usd" in row and row["cost_usd"] is None:
        shown = "unmetered"  # a harness with no dollar report is not a measured $0
    else:
        shown = "$%.4f" % float(row.get("cost_usd") or 0)
    line = FOOTER % (stop, verdict, _count(row.get("turns")), _count(row.get("tool_calls")), shown)
    write_atomic(path, body + line.encode("utf-8"))


def terminal_state(stop):
    """runner.rs: cancelled, timeout (the run's own clock), error, else completed."""
    if stop == "cancelled":
        return "cancelled"
    if stop in ("timeout", "preflight_timeout"):
        return "timeout"
    return "error" if stop.startswith("error") else "completed"


def terminal(run_dir, ws, row, run_id, spec_id, task, started, check_window):
    """terminal.json: runner.rs `run`'s fields (serde_json: sorted, compact, temp then rename), plus adapter."""
    state = terminal_state(str(row.get("stop") or ""))
    ended = time.time()
    stamp = lambda t: time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(t)) + (".%09dZ" % int((t % 1) * 1e9))
    value = {
        "run_id": run_id, "runner_implementation": "executor", "runner_build": None, "adapter": "executor",
        "run_dir": str(run_dir), "transcript": row["transcript"], "workspace": str(ws),
        "stimulus_dir": str(Path(run_dir) / "stimulus"), "spec": spec_id,
        "spec_sha": row.get("spec_sha", ""), "spec_path": row.get("spec", ""), "task": task,
        "trial": row.get("trial"), "task_sha": row.get("task_sha", ""), "env_sha": row.get("env_sha", ""),
        "stimulus_sha": row.get("stimulus_sha", ""), "model": row.get("model"), "stop": row.get("stop"),
        "verdict": row.get("verdict"), "turns": _count(row.get("turns")), "tool_calls": _count(row.get("tool_calls")),
        "cost_usd": row.get("cost_usd", 0), "est_usd": row.get("est_usd", 0), "est_basis": row.get("est_basis", ""),
        "cost_class": row.get("cost_class"), "input_tokens": row.get("input_tokens"),
        "output_tokens": row.get("output_tokens"),
        **({"peak_rss_bytes": row["peak_rss_bytes"]} if row.get("peak_rss_bytes") is not None else {}),
        "terminal_state": state,
        "cancel_reason": "SIGTERM" if state == "cancelled" else None,  # the gateway's cancel is a TERM
        "started": stamp(started), "ended": stamp(ended), "seconds": round(ended - started, 6),
        "check_window": max(int(check_window), 60),
    }
    value.update(spec_intent(row))
    if row.get("reason"):
        value["reason"] = str(row["reason"])
    if isinstance(row.get("stuck"), dict):
        value["stuck"] = row["stuck"]
    tmp = Path(run_dir) / "terminal.json.tmp"
    tmp.write_text(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False), encoding="utf-8")
    os.replace(tmp, Path(run_dir) / "terminal.json")


def finalize(lab, run_dir, env):
    """finalize-run in the executor's own environment less credentials (its roots, not the agent's allowlist): its
    reply, or a pending one. tick's sweep retries any run left without finalized.json."""
    try:
        done = subprocess.run([str(Path(lab) / "ops/bin/finalize-run")], input=json.dumps({"run_dir": str(run_dir)}),
                              capture_output=True, text=True, cwd=str(lab), env=env, timeout=600)
        reply = json.loads((done.stdout or "").strip().splitlines()[-1])
    except (OSError, ValueError, IndexError, subprocess.SubprocessError) as error:
        return {"ok": False, "state": "pending", "reason": str(error)}
    return reply if isinstance(reply, dict) else {"ok": False, "state": "pending", "reason": "no reply"}
