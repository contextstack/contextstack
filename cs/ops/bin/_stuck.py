"""Deterministic stuck-lane detection for streaming CLI executors."""
import hashlib
import json
import re
from collections import deque
from pathlib import Path

from _job_reason import clip

DEFAULT_REPEAT = 3
DEFAULT_IDLE_TURNS = 40
DEFAULT_EDIT_CHURN = 4
REPORT_CHARS = 12000

EDIT_TOOLS = {
    "edit", "write", "multiedit", "apply_patch", "fs_edit", "fs_write", "fs_patch",
    "replace_in_file", "create_or_update_file",
}
SHELL_TOOLS = {"bash", "exec", "shell", "terminal", "run"}
CHECK_HINT = re.compile(
    r"(?:^|\b)(pytest|php artisan test|vendor/bin/phpstan|vendor/bin/pint|cargo test|go test|npm test|pnpm test|"
    r"bun test|jest|vitest|rspec|mvn test|gradle test|scripts/check)(?:\b|$)"
)
FAIL_HINT = re.compile(r"(error|failed|traceback|exception|exit(?:ed)?(?: code| status)?\s*[1-9])", re.I)
ZERO_FAILURE = re.compile(
    r"(?i)(?:\b0\s+(?:tests?\s+)?(?:errors?|fail(?:ed|ures?)?)\b|"
    r"\b(?:errors?|fail(?:ed|ures?)?)\s*[:=]\s*0\b|\bno\s+(?:errors?|failures?)\b)"
)
PATCH_FILE = re.compile(r"^\*\*\* (?:Update|Add|Delete) File: (.+)$", re.M)
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def parse_budget_stuck(budget):
    """Parse budget.stuck from a spec budget mapping."""
    config = {
        "enabled": True,
        "repeat": DEFAULT_REPEAT,
        "idle_turns": DEFAULT_IDLE_TURNS,
        "edit_churn": DEFAULT_EDIT_CHURN,
        "writes": True,
    }
    if not isinstance(budget, dict) or "stuck" not in budget:
        return config
    value = budget.get("stuck")
    if value is False:
        config["enabled"] = False
        return config
    if isinstance(value, str):
        if value.strip().lower() == "off":
            config["enabled"] = False
            return config
        raise ValueError('budget.stuck must be a mapping or "off"')
    if not isinstance(value, dict):
        raise ValueError('budget.stuck must be a mapping or "off"')
    unknown = sorted(set(value) - {"repeat", "idle_turns", "edit_churn"})
    if unknown:
        raise ValueError("budget.stuck has unknown keys: " + ", ".join(unknown))
    for key in ("repeat", "idle_turns", "edit_churn"):
        if key not in value:
            continue
        raw = value[key]
        if type(raw) is not int or raw < 1:
            raise ValueError("budget.stuck.%s must be a positive integer" % key)
        config[key] = raw
    return config


def resolve_stuck_writes(*, writes=None, agent_writes=None, has_grant=True, agent_named=False,
                         repo_read_only=False, read_only_check=False):
    """Whether idle_turns applies: writing jobs only.

    Read-only when the agent profile has writes: false, the work order carries writes: false,
    the spec is a read-only repo view, the task check wraps a read-only agent, or a named agent
    job has no write grant. Grantless experiment work orders (no agent) keep idle_turns.
    """
    if writes is False or agent_writes is False or repo_read_only or read_only_check:
        return False
    if writes is True or agent_writes is True:
        return True
    if not has_grant and agent_named:
        return False
    return True


def agent_profile_writes(agents_dir, name):
    """Frontmatter writes: from cs/agents/<name>.md, or None when unknown/malformed."""
    if not name or not re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,62}", str(name)):
        return None
    path = Path(agents_dir) / ("%s.md" % name)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    if not text.startswith("---"):
        return None
    end = text.find("\n---", 3)
    if end < 0:
        return None
    for line in text[3:end].splitlines():
        if ":" not in line or line[:1].isspace():
            continue
        key, value = line.split(":", 1)
        if key.strip() != "writes":
            continue
        raw = value.strip().lower()
        if raw == "true":
            return True
        if raw == "false":
            return False
        return None
    return None


