#!/usr/bin/env python3
"""Darwin-only: one hosted-lead turn with sandbox enabled; probe login credential reads."""
from __future__ import annotations

import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
KIT = HERE.parents[2]
OPS = KIT / "ops/bin"
STUB = HERE / "stub_stream_cli"
LEAD = KIT / "lead"
sys.path.insert(0, str(OPS))

import _lead_host  # noqa: E402
sys.path.insert(0, str(HERE.parent / "lead-wakeups"))
from fake_events import FakeDirector

CONV = "con_darwin_sandbox"
TURN_TIMEOUT_S = 120
SERVE_READY_S = "60"


def stop_serve(state: Path, conversation: str) -> None:
    st = _lead_host.load_state(_lead_host.state_file(state, conversation))
    pid = st.get("serve_pid")
    if pid:
        pid = int(pid)
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            except OSError:
                break
            time.sleep(0.1)
        else:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
    _lead_host.socket_file(state, conversation).unlink(missing_ok=True)


def workspace(root: Path, message: str) -> tuple[Path, Path]:
    ws = root / "state/jobs/sandbox-ws"
    ws.mkdir(parents=True)
    (ws / "job.json").write_text(json.dumps({"job_id": ws.name, "task": "tasks/turn", "spec": "spec.yaml"}))
    local_stub = ws / "stub_stream_cli"
    shutil.copy(STUB, local_stub)
    local_stub.chmod(stat.S_IRWXU)
    (ws / "session").mkdir(parents=True, exist_ok=True)
    (ws / "session/mcp.json").write_text(json.dumps({"mcpServers": {}}) + "\n", encoding="utf-8")
    shutil.copy(LEAD / "session/host.json", ws / "session/host.json")
    for rel, src in (
        ("spec.yaml", LEAD / "spec.yaml"),
        ("prompts/system.md", LEAD / "prompts/system.md"),
    ):
        dest = ws / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(src, dest)
    turn = ws / "tasks/turn"
    turn.mkdir(parents=True)
    (turn / "goal.md").write_text(message, encoding="utf-8")
    (turn / "check").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    (turn / "check").chmod(stat.S_IRWXU)
    return ws, local_stub


def sandbox_exec_available() -> bool:
    try:
        done = subprocess.run(
            ["sandbox-exec", "-p", "(version 1)(allow default)", "/usr/bin/true"],
            capture_output=True,
            timeout=10,
        )
        return done.returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def main() -> int:
    if sys.platform != "darwin":
        print("darwin_sandbox_smoke: skip (not darwin)")
        return 0
    if not sandbox_exec_available():
        print("darwin_sandbox_smoke: skip (sandbox-exec unavailable in this environment)")
        return 0
    if not STUB.is_file():
        print("darwin_sandbox_smoke: missing stub", file=sys.stderr)
        return 1
    STUB.chmod(stat.S_IRWXU | stat.S_IRGRP | stat.S_IXGRP)
    fake = FakeDirector()
    fake.start()
    fake.add_turn(CONV, "sandbox-ws")
    with tempfile.TemporaryDirectory(prefix="lead-darwin-sandbox-") as raw:
        root = Path(raw)
        login_home = root / "login-home"
        login_home.mkdir()
        protected = login_home / ".contextstack/credentials"
        protected.parent.mkdir()
        protected.write_text("dummy")
        state = root / "state"
        state.mkdir()
        cs_root = root / "cs"
        cfg = cs_root / "config"
        cfg.mkdir(parents=True)
        (cfg / "lead-credential").write_text("cslead_fixture_only\n", encoding="utf-8")
        os.chmod(cfg / "lead-credential", 0o600)
        (cfg / "hosted-lead.json").write_text(
            json.dumps({"conversation_id": CONV, "model": "claude-code-sonnet", "director": fake.url}) + "\n",
            encoding="utf-8",
        )
        env = dict(
            os.environ,
            HOME=str(login_home),
            LAB_STATE_ROOT=str(state),
            LAB_CONFIG_ROOT=str(cs_root / "config"),
            LAB_CODE_ROOT=str(KIT),
            LEAD_HOST_SERVE_READY_S=SERVE_READY_S,
            LEAD_HOST_IDLE_EXIT_S="120",
        )
        env.pop("LEAD_HOST_CLI_EXTRA", None)
        env.pop("LEAD_HOST_NO_SANDBOX", None)
        prev = os.environ.copy()
        os.environ.clear()
        os.environ.update(env)
        try:
            ws, local_stub = workspace(root, "Probe sandbox please.")
            other = state / "lead-sessions/con_other/home/transcript.jsonl"
            other.parent.mkdir(parents=True)
            other.write_text("other-conversation-decoy")
            # Fixture-owned wrapper supplies probes; environment flags never select CLI policy.
            local_stub.write_text("#!/usr/bin/env python3\nimport runpy, sys\nsys.argv = [%r, %r, %r, *sys.argv[1:]]\nrunpy.run_path(%r, run_name='__main__')\n" %
                                  (str(STUB), "--sandbox-probe=" + str(protected),
                                   "--sandbox-probe=" + str(other), str(STUB)))
            t0 = time.monotonic()
            print("darwin_sandbox_smoke: step begin hosted turn", flush=True)
            try:
                reply = _lead_host.client_turn(
                    ws,
                    conversation_id=CONV,
                    config_root=cs_root / "config",
                    state=state,
                    cli_binary=str(local_stub),
                    sandbox=True,
                    timeout=TURN_TIMEOUT_S,
                )
            except TimeoutError as error:
                print(
                    "darwin_sandbox_smoke: client_turn timed out (step=hosted turn, limit=%ds, elapsed=%.2fs): %s"
                    % (TURN_TIMEOUT_S, time.monotonic() - t0, error),
                    flush=True,
                )
                return 1
            print("darwin_sandbox_smoke: step done hosted turn %.2fs" % (time.monotonic() - t0), flush=True)
            if reply.count("SANDBOX_DENY") != 2 or "SANDBOX_LEAK" in reply:
                print("darwin_sandbox_smoke: expected SANDBOX_DENY in reply, got %r" % reply, file=sys.stderr)
                return 1
        finally:
            stop_serve(state, CONV)
            os.environ.clear()
            os.environ.update(prev)
            fake.stop()
    print("darwin_sandbox_smoke: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
