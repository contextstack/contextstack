"""A coding agent's output while it runs. Not an op.

The executor runs the agent in its streaming mode and reads stdout line by line: each line goes on to the
executor's own stdout under the gateway (whose streamed lab-execute maps it to deltas), and its text to
`<run_dir>/events.jsonl`. The reply goes to `messages.jsonl` as a `turn` event. Both files have the Rust
runner's exact line shape (harness/src/runner.rs `Events`, `Cell::emit`), so downstream reads both paths alike.

Text mapping mirrors gateway/src/agentstream.rs:
- claude stream-json: `stream_event` text deltas, else each `assistant` message's text blocks; the reply is
  the `result` event's `result`.
- codex `exec --json`: `item.completed` agent_message items (and the older `msg` form); the reply is the last.
- gemini stream-json: assistant `message` events; the reply is their text joined.
- cursor-agent stream-json (and cursor-* templates that share that binary): Claude's event shape
  (system init, assistant messages, result), so the claude mapping.
- antigravity (agy) stream-json: events keyed `event`; `step_update` text deltas on `agent_response` steps; the
  reply is the `result` event's `result.response`, else the deltas joined.
"""
import json
import os
import signal
import subprocess
import threading
import time
from pathlib import Path

from _job_reason import redact
from _muse import Cancelled, check_environment
from _provider_limits import limit_from_event

DIALECTS = {"claude": "claude", "codex": "codex", "gemini": "gemini", "cursor-agent": "claude", "agy": "antigravity"}


def dialect(binary):
    return DIALECTS.get(Path(str(binary)).name)


def _s(v):
    return v if isinstance(v, str) else ""


def _d(v):
    return v if isinstance(v, dict) else {}


def _decode(line):
    if isinstance(line, bytes):
        line = line.decode("utf-8", errors="replace")
    try:
        value = json.loads(line)
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


def _text(v):
    if isinstance(v, str):
        return v
    if isinstance(v, list):
        parts = [_text(part) for part in v]
        return "\n".join([part for part in parts if part])
    if isinstance(v, dict):
        if isinstance(v.get("text"), str):
            return v["text"]
        if isinstance(v.get("content"), str):
            return v["content"]
        return _text(v.get("content"))
    return ""


def _tool_status(*values):
    out = {}
    for value in values:
        if not isinstance(value, dict):
            continue
        if "is_error" in value:
            out["failed"] = value.get("is_error")
        elif "error" in value:
            out["failed"] = value.get("error")
        for key in ("exit_code", "exitCode", "return_code", "returnCode"):
            if value.get(key) is not None:
                out["exit_code"] = value.get(key)
                break
        if value.get("status") is not None:
            out["status"] = value.get("status")
    return out


def _claude_limit(v):
    hit = limit_from_event(v)
    if hit:
        return [{"kind": "provider_limit", **hit}]
    return []


def _claude_signals(v):
    out = _claude_limit(v)
    kind = _s(v.get("type"))
    if kind == "assistant":
        msg = _d(v.get("message"))
        blocks = msg.get("content") if isinstance(msg.get("content"), list) else []
        if blocks:
            out.append({"kind": "turn"})
        for block in blocks:
            if not isinstance(block, dict):
                continue
            if block.get("type") == "tool_use":
                out.append({
                    "kind": "tool_call",
                    "id": _s(block.get("id")),
                    "name": _s(block.get("name")),
                    "args": _d(block.get("input")),
                })
    elif kind == "user":
        msg = _d(v.get("message"))
        blocks = msg.get("content") if isinstance(msg.get("content"), list) else []
        for block in blocks:
            if not isinstance(block, dict) or block.get("type") != "tool_result":
                continue
            out.append({
                "kind": "tool_result",
                "id": _s(block.get("tool_use_id")),
                "name": _s(block.get("name")),
                "text": _text(block.get("content")),
                **_tool_status(block),
            })
    return out