def _normalize(text):
    lines = []
    cleaned = ANSI.sub("", str(text or "")).replace("\r\n", "\n").replace("\r", "\n")
    for line in cleaned.split("\n"):
        line = " ".join(line.strip().split())
        if line:
            lines.append(line)
    return "\n".join(lines)


def _preview(text, lines=3):
    value = _normalize(text)
    parts = value.splitlines()
    return clip("\n".join(parts[:lines]), 1000)


def _truthy(value):
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        return value.strip().lower() in {"1", "true", "yes", "on", "error", "failed"}
    return bool(value)


def _looks_failed(text):
    return bool(FAIL_HINT.search(ZERO_FAILURE.sub("", str(text or ""))))


def _status_failed(event, text):
    statuses = []
    if event.get("failed") is not None:
        statuses.append(_truthy(event.get("failed")))
    if event.get("exit_code") is not None:
        value = event.get("exit_code")
        try:
            statuses.append(int(value) != 0)
        except (TypeError, ValueError):
            statuses.append(_truthy(value))
    if event.get("status") is not None:
        value = str(event.get("status") or "").strip().lower()
        if value in {"ok", "pass", "passed", "success", "succeeded", "completed", "done"}:
            statuses.append(False)
        elif value in {"error", "fail", "failed", "failure"}:
            statuses.append(True)
    return any(statuses) if statuses else _looks_failed(text)


def _safe(value):
    if isinstance(value, str):
        return clip(value, 1000)
    if isinstance(value, list):
        return [_safe(item) for item in value[:20]]
    if isinstance(value, dict):
        return {clip(key, 100): _safe(item) for key, item in list(value.items())[:20]}
    return value


def _command_text(args):
    if isinstance(args, str):
        return args
    if not isinstance(args, dict):
        return ""
    for key in ("command", "cmd", "script"):
        value = args.get(key)
        if isinstance(value, str):
            return value
        if isinstance(value, list):
            return " ".join(str(part) for part in value)
    return ""


def _call_key(tool, args):
    return "%s:%s" % (
        str(tool or "").strip().lower(),
        json.dumps(args if isinstance(args, dict) else {}, sort_keys=True, separators=(",", ":")),
    )


def _is_check(tool, args):
    name = str(tool or "").strip().lower()
    if name in {"check", "test"}:
        return True
    if name not in SHELL_TOOLS:
        return False
    return bool(CHECK_HINT.search(_command_text(args)))


def _edit_paths(name, args):
    out = []
    tool = str(name or "").strip().lower()
    if tool not in EDIT_TOOLS and "edit" not in tool and "write" not in tool and "patch" not in tool:
        return out
    if isinstance(args, dict):
        for key in ("path", "file_path", "filepath", "filename", "target_file"):
            value = args.get(key)
            if isinstance(value, str) and value.strip():
                out.append(value.strip())
        value = args.get("paths")
        if isinstance(value, list):
            out.extend(str(part).strip() for part in value if str(part).strip())
        patch = args.get("patch")
        if isinstance(patch, str):
            out.extend(path.strip() for path in PATCH_FILE.findall(patch))
        edits = args.get("edits")
        if isinstance(edits, list):
            for item in edits:
                if isinstance(item, dict):
                    for key in ("path", "file"):
                        value = item.get(key)
                        if isinstance(value, str) and value.strip():
                            out.append(value.strip())
    seen, uniq = set(), []
    for path in out:
        key = path.strip()
        if key and key not in seen:
            seen.add(key)
            uniq.append(key)
    return uniq


