#!/usr/bin/env python3
"""Stale lane base must not trip protected-path refusal for main-only drift."""
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

KIT = Path(__file__).resolve().parents[3]
ENV = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "CS_"))}
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


tmp = Path(tempfile.mkdtemp(prefix="lane-policy-")).resolve()
repos = tmp / "repos.json"
fixture_home = tmp / "cs-home"
fixture_home.mkdir()
ENV["CS_REPOS_FILE"] = str(repos)
ENV["HOME"] = str(fixture_home)
ENV["GIT_CONFIG_GLOBAL"] = "/dev/null"
os.environ.update({"CS_REPOS_FILE": str(repos), "HOME": str(fixture_home), "GIT_CONFIG_GLOBAL": "/dev/null"})
main = gwt(tmp / "stale-base")
repos.write_text(json.dumps({"policy": {"path": str(main), "land_check": "host"}}))
sys.path.insert(0, str(KIT / "ops/bin"))
import _lanes  # noqa: E402
branch_from = git(main, "rev-parse", "HEAD")
lane_path = Path(_lanes.ensure_lane(_lanes.repo_config("policy"), "ordinary")["path"])
(lane_path / "src").mkdir(parents=True, exist_ok=True)
(lane_path / "src/change.txt").write_text("lane change\n")
_lanes.commit_lane(lane_path, "ordinary lane change", main=main, lane="ordinary")

main_agents = main / "AGENTS.md"
main_agents.write_text("# owner drift after lane branched\n")
git(main, "add", "AGENTS.md")
git(main, "commit", "-q", "-m", "main touches protected path after lane branched")
expect(git(main, "merge-base", "--is-ancestor", branch_from, "HEAD") == "", "main advanced after the lane was created")

done = subprocess.run([sys.executable, "-I", str(KIT / "scripts/cs"), "land", "policy", "ordinary", "--json"],
                      capture_output=True, text=True, env={**ENV, "HOME": str(fixture_home)}, timeout=60)
try:
    out = json.loads(done.stdout)
except ValueError:
    out = {"stdout": done.stdout, "stderr": done.stderr}
expect(done.returncode == 0 and out.get("status") == "landed",
       "land is not refused when only main changed a protected path after branch", out)
expect((main / "src/change.txt").read_text() == "lane change\n", "ordinary lane change landed")

path2 = Path(_lanes.ensure_lane(_lanes.repo_config("policy"), "protected")["path"])
(path2 / "AGENTS.md").write_text("# lane protected change\n")
_lanes.commit_lane(path2, "protected on lane", main=main, lane="protected")
before = git(main, "rev-parse", "HEAD")
done2 = subprocess.run([sys.executable, "-I", str(KIT / "scripts/cs"), "land", "policy", "protected", "--json"],
                       capture_output=True, text=True, env={**ENV, "HOME": str(fixture_home)}, timeout=60)
try:
    out2 = json.loads(done2.stdout)
except ValueError:
    out2 = {"stdout": done2.stdout, "stderr": done2.stderr}
expect(done2.returncode == 2 and "AGENTS.md" in out2.get("reason", ""),
       "a lane that changes a protected path is still refused", out2)
expect(git(main, "rev-parse", "HEAD") == before, "protected refusal leaves main unchanged")

repos.write_text(json.dumps({"policy": {"path": str(main), "land_check": "sandbox"}}))
rc, result = _lanes.land("policy", "protected")
expect(rc == 2 and "AGENTS.md" in result.get("reason", "") and git(main, "rev-parse", "HEAD") == before,
       "sandbox opt-in retains the protected-path check before sandbox setup", result)
expect(not (fixture_home / ".contextstack/state/land-check").exists(),
       "policy refusal never creates a land-check workspace")

if problems:
    print("lane-policy: %d failed" % len(problems))
    raise SystemExit(1)
print("lane-policy: ok")