def _codex_signals(v):
    out = []
    record = _d(v.get("item")) if isinstance(v.get("item"), dict) else _d(v.get("msg"))
    kind = _s(record.get("type"))
    outer = _s(v.get("type"))
    if kind == "agent_message":
        out.append({"kind": "turn"})
    elif kind == "command_execution":
        if outer in ("item.started", "item.created"):
            out.append({
                "kind": "tool_call",
                "id": _s(record.get("id")),
                "name": "shell",
                "args": {"command": _s(record.get("command"))},
            })
        elif outer in ("item.completed", "item.failed"):
            out.append({
                "kind": "tool_result",
                "id": _s(record.get("id")),
                "name": "shell",
                "text": _text(record.get("aggregated_output") or record.get("output")),
                **_tool_status(record),
            })
    elif kind in ("tool_call", "function_call"):
        args = record.get("input")
        if not isinstance(args, dict):
            args = _d(record.get("arguments"))
        out.append({
            "kind": "tool_call",
            "id": _s(record.get("id")),
            "name": _s(record.get("name") or record.get("tool_name")),
            "args": args if isinstance(args, dict) else {},
        })
    elif kind in ("tool_result", "function_call_output", "tool_output"):
        out.append({
            "kind": "tool_result",
            "id": _s(record.get("tool_call_id") or record.get("id")),
            "name": _s(record.get("name") or record.get("tool_name")),
            "text": _text(record.get("output") if "output" in record else record.get("content") or record.get("text")),
            **_tool_status(record, record.get("result")),
        })
    return out


def _gemini_signals(v):
    out = []
    kind = _s(v.get("type"))
    if kind == "message" and _s(v.get("role")) == "assistant":
        out.append({"kind": "turn"})
    elif kind in ("tool_call", "tool_use"):
        args = _d(v.get("args")) or _d(v.get("parameters"))
        out.append({
            "kind": "tool_call",
            "id": _s(v.get("id")),
            "name": _s(v.get("name") or v.get("tool_name")),
            "args": args,
        })
    elif kind in ("tool_result", "tool_response") or (_s(v.get("role")) == "tool" and kind == "message"):
        out.append({
            "kind": "tool_result",
            "id": _s(v.get("tool_call_id") or v.get("id")),
            "name": _s(v.get("name") or v.get("tool_name")),
            "text": _text(v.get("content") if "content" in v else v.get("output")),
            **_tool_status(v, v.get("result")),
        })
    return out


def _antigravity_signals(v):
    out = []
    kind, step = _s(v.get("event")), _s(v.get("step_type"))
    if kind == "step_update" and step == "agent_response" and _s(v.get("text_delta")):
        out.append({"kind": "turn"})
    if kind == "step_update" and "tool" in step and _s(v.get("tool_name")):
        if "result" in step or "response" in step:
            out.append({
                "kind": "tool_result",
                "id": _s(v.get("tool_call_id") or v.get("id")),
                "name": _s(v.get("tool_name")),
                "text": _text(v.get("output") if "output" in v else v.get("text") or v.get("text_delta")),
                **_tool_status(v, v.get("result")),
            })
        else:
            out.append({
                "kind": "tool_call",
                "id": _s(v.get("tool_call_id") or v.get("id")),
                "name": _s(v.get("tool_name")),
                "args": _d(v.get("args")) or _d(v.get("parameters")),
            })
    return out


def signals(kind, line):
    """Structured stream events for deterministic executor logic (stuck detection)."""
    value = _decode(line)
    if value is None:
        return []
    if kind == "claude":
        return _claude_signals(value)
    if kind == "codex":
        return _codex_signals(value)
    if kind == "gemini":
        return _gemini_signals(value)
    if kind == "antigravity":
        return _antigravity_signals(value)
    return []


class CursorAvailability:
    """Bound Cursor connection churn without treating reconnect receipts as progress."""

    def __init__(self, clock=time.monotonic):
        self.clock, self.count, self.since = clock, 0, None
        self.lock = threading.Lock()

    def reason(self):
        return ("Cursor could not reach its model (unavailable after %s reconnects); "
                "the model's quota may be used up — try another model" % self.count)

    def feed(self, raw, progress=False):
        with self.lock:
            value = _decode(raw) or {}
            blocks = _d(value.get("message")).get("content") or []
            assistant_text = value.get("type") == "assistant" and any(
                isinstance(b, dict) and b.get("type") == "text" and b.get("text") for b in blocks)
            if progress or assistant_text or any(s.get("kind") == "tool_call" for s in signals("claude", raw)):
                self.count, self.since = 0, None
            elif (value.get("type"), value.get("subtype")) in (
                    ("connection", "reconnecting"), ("retry", "starting")):
                self.count += 1
                if self.since is None:
                    self.since = self.clock()
        return self.poll()

    def poll(self):
        with self.lock:
            if self.count >= 5 or (self.count >= 3 and self.clock() - self.since >= 120):
                return {"stop": "provider_unavailable", "reason": self.reason()}
        return None


