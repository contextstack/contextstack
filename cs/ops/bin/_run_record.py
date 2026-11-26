"""A coding-agent run's terminal record, in the Rust runner's shapes, so finalize-run closes it as it closes a runner
run. Not an op.

The executor appends its own ledger row first (row transcript "ledger/runs/<id>"), then writes check.json and
terminal.json and calls finalize-run, whose row step finds that row by transcript and appends nothing; it writes
the run_finalized envelope (messages.jsonl, events.jsonl, transcript, evidence ...) and finalized.json. The
transcript footer goes on before publish-evidence hashes the transcript, so finalize-run never appends to it.
"""
import hashlib
import json
import os
import re
import socket
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path

from _agent_stream import append, line

from _estate_io import estate_id, spec_intent, write_atomic
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


def check_json(run_dir, stimulus_sha, window, exit_code=None, output="", verdict=None, *, grader=None):
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
    value.update(grader or {})
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


def terminal(run_dir, ws, row, run_id, spec_id, task, started, check_window, trace=None, *, cli_row=False, observed_trace=None, cli_lane=None):
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
        "cost_usd": row.get("cost_usd", 0), "est_usd": row.get("est_usd", 0), "est_basis": row.get("est_basis", "unreported"),
        "cost_class": row.get("cost_class"), "input_tokens": row.get("input_tokens"),
        "output_tokens": row.get("output_tokens"),
        **({"peak_rss_bytes": row["peak_rss_bytes"]} if row.get("peak_rss_bytes") is not None else {}),
        "terminal_state": state,
        "cancel_reason": "SIGTERM" if state == "cancelled" else None,  # the gateway's cancel is a TERM
        "started": stamp(started), "ended": stamp(ended), "seconds": round(ended - started, 6),
        "check_window": check_window,
    }
    value.update(spec_intent(row))
    if trace is not None:
        value.update(trace.terminal_fields())
    if observed_trace:
        value.update(observed_trace)
    if cli_row:
        value["cli_row"] = row
        if cli_lane:
            value["cli_lane"] = {key: cli_lane[key] for key in ("main", "lane")}
    if row.get("reason"):
        value["reason"] = str(row["reason"])
    if isinstance(row.get("stuck"), dict):
        value["stuck"] = row["stuck"]
    if "refused_paths" in row:
        value["refused_paths"] = row["refused_paths"]
    tmp = Path(run_dir) / "terminal.json.tmp"
    tmp.write_text(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False), encoding="utf-8")
    os.replace(tmp, Path(run_dir) / "terminal.json")


def finalize(code_root, run_dir, env):
    """finalize-run in the executor's own environment less credentials (its roots, not the agent's allowlist): its
    reply, or a pending one. tick's sweep retries any run left without finalized.json."""
    try:
        done = subprocess.run([str(Path(code_root) / "ops/bin/finalize-run")], input=json.dumps({"run_dir": str(run_dir)}),
                              capture_output=True, text=True, cwd=str(code_root), env=env, timeout=600)
        reply = json.loads((done.stdout or "").strip().splitlines()[-1])
    except (OSError, ValueError, IndexError, subprocess.SubprocessError) as error:
        return {"ok": False, "state": "pending", "reason": str(error)}
    return reply if isinstance(reply, dict) else {"ok": False, "state": "pending", "reason": "no reply"}