class Monitor:
    def __init__(self, config, changed_state):
        self.config = dict(config or {})
        self.changed_state = changed_state
        self.repeat_counts = {}
        self.repeat_seen = {}
        self.repeat_calls = {}
        self.edit_counts = {}
        self.edit_hashes = {}
        self.check_edits = {}
        self.failed_check = None
        self.edit_activity = 0
        self.turn_edit_activity = 0
        self.pending = {}
        self.pending_fifo = deque()
        self.recent = deque(maxlen=8)
        self.idle_turns = 0
        self.last_state = self._state()
        self.last_changed = tuple(sorted(self.last_state))
        self.turns_seen = 0
        self.check_attempted = False
        self.passing_checks = 0
        self.stopped = None

    def _reset_repeats(self, call_key=None):
        if call_key is None:
            self.repeat_counts.clear()
            self.repeat_seen.clear()
            self.repeat_calls.clear()
            return
        failures = self.repeat_calls.pop(call_key, set())
        for key in failures:
            self.repeat_counts.pop(key, None)
            self.repeat_seen.pop(key, None)
        for seen in self.repeat_calls.values():
            seen.difference_update(failures)

    def _state(self, paths=None):
        try:
            value = self.changed_state(paths) if paths is not None else self.changed_state()
        except TypeError:
            value = self.changed_state()
        if isinstance(value, dict):
            return {str(k): str(v) for k, v in value.items()}
        return {str(path): "" for path in (value or ())}

    def _trip(self, signal, reason, extra=None):
        if self.stopped:
            return self.stopped
        reason = clip(reason, 400)
        payload = {"signal": signal, "reason": reason}
        if isinstance(extra, dict):
            payload.update(_safe(extra))
        self.stopped = {"stop": "stuck:" + signal, "reason": reason, "stuck": payload}
        return self.stopped

    def _take_call(self, event):
        key = event.get("id")
        call = None
        if key and key in self.pending:
            call = self.pending.pop(key)
            try:
                self.pending_fifo.remove(key)
            except ValueError:
                pass
        while call is None and self.pending_fifo:
            candidate = self.pending_fifo.popleft()
            call = self.pending.pop(candidate, None)
        return call or {}

    def _record_recent(self, call, failed, text):
        entry = {
            "tool": str(call.get("name") or "unknown"),
            "args": clip(json.dumps(
                call.get("args") if isinstance(call.get("args"), dict) else {}, sort_keys=True), 220),
            "status": "fail" if failed else "ok",
            "result": clip(_preview(text, lines=2), 300),
        }
        self.recent.append(entry)

    def _handle_call(self, event):
        token = event.get("id") or ("pending-%d" % (len(self.pending) + len(self.pending_fifo) + 1))
        call = {"id": token, "name": str(event.get("name") or ""), "args": event.get("args") or {}}
        paths = _edit_paths(call["name"], call["args"])
        call["before"] = self._state(paths) if paths else {}
        self.pending[token] = call
        self.pending_fifo.append(token)

    def _handle_result(self, event):
        call = self._take_call(event)
        name = call.get("name") or event.get("name") or ""
        args = call.get("args") if isinstance(call.get("args"), dict) else {}
        text = str(event.get("text") or "")
        failed = _status_failed(event, text)
        call_key = _call_key(name, args)
        self._record_recent({"name": name, "args": args}, failed, text)
        if _is_check(name, args):
            self.check_attempted = True
            if not failed:
                self.passing_checks += 1
                self._reset_repeats()
                self.edit_counts.clear()
                self.check_edits.clear()
                self.failed_check = None
        elif not failed:
            self._reset_repeats(call_key)
        failure_key = None
        if failed and text:
            norm = _normalize(text)
            if norm:
                failure_key = hashlib.sha256(norm.encode("utf-8")).hexdigest()
                if _is_check(name, args):
                    self.failed_check = failure_key
        paths = _edit_paths(name, args)
        after = self._state(paths) if paths else {}
        for path in paths:
            if failed:
                continue
            before_hash = (call.get("before") or {}).get(path)
            after_hash = after.get(path)
            hashes = self.edit_hashes.setdefault(path, set())
            if before_hash is not None:
                hashes.add(before_hash)
            self.edit_counts[path] = self.edit_counts.get(path, 0) + 1
            self.edit_activity += 1
            threshold = int(self.config.get("edit_churn") or DEFAULT_EDIT_CHURN)
            cycled = after_hash is not None and after_hash != before_hash and after_hash in hashes
            if after_hash is not None:
                hashes.add(after_hash)
            repeated_check = False
            if self.failed_check:
                key = (path, self.failed_check)
                self.check_edits[key] = self.check_edits.get(key, 0) + 1
                repeated_check = self.check_edits[key] >= threshold
            if self.edit_counts[path] >= threshold and (cycled or repeated_check):
                extra = {"file": path, "edits": self.edit_counts[path], "files": sorted(self.edit_counts)}
                cause = "content returned to an earlier state" if cycled else "same failing check repeated between edits"
                return self._trip(
                    "edit_churn", "edited %s %d times; %s" % (path, self.edit_counts[path], cause), extra)
        if paths and not failed:
            self.failed_check = None
        if failed and text:
            if failure_key:
                key = failure_key
                count = self.repeat_counts.get(key, 0) + 1
                self.repeat_counts[key] = count
                self.repeat_calls.setdefault(call_key, set()).add(key)
                self.repeat_seen.setdefault(key, {"text": _preview(text), "tool": clip(str(name or "tool"), 100)})
                threshold = int(self.config.get("repeat") or DEFAULT_REPEAT)
                if count >= threshold:
                    seen = self.repeat_seen[key]
                    extra = {"count": count, "hash": key, "output": seen["text"], "tool": seen["tool"]}
                    return self._trip("repeat", "%s failed %dx: %s" % (seen["tool"], count, seen["text"]), extra)
        return None

    def _handle_turn(self):
        self.turns_seen += 1
        current = self._state()
        changed = current != self.last_state or self.edit_activity != self.turn_edit_activity
        self.last_state = current
        self.last_changed = tuple(sorted(current))
        self.turn_edit_activity = self.edit_activity
        # Read-only jobs never change files by design; idle_turns would always trip.
        if not self.config.get("writes", True):
            return None
        if changed:
            self.idle_turns = 0
            return None
        self.idle_turns += 1
        threshold = int(self.config.get("idle_turns") or DEFAULT_IDLE_TURNS)
        if self.idle_turns >= threshold:
            files = list(self.last_changed[:8])
            extra = {"turns": self.idle_turns, "files": files}
            reason = "%d turns without new file changes" % self.idle_turns
            if files:
                reason += " (%s)" % ", ".join(files[:3])
            return self._trip("idle_turns", reason, extra)
        return None

    def feed(self, events):
        if not self.config.get("enabled", True) or self.stopped:
            return self.stopped
        for event in events or ():
            kind = event.get("kind")
            if kind == "tool_call":
                self._handle_call(event)
            elif kind == "tool_result":
                tripped = self._handle_result(event)
                if tripped:
                    return tripped
            elif kind == "turn":
                tripped = self._handle_turn()
                if tripped:
                    return tripped
        return None

    def report(self, goal, stop, reason):
        title = "unspecified goal"
        for line in str(goal or "").splitlines():
            text = line.strip().lstrip("#").strip()
            if text:
                title = text
                break
        lines = [
            "# Stuck lane",
            "",
            "Goal: %s" % clip(title, 500),
            "Stop: %s" % clip(stop, 100),
            "Reason: %s" % clip(reason, 500),
            "",
            "## Recent tool calls",
        ]
        if self.recent:
            for item in list(self.recent)[-6:]:
                lines.append("- %s (%s) -> %s: %s" % (
                    item["tool"], item["args"], item["status"], item["result"] or "(no output)"))
        else:
            lines.append("- (no structured tool calls observed)")
        lines += ["", "## Files touched"]
        if self.edit_counts:
            for path, count in sorted(self.edit_counts.items(), key=lambda x: (-x[1], x[0]))[:10]:
                lines.append("- %s (%d edits)" % (path, count))
        else:
            files = list(self.last_changed)
            if files:
                for path in files[:10]:
                    lines.append("- %s" % path)
            else:
                lines.append("- (none)")
        lines.append("")
        return clip("\n".join(lines).rstrip() + "\n", REPORT_CHARS)


def write_report(path, text):
    target = Path(path)
    target.write_text(text, encoding="utf-8")