class Mapper:
    """One run's stdout lines -> delta text, and the reply once the run ends."""

    def __init__(self, kind):
        self.kind, self.partial = kind, False
        self.result, self.last, self.texts = None, "", []

    def text(self, line):
        try:
            v = json.loads(line)
        except ValueError:
            return ""
        if not isinstance(v, dict):
            return ""
        out = getattr(self, self.kind)(v)
        if out:
            self.texts.append(out)
        return out

    def claude(self, v):
        kind = _s(v.get("type"))
        if kind == "result" and isinstance(v.get("result"), str):
            self.result = v["result"]
        if kind == "stream_event":
            e = _d(v.get("event"))
            if _s(e.get("type")) == "content_block_delta" and _s(_d(e.get("delta")).get("type")) == "text_delta":
                self.partial = True
                return _s(_d(e.get("delta")).get("text"))
            return ""
        if kind == "assistant" and not self.partial:
            blocks = _d(v.get("message")).get("content")
            text = "".join(_s(b.get("text")) for b in blocks or [] if isinstance(b, dict) and b.get("type") == "text")
            if text:
                self.last = text
            return text
        return ""

    def codex(self, v):
        item, msg = _d(v.get("item")), _d(v.get("msg"))
        text = ""
        if _s(v.get("type")) == "item.completed" and _s(item.get("type")) == "agent_message":
            text = _s(item.get("text"))
        elif _s(msg.get("type")) == "agent_message":
            text = _s(msg.get("message"))
        if text:
            self.last = text
        return text

    def gemini(self, v):
        if _s(v.get("type")) == "message" and _s(v.get("role")) == "assistant":
            return _s(v.get("content"))
        return ""

    def antigravity(self, v):
        kind = _s(v.get("event"))
        if kind == "result" and isinstance(_d(v.get("result")).get("response"), str):
            self.result = v["result"]["response"]
        if kind == "step_update" and _s(v.get("step_type")) == "agent_response":
            return _s(v.get("text_delta"))
        return ""

    def reply(self):
        if self.kind == "antigravity":
            return self.result if self.result is not None else "".join(self.texts)
        if self.kind == "claude":
            return self.result if self.result is not None else self.last
        return self.last if self.kind == "codex" else "".join(self.texts)


