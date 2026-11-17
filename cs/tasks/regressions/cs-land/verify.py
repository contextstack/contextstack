#!/usr/bin/env python3
"""Interrupted cs land must not leave main holding a staged squash."""
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

KIT = Path(__file__).resolve().parents[3]
ENV = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "LAB_"))}
ENV.update(GIT_TERMINAL_PROMPT="0", GIT_CONFIG_GLOBAL="/dev/null", GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t",
           GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
problems = []


def expect(cond, msg, detail=""):
    print(("ok   " if cond else "FAIL ") + msg + ("" if cond else "  " + str(detail)[:800]))
    cond or problems.append(msg)


def git(cwd, *args):
    done = subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, text=True, env=ENV)
    if done.returncode:
        raise SystemExit("git %s: %s" % (args, done.stderr))
    return done.stdout.strip()


def gwt(root):
    root.mkdir(parents=True, exist_ok=True)
    (root / "seed").mkdir(parents=True)
    git(root / "seed", "init", "-q", "-b", "main")
    (root / "seed/README").write_text("seed\n")
    git(root / "seed", "add", "-A")
    git(root / "seed", "commit", "-q", "-m", "seed")
    git(root, "clone", "-q", "--bare", str(root / "seed"), str(root / ".bare"))
    git(root / ".bare", "worktree", "add", "-q", str(root / "main"), "main")
    return root / "main"


def state(main):
    return git(main, "rev-parse", "HEAD"), git(main, "status", "--porcelain")


tmp = Path(tempfile.mkdtemp(prefix="cs-land-")).resolve()
repos = tmp / "repos.json"
fixture_home = tmp / "cs-home"
fixture_home.mkdir(exist_ok=True)
ENV["CS_REPOS_FILE"] = str(repos)
ENV["HOME"] = str(fixture_home)
ENV["GIT_CONFIG_GLOBAL"] = "/dev/null"
os.environ.update({"CS_REPOS_FILE": str(repos), "HOME": str(fixture_home), "GIT_CONFIG_GLOBAL": "/dev/null"})
main = gwt(tmp / "interrupt")
repos.write_text(json.dumps({"demo": {"path": str(main),
                                      "gate": "touch gate-started; while [ ! -f gate-release ]; do sleep 0.05; done; rm -f gate-started gate-release",
                                      "gate_timeout_s": 30}}))
sys.path.insert(0, str(KIT / "ops/bin"))
import _lanes  # noqa: E402

lane = Path(_lanes.ensure_lane(_lanes.repo_config("demo"), "slow")["path"])
(lane / "slow.txt").write_text("slow\n")
_lanes.commit_lane(lane, "slow", main=main, lane="slow")
before = state(main)

proc = subprocess.Popen([sys.executable, "-I", str(KIT / "scripts/cs"), "land", "demo", "slow", "--json"],
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                        env={**ENV, "HOME": str(fixture_home)})
deadline = time.monotonic() + 15
while not (main / "gate-started").exists() and time.monotonic() < deadline:
    time.sleep(0.02)
expect((main / "gate-started").exists(), "slow gate started")
proc.send_signal(signal.SIGTERM)
try:
    proc.wait(timeout=10)
except subprocess.TimeoutExpired:
    proc.kill()
    proc.wait(timeout=5)

expect(state(main) == before, "interrupted land leaves main clean (no staged squash)")
expect(not (main / "slow.txt").exists(), "interrupted land did not commit the lane onto main")

# Dead land lock plus staged squash: the next land clears it.
subprocess.run(["git", "-C", str(main), "merge", "--squash", "lane/slow"], check=True, env=ENV)
expect(subprocess.run(["git", "-C", str(main), "diff", "--cached", "--quiet"], env=ENV).returncode != 0,
       "staged squash planted")
common = subprocess.check_output(["git", "-C", str(main), "rev-parse", "--path-format=absolute", "--git-common-dir"],
                                 text=True, env=ENV).strip()
lock = Path(common) / "cs-land.lock"
lock.write_text("999999\n")
rec = subprocess.run([sys.executable, "-I", str(KIT / "scripts/cs"), "land", "demo", "slow", "--json"],
                     capture_output=True, text=True, env={**ENV, "HOME": str(fixture_home)}, timeout=60)
expect(git(main, "status", "--porcelain") == "", "next land does not leave a staged squash on main")
expect("leftover landing squash" not in (rec.stdout + rec.stderr), "next land does not refuse with squash trap", rec.stdout[-300:])

if problems:
    print("cs-land: %d failed" % len(problems))
    raise SystemExit(1)
print("cs-land: ok")
