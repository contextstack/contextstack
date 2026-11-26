#!/usr/bin/env python3
"""Reference executor profiles via the production v1 transport (offline)."""
import ast
import json
import os
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

OPS = Path(__file__).resolve().parents[3] / "cs/ops/bin"
sys.path.insert(0, str(OPS))
# Node toolchains (e.g. Herd's php) are host-specific read grants that cs-sandbox does not model;
# pin them to none so the golden comparison is the same on every machine.
_no_toolchains = tempfile.NamedTemporaryFile("w", suffix="-toolchains.json", delete=False)
_no_toolchains.write("[]")
_no_toolchains.close()
os.environ["CS_TOOLCHAINS_FILE"] = _no_toolchains.name
__import__("atexit").register(os.unlink, _no_toolchains.name)

import _executor
ns = {"sandbox_cmd": _executor.sandbox_cmd}

from _cli_native import stage  # noqa: E402
from _estate_io import native_caller, native_sandbox_request


def layout(case: str, root: Path):
    home, ws = root / "home", root / "workspace"
    private = root / "private"
    for path in (home, ws, private):
        path.mkdir(parents=True, exist_ok=True)
    os.environ["HOME"] = str(home)
    _executor.HOME = home
    templates = json.loads((OPS / "executor-templates.json").read_text())
    login_writes = None
    lane = None
    tmpl = templates[case if case != "repo-lane" else "claude-code"]

    if case == "codex":
        for src in tmpl.get("links", {}).values():
            if src.startswith("~/"):
                target = home / src[2:]
                target.parent.mkdir(parents=True, exist_ok=True)
                target.touch()
        login_writes = tmpl
    elif case == "repo-lane":
        import _lanes  # noqa: E402
        import subprocess

        seed = root / "seed"
        seed.mkdir()
        os.environ["GIT_CONFIG_GLOBAL"] = "/dev/null"
        os.environ["GIT_TERMINAL_PROMPT"] = "0"
        subprocess.run(["git", "init", "-q", "-b", "main", str(seed)], check=True)
        (seed / "f").write_text("x")
        subprocess.run(["git", "-C", str(seed), "add", "f"], check=True)
        subprocess.run(
            [
                "git",
                "-C",
                str(seed),
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "s",
            ],
            check=True,
        )
        bare = root / ".bare"
        subprocess.run(["git", "clone", "-q", "--bare", str(seed), str(bare)], check=True)
        lane_name = "test"
        lane_dir = root / lane_name
        subprocess.run(
            [
                "git",
                "-C",
                str(bare),
                "worktree",
                "add",
                "-q",
                "-b",
                "lane/" + lane_name,
                str(lane_dir),
            ],
            check=True,
        )
        ws = lane_dir
        lane = dict(main=str(bare), lane=lane_name)

    _, links = stage(tmpl, private, home)
    caller, read_roots = native_caller(tmpl, home, root / "run", ws, private, lane)
    executable = home / ".local/bin" / Path(tmpl["binary"]).name
    executable.parent.mkdir(parents=True, exist_ok=True)
    if case == "codex":
        # Shebang scripts must remain readable by the interpreter by launcher path.
        executable.write_text("#!/usr/bin/python3\n")
    else:
        suffix = "1" if case == "claude-code" else "1/bin/agent"
        payload = home / (".local/share/agent/versions/" + suffix)
        payload.parent.mkdir(parents=True, exist_ok=True)
        payload.write_text("#!/usr/bin/python3\n")
        hop = executable.parent / "agent-current"
        hop.symlink_to("../share/agent/versions/" + suffix)
        executable.symlink_to("agent-current")
    profile = root / f"{case}.sb"
    with patch.object(sys, "platform", "darwin"):
        ns["sandbox_cmd"](str(executable), [], ws, private, profile,
                          read_links=links, login_writes=login_writes or tmpl,
                          caller=caller, read_roots=read_roots)
    # Compare the production output verbatim; rewriting denies conceals policy drift.
    profile_text = profile.read_text()
    request = native_sandbox_request(str(executable), ws, private, home,
                                     read_links=links, login=login_writes or tmpl,
                                     caller=caller, read_roots=read_roots)
    return {**request, "profile_text": profile_text}


def main() -> None:
    if len(sys.argv) not in (3, 4) or sys.argv[1] not in ("profile", "request", "bundle"):
        raise SystemExit("usage: executor_profile_reference.py profile|request|bundle CASE [ROOT]")
    if len(sys.argv) == 4:
        root = Path(sys.argv[3]).resolve()
    else:
        raw = tempfile.mkdtemp(prefix="executor-golden-")
        root = Path(raw)
    data = layout(sys.argv[2], root)
    if sys.argv[1] == "profile":
        sys.stdout.write(data.pop("profile_text"))
    elif sys.argv[1] == "request":
        data.pop("profile_text")
        sys.stdout.write(json.dumps(data))
    else:
        sys.stdout.write(json.dumps(data))


if __name__ == "__main__":
    main()
