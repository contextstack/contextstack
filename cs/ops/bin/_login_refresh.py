"""Host-only CLI login maintenance. Copies from jobs never authorize writeback."""
import json
import os
import selectors
import signal
import subprocess
import time
from pathlib import Path

from _cli_native import finish_copies, stage_copies
from _muse import Cancelled

from _provider_limits import LOGIN_SOURCES as SOURCES, login_signature, notice, clear_pause  # noqa: F401  (re-exported)


def refresh_command(binary, args):
    """No task arguments, project settings or tool permissions are inherited."""
    cli = Path(binary).name
    if cli == "codex":
        return [binary, "--ignore-user-config", "-c", 'cli_auth_credentials_store="file"', "app-server"]
    if cli == "pi" and "--model" in args and args.index("--model") + 1 < len(args):
        return [binary, "--print", "--mode", "text", "--no-session", "--no-tools",
                "--no-extensions", "--no-mcp", "--no-skills", "--no-prompt-templates",
                "--no-context-files", "--model", args[args.index("--model") + 1],
                "Reply with exactly: AUTH_OK"]
    # Grok has automatic refresh and headless tool filtering, but no verified
    # refresh-only CLI or complete settings/hook isolation contract. Notice only.
    return None


def codex_refresh(command, cwd, env, timeout):
    """Only initialize and account/read: no thread, turn, tool, or login/start RPC."""
    proc = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
    deadline = time.monotonic() + timeout
    selector = selectors.DefaultSelector()
    selector.register(proc.stdout, selectors.EVENT_READ)
    pending = b""
    def send(value):
        proc.stdin.write((json.dumps(value) + "\n").encode())
        proc.stdin.flush()
    def receive(expected):
        nonlocal pending
        while time.monotonic() < deadline:
            if b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                obj = json.loads(line)
                if isinstance(obj, dict) and obj.get("id") == expected:
                    return obj
                continue
            if not selector.select(max(0, deadline - time.monotonic())):
                break
            chunk = os.read(proc.stdout.fileno(), 65536)
            if not chunk:
                break
            pending += chunk
            if len(pending) > 1048576:
                break
        return {}
    try:
        send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "contextstack", "version": "1"}}})
        initialized = receive(1)
        if "error" in initialized or "result" not in initialized:
            return False
        send({"method": "initialized", "params": {}})
        send({"id": 2, "method": "account/read", "params": {"refreshToken": True}})
        response = receive(2)
        if "error" in response:
            return False
        result = response.get("result", {})
        return isinstance(result, dict) and isinstance(result.get("account"), dict) and result["account"].get("type") == "chatgpt"
    finally:
        selector.close()
        if proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        proc.wait()
        proc.stdin.close()
        proc.stdout.close()


def maintain(binary, args, results, expired, *, home, root, env, sandbox, run=None):
    """Fresh host staging only. `run` is an optional test seam, never a task command."""
    cli = Path(binary).name
    # owner_changed means the owner refreshed their own login meanwhile: nothing to maintain.
    if cli not in SOURCES or not (expired or any(r["status"] == "task_copy" for r in results)):
        return None
    command = refresh_command(binary, args)
    if command is None:
        return "notice_only"
    root = Path(root)
    try:
        root.mkdir(mode=0o700, parents=True, exist_ok=False)
    except OSError:
        return "refresh_failed"
    records = []
    trusted = False
    status = "refresh_failed"
    try:
        private_home = root / "home"
        private_home.mkdir(mode=0o700)
        login_dir = root / "login"
        records = stage_copies({"copies": {str(login_dir / "auth.json"): SOURCES[cli]}}, root, home)
        if not records:
            return "login_missing"
        refresh_env = {**env, "HOME": str(private_home), "TMPDIR": str(root / "os-tmp"),
                       "TMP": str(root / "os-tmp"), "TEMP": str(root / "os-tmp")}
        (root / "os-tmp").mkdir()
        for key in ("CODEX_HOME", "PI_CODING_AGENT_DIR", "PI_CODING_AGENT_SESSION_DIR", "GROK_HOME"):
            refresh_env.pop(key, None)
        refresh_env["CODEX_HOME" if cli == "codex" else "PI_CODING_AGENT_DIR"] = str(login_dir)
        command = sandbox(command, root)
        if run is not None:
            trusted = run(command, root, refresh_env)
        elif cli == "codex":
            trusted = codex_refresh(command, root, refresh_env, 60)
        else:
            from _muse import run_process
            done = run_process(command, str(root), refresh_env, 60)
            trusted = done.returncode == 0 and done.stdout in ("AUTH_OK", "AUTH_OK\n")
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired, Cancelled):
        status = "refresh_failed"
    finally:
        outcomes = finish_copies(records, trusted_refresh=trusted)
        if trusted and outcomes:
            status = outcomes[0]["status"]
        import shutil
        shutil.rmtree(root)
    return status