def utc_now():
    return datetime.now(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")


def trace_clock():
    return utc_now(), time.monotonic_ns()


def elapsed_ms(start, end):
    return max(0, end[1] - start[1]) / 1_000_000


def producer_time(value):
    """Preserve contract-shaped producer times byte for byte."""
    for field in ("at", "timestamp", "ts"):
        stamp = value.get(field)
        if isinstance(stamp, str) and re.fullmatch(
            r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{1,6}Z", stamp
        ):
            try:
                datetime.fromisoformat(stamp[:-1] + "+00:00")
            except ValueError:
                continue
            return stamp
        # Native OpenCode events use Unix milliseconds.
        if field == "timestamp" and type(stamp) is int and 10**12 <= stamp < 10**14:
            return datetime.fromtimestamp(stamp / 1000, timezone.utc).isoformat(
                timespec="milliseconds").replace("+00:00", "Z")
    return None


def span(run_id, machine, kind, span_id, parent_id, start, end, outcome, attributes, duration=None):
    return {
        "trace_id": run_id, "span_id": span_id, "parent_id": parent_id, "kind": kind, "name": kind,
        "start": start[0], "end": end[0], "duration_ms": elapsed_ms(start, end) if duration is None else duration,
        "outcome": outcome, "attributes": {"machine": machine, **attributes},
    }


class ObservedTrace:
    """The adapter's task CLI stream, with a durable pointer for each received event.

    An auth refresh is maintenance. If it triggers a task retry, the process
    record covers both task invocations, counts both streams and uses the
    last task exit status. Duration sums the time spent in task processes.
    """

    def __init__(self, run_dir, code_root, task, trial, started, execution_id=None):
        self.path = run_dir / "trace.jsonl"
        self.cli_events = run_dir / "cli-events.jsonl"
        self.run_id = run_dir.name
        try:
            self.machine = estate_id(code_root)
        except (OSError, ValueError):
            self.machine = socket.gethostname() or "local"
        self.started = started
        self.identity = {"run_id": self.run_id, "coverage": "observed"}
        self.execution_id = execution_id
        if execution_id:
            self.identity["execution_id"] = execution_id
        if job_id := os.environ.get("CS_JOB_ID", "").strip():
            self.identity.update(job_id=job_id, task=task, trial=trial)
        self.process_start = self.process_end = self.finalization_start = None
        self.received = 0
        self.duration = 0
        self.status = None
        self.process_outcome = "exited"
        self.usage_row = {}
        self.usage = {}

    clock = staticmethod(trace_clock)

    def launch(self):
        if self.process_start is None:
            if self.execution_id:  # The harness may already have recorded a lost transport.
                self.path.touch()
            else:
                write_atomic(self.path, b"")
            write_atomic(self.cli_events, b"")
        start = trace_clock()
        if self.process_start is None:
            self.process_start = start
        return start

    def exited(self, start, end, status, outcome="exited"):
        self.process_end = end
        self.status = status
        self.process_outcome = outcome
        self.duration += elapsed_ms(start, end)

    def event(self, raw):
        received = trace_clock()
        text = raw.decode("utf-8", errors="replace").rstrip("\r\n")
        if not text.strip():
            return received[0], "adapter_receive"
        try:
            value = json.loads(text)
        except ValueError:
            value = None
        at = producer_time(value) if isinstance(value, dict) else None
        source = "producer" if at else "adapter_receive"
        record = {"event": "cli_event", "output": redact(text)}
        append(self.cli_events, record, durable=True)
        self.received += 1
        instant = (at or received[0], received[1])
        pointer = {"file": self.cli_events.name, "line": self.received,
                   "sha256": hashlib.sha256(line(record).encode("utf-8")[:-1]).hexdigest()}
        self.write("cli_event", "cli_event:%d" % self.received, "cli_process", instant, instant, "received",
                   {"timestamp_source": source, "content": {"output": pointer}})
        return instant[0], source

    def report_usage(self, source, row, reported=False):
        tokens = {key: row[key] for key in ("input_tokens", "output_tokens", "cache_read_tokens",
                  "cache_creation_tokens", "reasoning_tokens") if row.get(key) is not None}
        if row.get("est_basis") == "api_list":
            tokens["estimated_usd"] = row["est_usd"]
        if tokens or reported:
            self.usage = {"source": source, **tokens}
            self.usage_row = row

    def write(self, kind, span_id, parent, start, end, outcome, attributes, duration=None):
        append(self.path, span(self.run_id, self.machine, kind, span_id, parent, start, end, outcome,
                               attributes, duration), durable=True)

    def terminal_fields(self):
        if self.status is None:
            return {}
        if self.usage:
            self.write("cli_usage", "cli_usage", "cli_process", self.process_end, self.process_end,
                       "reported", self.usage)
        self.write("cli_process", "cli_process", "run", self.process_start, self.process_end, self.process_outcome, {
            "exit_status": self.status, "received_events": self.received, "usage_reported": bool(self.usage),
        }, self.duration)
        return {"observed_trace": {
            "identity": self.identity, "machine": self.machine, "started": self.started,
            "finalization_started": self.finalization_start or trace_clock(),
        }}


def run_observed(command, workspace, env, timeout, *, trace, **kwargs):
    from _agent_stream import run_streaming
    return run_streaming(command, workspace, env, timeout, lambda *_: None, trace=trace, **kwargs)
