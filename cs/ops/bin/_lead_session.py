#!/usr/bin/env python3
"""Long-lived CLI session backends (stream-json today; Codex app-server later).

Demanded-by: Design.HostedLead 2026-11-11 lane 3. Evolves claude-session-spike/claude_session.py.
"""
from __future__ import annotations

import json
import os
import queue
import re

from _cli_native import stage_git
from _cli_native import stage_claude_login, finish_copies
from _job_reason import redact_obj
import signal
import subprocess
import sys
import threading
import time
import uuid
from abc import ABC, abstractmethod
from pathlib import Path

KEEP_ENV = {"PATH", "USER", "LOGNAME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "SHELL"}
CRED_ENV = re.compile(r"(API[_-]?KEY|SECRET|TOKEN|PASSWORD|CREDENTIAL|^AWS_|SSH_AUTH_SOCK)", re.I)


def child_env(runtime_home: Path, tmpdir: Path, extra: dict | None = None) -> dict[str, str]:
    home = Path.home()
    env = {k: v for k, v in os.environ.items() if k in KEEP_ENV and not CRED_ENV.search(k)}
    env.update(
        HOME=str(runtime_home),
        TMPDIR=str(tmpdir),
        TMP=str(tmpdir),
        TEMP=str(tmpdir),
        GIT_TERMINAL_PROMPT="0",
    )
    env["PATH"] = env.get("PATH", "") + os.pathsep + str(home / ".local/bin")
    if extra:
        env.update(extra)
    env.update(stage_git(tmpdir, home))
    return env


# The lead orchestrates through its Director tools only (cs mcp; land_task is withheld there).
LEAD_TOOL_PREFIX = "mcp__contextstack__"


def allowed_tool(name: str) -> bool:
    return name.startswith(LEAD_TOOL_PREFIX) and len(name) > len(LEAD_TOOL_PREFIX)


def prepare_runtime_home(runtime_home: Path, *, stage_login: bool = True) -> list:
    runtime_home.mkdir(mode=0o700, parents=True, exist_ok=True)
    if sys.platform == "darwin":
        (runtime_home / "Library").mkdir(exist_ok=True)
        link = runtime_home / "Library/Keychains"
        if not link.exists() and not link.is_symlink():
            link.symlink_to(Path.home() / "Library/Keychains", target_is_directory=True)
    return stage_claude_login(runtime_home, Path.home()) if stage_login else []


class SessionBackend(ABC):
    """Backend interface: Claude stream-json today; Codex app-server can subclass later."""

    session_id: str | None = None

    @abstractmethod
    def start(self) -> SessionBackend: ...

    @abstractmethod
    def send(self, text: str) -> None: ...

    @abstractmethod
    def run_turn(self, text: str, timeout: float = 300) -> str: ...

    @abstractmethod
    def collect_turn(self, timeout: float = 300) -> str: ...

    @abstractmethod
    def stop(self) -> int: ...

    @property
    @abstractmethod
    def alive(self) -> bool: ...


class ClaudeStreamSession(SessionBackend):
    """One Claude CLI process on stream-json stdin/stdout."""

    def __init__(
        self,
        workspace: Path,
        runtime_home: Path,
        tmpdir: Path,
        *,
        model: str = "sonnet",
        resume: str | None = None,
        binary: str = "claude",
        sandbox_argv_builder=None,
        mcp_config: dict | None = None,
        tools: str = "",
        host_approvals: bool = True,
        transcript: Path | None = None,
        cli_extra: list[str] | None = None,
        extra_env: dict[str, str] | None = None,
    ):
        self.ws = Path(workspace)
        self.home = Path(runtime_home)
        self.tmp = Path(tmpdir)
        self.model = model
        self.resume = resume
        self.binary = binary
        self.sandbox_argv_builder = sandbox_argv_builder
        self.mcp_config = mcp_config or {"mcpServers": {}}
        self.tools = tools
        self.host_approvals = host_approvals
        self.transcript = transcript
        self.cli_extra = list(cli_extra or [])
        self.extra_env = dict(extra_env or {})
        self.events: queue.Queue = queue.Queue()
        self.session_id = resume
        self.proc: subprocess.Popen | None = None
        self.t0: float | None = None
        self._lock = threading.Lock()
        self._login_lock = threading.Lock()
        self._login_copies = []
        self.login_copy_refresh = []

    def argv(self) -> list[str]:
        args = [
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompts",
            "host",
            "--model",
            self.model,
            "--setting-sources",
            "",
            "--settings",
            '{"disableAllHooks":true}',
            "--strict-mcp-config",
            "--mcp-config",
            json.dumps(self.mcp_config),
            "--disable-slash-commands",
            "--tools",
            self.tools,
        ]
        if self.host_approvals:
            args += ["--permission-prompt-tool", "stdio"]
        if self.resume:
            args += ["--resume", self.resume]
        inner = [self.binary, *self.cli_extra, *args]
        if self.sandbox_argv_builder:
            return self.sandbox_argv_builder(self.binary, [*self.cli_extra, *args])
        return inner

    def start(self) -> ClaudeStreamSession:
        if self.alive:
            return self
        self._finish_login_copies()
        self._login_copies = prepare_runtime_home(self.home)
        self.t0 = time.monotonic()
        try:
            self.tmp.mkdir(parents=True, exist_ok=True)
            self.proc = subprocess.Popen(
                self.argv(),
                cwd=str(self.ws),
                env=child_env(self.home, self.tmp, self.extra_env),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                bufsize=1,
                start_new_session=True,
            )
        except BaseException:
            self._finish_login_copies()
            raise
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()
        return self

    @property
    def alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def _finish_login_copies(self) -> None:
        with self._login_lock:
            if not self._login_copies:
                return
            results = finish_copies(self._login_copies)
            self.login_copy_refresh.extend(results)
            self._log("host", {"login_copy_refresh": results})

    def stop(self) -> int:
        try:
            return self._stop()
        finally:
            self._finish_login_copies()

    def _stop(self) -> int:
        bridge = getattr(self, "mcp_bridge", None)
        if bridge is not None:
            bridge.close()
        if not self.proc:
            return 0
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            return self.proc.wait(10)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(self.proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            return self.proc.wait()

    def send(self, text: str) -> None:
        self._write(
            {
                "type": "user",
                "message": {"role": "user", "content": text},
                "parent_tool_use_id": None,
                "session_id": self.session_id or "",
            }
        )

    def collect_turn(self, timeout: float = 300) -> str:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                msg = self.events.get(timeout=max(0.1, deadline - time.monotonic()))
            except queue.Empty:
                continue
            typ = msg.get("type")
            if typ == "control_request" and msg.get("request", {}).get("subtype") == "can_use_tool":
                request = msg.get("request", {})
                if allowed_tool(str(request.get("tool_name") or "")):
                    decision = {"behavior": "allow", "updatedInput": request.get("input") or {}}
                else:
                    decision = {"behavior": "deny", "message": "Denied by the lead session host.", "interrupt": False}
                self._write(
                    {
                        "type": "control_response",
                        "response": {"subtype": "success", "request_id": msg["request_id"], "response": decision},
                    }
                )
            elif typ == "result":
                if msg.get("is_error"):
                    raise RuntimeError(str(msg.get("result") or msg.get("subtype") or "turn error"))
                return str(msg.get("result") or "").strip()
            elif typ == "_eof":
                raise RuntimeError("CLI exited before result (code %s)" % msg.get("returncode"))
        raise TimeoutError("turn timed out after %ss" % timeout)

    def run_turn(self, text: str, timeout: float = 300) -> str:
        if not self.alive:
            self.start()
        self.send(text)
        return self.collect_turn(timeout=timeout)

    def _write(self, msg: dict) -> None:
        self._log("in", msg)
        assert self.proc and self.proc.stdin
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def _read_stdout(self) -> None:
        assert self.proc and self.proc.stdout
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                msg = {"type": "_unparsed", "raw": line}
            self._log("out", msg)
            if msg.get("session_id"):
                self.session_id = msg["session_id"]
            self.events.put(msg)
        rc = self.proc.wait()
        self._finish_login_copies()
        self.events.put({"type": "_eof", "returncode": rc})

    def _read_stderr(self) -> None:
        if not self.proc or not self.proc.stderr:
            return
        for line in self.proc.stderr:
            self._log("err", {"line": line.rstrip("\n")})

    def _log(self, direction: str, msg: dict) -> None:
        if not self.transcript:
            return
        row = {"t": round(time.monotonic() - (self.t0 or time.monotonic()), 3), "dir": direction, "msg": redact_msg(msg)}
        with self._lock, self.transcript.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(row) + "\n")


def redact_msg(obj):
    return redact_obj(obj)
