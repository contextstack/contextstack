#!/usr/bin/env python3
"""Host-only installed CLI startup regression; output is statuses, never CLI output.

Use the real HOME read-only so HOME-installed binaries and sibling resources are
exercised. Only the disposable workspace/tmp beneath state/runs are writable.
No staging or login-write grants are needed for --version.
"""
import json
import os
from pathlib import Path
import pwd
import shutil
import subprocess
import sys
import tempfile
from unittest.mock import patch

OPS = Path(__file__).resolve().parents[3] / "ops/bin"


def run(command, workspace, env):
    try:
        return subprocess.run(command, cwd=workspace, env=env, capture_output=True,
                              timeout=60).returncode
    except subprocess.TimeoutExpired:
        return "timeout"


def main():
    if "--darwin-probe" not in sys.argv or sys.platform != "darwin":
        print("SKIP: real CLI startup requires --darwin-probe on the macOS host")
        return 0
    capability = subprocess.run(
        ["/usr/bin/sandbox-exec", "-p", "(version 1)(allow default)", "/usr/bin/true"],
        capture_output=True, timeout=10,
    )
    if capability.returncode:
        if b"sandbox_apply: Operation not permitted" in capability.stderr:
            print("SKIP: nested Seatbelt unavailable; lead must run on the host")
            return 0
        print("FAIL: Seatbelt capability probe")
        return 1
    sys.path.insert(0, str(OPS))
    import _lead_host
    # A lane's HOME may be staged. The passwd entry names the installed owner's HOME.
    home = Path(pwd.getpwuid(os.getuid()).pw_dir).resolve()
    sandbox_cmd = _lead_host._load_sandbox_cmd()
    sandbox_cmd.__globals__["HOME"] = home
    templates = json.loads((OPS / "executor-templates.json").read_text())
    search_path = os.pathsep.join((str(home / ".local/bin"), str(home / ".grok/bin"),
                                 os.environ.get("PATH", "/usr/bin:/bin")))
    runs = home / ".contextstack/state/runs"
    runs.mkdir(parents=True, exist_ok=True)
    failures = 0
    installed = 0
    for name, template in templates.items():
        raw_binary = template["binary"]
        if raw_binary.startswith("~/"):
            binary = str(home / raw_binary[2:])
            binary = binary if os.access(binary, os.X_OK) else None
        elif "/" in raw_binary and not raw_binary.startswith("/"):  # a repo-relative fixture CLI: not a HOME install
            print("SKIP: %s is a repo fixture (%s)" % (name, raw_binary))
            continue
        else:
            binary = shutil.which(raw_binary, path=search_path)
        if not binary:
            print("SKIP: %s is not installed" % name)
            continue
        installed += 1
        with tempfile.TemporaryDirectory(prefix="cli-startup-", dir=runs) as raw:
            root = Path(raw).resolve()
            ws, tmp = root / "ws", root / "tmp"
            ws.mkdir()
            tmp.mkdir()
            env = {"HOME": str(home), "PATH": search_path, "TMPDIR": str(tmp),
                   "LANG": "en_US.UTF-8"}
            # Redirect startup caches/config exactly as declared, without reading or
            # staging credentials. Real HOME remains read-only in the sandbox.
            for key, value in template.get("env", {}).items():
                value = str(value).replace("{{tmp}}", str(tmp))
                env[key] = value
                if value.startswith(str(tmp) + "/"):
                    Path(value).mkdir(parents=True, exist_ok=True)
            baseline = run([binary, "--version"], ws, env)
            if baseline != 0:
                print("SKIP: %s unsandboxed --version rc=%s" % (name, baseline))
                continue
            with patch.dict(os.environ, {"PATH": search_path}):
                command = sandbox_cmd(binary, ["--version"], ws, tmp, tmp / "profile.sb",
                                      login_writes={key: template.get(key, [])
                                                    for key in ("home_reads", "interpreters")})
            result = run(command, ws, env)
            if result != 0:
                failures += 1
                print("FAIL: %s sandboxed --version rc=%s (baseline 0)" % (name, result))
            else:
                print("PASS: %s sandboxed --version rc=0" % name)
    print("real CLI startup: %d installed templates, %d failures" % (installed, failures))
    return int(bool(failures))


if __name__ == "__main__":
    sys.exit(main())