def line(event):
    """serde_json's line for a json! object (keys sorted, compact, raw UTF-8) as runner.rs `event_line` writes it."""
    text = json.dumps(event, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    for raw, esc in (("\u0085", "\\u0085"), (" ", "\\u2028"), (" ", "\\u2029")):
        text = text.replace(raw, esc)
    return text + "\n"


def append(path, event, *, durable=False):
    """One whole line; a failed write is truncated away (runner.rs `append`)."""
    data = line(event).encode("utf-8")
    fd = os.open(str(path), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
    try:
        offset = os.fstat(fd).st_size
        try:
            remaining = memoryview(data)
            while remaining:
                written = os.write(fd, remaining)
                if written <= 0:
                    raise OSError("event append made no progress")
                remaining = remaining[written:]
            if durable:
                os.fsync(fd)
        except OSError:
            os.ftruncate(fd, offset)
            raise
    finally:
        os.close(fd)


class Events:
    """`events.jsonl`: kind model.output, payload {turn, partial, delta[, discard]}, seq from 1 across the run."""

    def __init__(self, run_dir):
        self.path, self.seq = Path(run_dir) / "events.jsonl", 0

    def write(self, turn, delta=None, discard=False):
        payload = {"turn": turn, "partial": delta is not None, "delta": redact(delta) if isinstance(delta, str) else ""}
        if discard:
            payload["discard"] = True
        self.seq += 1
        append(self.path, {"seq": self.seq, "kind": "model.output", "payload": payload}, durable=discard)


def run_streaming(command, workspace, env, timeout, on_line, memory_cap_mb=None, on_evidence=None, on_idle=None):
    """_muse.run_process with stdout read line by line: `on_line(bytes)` sees each line as it arrives.
    Returning a dict with {"stop": "..."} from on_line requests early termination.
    Same outcomes: CompletedProcess, TimeoutExpired or Cancelled (SIGTERM) carrying the output so far."""
    check_environment(env)
    from _job_memory import prepare_job
    command, env, marker = prepare_job(command, env)
    proc = subprocess.Popen(command, cwd=workspace, env=env, start_new_session=True, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    from _job_memory import JobProcesses
    processes = JobProcesses(proc.pid, workspace, marker=marker)
    if not hasattr(processes, "evidence"):
        processes.evidence = []
    monitor = None
    if memory_cap_mb is not None and memory_cap_mb > 0:
        from _job_memory import MemoryMonitor

        monitor = MemoryMonitor(memory_cap_mb, proc.pid, processes)
        monitor.start()
    out, err = [], []
    forced = {"meta": None}

    def read_out():
        for raw in iter(proc.stdout.readline, b""):
            out.append(raw)
            try:
                asked = on_line(raw)
                if forced["meta"] is None and asked:
                    forced["meta"] = asked if isinstance(asked, dict) else {"stop": str(asked)}
                    kill_group()
            except Exception:  # a gone reader or full disk must not stop the drain and stall the agent on its pipe
                pass

    def read_err():
        err.append(proc.stderr.read())
    def kill_group():
        processes.kill()

    readers = [threading.Thread(target=f, daemon=True) for f in (read_out, read_err)]
    def text():
        for t in readers:
            t.join()
        decode = lambda parts: b"".join(parts).decode("utf-8", errors="replace")
        return decode(out), decode(err)

    cancelled = False

    def cancel(_signum, _frame):
        # Do not interrupt reader startup or cleanup: both must finish before
        # the executor closes the turn and records its discard.
        nonlocal cancelled
        cancelled = True
    previous = signal.signal(signal.SIGTERM, cancel)
    try:
        try:
            for t in readers:
                t.start()
            deadline = time.monotonic() + timeout
            while True:
                if cancelled:
                    raise KeyboardInterrupt()
                if on_idle is not None and forced["meta"] is None:
                    asked = on_idle()
                    if asked:
                        forced["meta"] = asked
                        kill_group()
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(command, timeout)
                try:
                    code = proc.wait(timeout=min(0.1, remaining))
                    break
                except subprocess.TimeoutExpired:
                    pass
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
            kill_group()
            proc.wait()
            stdout, stderr = text()
            if isinstance(error, subprocess.TimeoutExpired):
                raise subprocess.TimeoutExpired(command, timeout, output=stdout, stderr=stderr) from None
            raise Cancelled(stdout, stderr) from None
        kill_group()
        stdout, stderr = text()
        done = subprocess.CompletedProcess(command, code, stdout, stderr)
        done.job_process_evidence = processes.evidence
        meta = forced["meta"]
        if monitor is not None:
            monitor.close()
            if meta is None:
                meta = monitor.forced_meta()
        if meta is not None:
            setattr(done, "forced_meta", meta)
            setattr(done, "forced_stop", str(meta.get("stop") or "stopped"))
        return done
    finally:
        try:
            if monitor is not None:
                monitor.close()
        finally:
            try:
                kill_group()
                proc.wait()
            finally:
                try:
                    try:
                        try:
                            processes.close()
                        finally:
                            if on_evidence is not None:
                                on_evidence(processes.evidence)
                    finally:
                        # Cleanup deadlines/errors must not mask a received TERM:
                        # the caller still has to close the partial turn. Readers
                        # must finish before that discard, even on this path.
                        if cancelled:
                            raise Cancelled(*text())
                finally:
                    # Keep later TERM harmless until the executor records the
                    # discard and finalizes. Restoring SIG_DFL here leaves a gap
                    # between Cancelled and the executor's emit() handler.
                    signal.signal(signal.SIGTERM, signal.SIG_IGN if cancelled else previous)
