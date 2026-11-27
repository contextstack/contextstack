#!/usr/bin/env python3
"""Interrupted cs land must not leave main holding a staged squash."""
import hashlib
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

KIT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "_lib"))
from hermetic_env import hermetic_env

ENV = {k: v for k, v in hermetic_env().items() if not k.startswith(("GIT_", "CS_"))}
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


temp = tempfile.TemporaryDirectory(prefix="cs-land-")
tmp = Path(temp.name).resolve()
repos = tmp / "repos.json"
fixture_home = tmp / "cs-home"
fixture_home.mkdir(exist_ok=True)
ENV["CS_REPOS_FILE"] = str(repos)
ENV["HOME"] = str(fixture_home)
ENV["GIT_CONFIG_GLOBAL"] = "/dev/null"
os.environ.update({"CS_REPOS_FILE": str(repos), "HOME": str(fixture_home), "GIT_CONFIG_GLOBAL": "/dev/null"})
main = gwt(tmp / "interrupt")
repos.write_text(json.dumps({"demo": {"path": str(main), "land_check": "host",
                                      "gate": "touch gate-started; while [ ! -f gate-release ]; do sleep 0.05; done; rm -f gate-started gate-release",
                                      "gate_timeout_s": 30}}))
sys.path.insert(0, str(KIT / "ops/bin"))
import _lanes  # noqa: E402
expect(_lanes.repo_config("demo")["land_check"] == "host", "explicit host selects host mode")
expect(_lanes.repo_config("demo", repos={"demo": str(main)})["land_check"] == "sandbox",
       "a repo without a setting defaults to sandbox mode")

lane = Path(_lanes.ensure_lane(_lanes.repo_config("demo"), "slow")["path"])
(lane / "slow.txt").write_text("slow\n")
_lanes.commit_lane(lane, "slow", main=main, lane="slow")
before = state(main)

(main / "README").write_text("owner staged\n")
git(main, "add", "README")
(main / "README").write_text("owner unstaged\n")
(main / "scratch").write_text("owner untracked\n")
dirty = state(main)
refusal = subprocess.run([sys.executable, "-I", str(KIT / "scripts/cs"), "land", "demo", "slow", "--json"],
                         capture_output=True, text=True, env=ENV, timeout=4)
expect(refusal.returncode == 2 and "uncommitted changes" in refusal.stdout, "land refuses owner changes", refusal.stdout)
expect(state(main) == dirty and git(main, "show", ":README") == "owner staged"
       and (main / "README").read_text() == "owner unstaged\n"
       and (main / "scratch").read_text() == "owner untracked\n", "owner changes survive")
if problems:
    raise SystemExit(1)
git(main, "reset", "--hard", "HEAD")
(main / "scratch").unlink()

with _lanes.lane_lock(_lanes.repo_config("demo"), "slow"):
    rc, result = _lanes.land("demo", "slow")
    expect(rc == 2 and "busy" in result.get("reason", ""), "land refuses a busy lane", result)
    try:
        with _lanes.lane_lock(_lanes.repo_config("demo"), "slow"):
            expect(False, "second job acquired a busy lane")
    except _lanes.LaneError:
        pass

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

interrupted_stdout, interrupted_stderr = proc.communicate(timeout=5)
# Restore removes only what the merge introduced: the gate's own untracked file stays (no git clean on main).
expect(state(main) == (before[0], "?? gate-started"), "interrupted land leaves main clean (no staged squash)",
       interrupted_stdout + interrupted_stderr)
expect(not (main / "slow.txt").exists(), "interrupted land did not commit the lane onto main")
(main / "gate-started").unlink()  # the owner's to clear, not the land's

# A TERM-ignoring descendant keeps the output pipe open unless the whole group is killed.
cfg = json.loads(repos.read_text())
cfg["demo"].update(gate="(trap '' TERM; sleep 3; touch '%s') & wait" % (tmp / "escaped"), gate_timeout_s=1)
repos.write_text(json.dumps(cfg))
timeout = subprocess.run([sys.executable, "-I", str(KIT / "scripts/cs"), "land", "demo", "slow", "--json"],
                         capture_output=True, text=True, env=ENV, timeout=8)
expect(timeout.returncode == 1 and json.loads(timeout.stdout).get("stage") == "gate",
       "timed-out gate returns red", timeout.stdout + timeout.stderr)
expect(not (tmp / "escaped").exists() and state(main) == before, "gate descendants stop before restoring main")
cfg["demo"]["gate"] = "true"
repos.write_text(json.dumps(cfg))

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
expect(lock.exists(), "recovery keeps the landing lock inode")
expect(git(main, "status", "--porcelain") == "", "next land does not leave a staged squash on main")
expect("leftover landing squash" not in (rec.stdout + rec.stderr), "next land does not refuse with squash trap", rec.stdout[-300:])

git(main, "config", "user.name", "t")
git(main, "config", "user.email", "t@t")
(main / ".gitignore").write_text("/assets/generated\n")
git(main, "add", ".gitignore")
git(main, "commit", "-qm", "fixture ignore rule")
lane = Path(_lanes.ensure_lane(_lanes.repo_config("demo"), "ignored")["path"])
(lane / "assets").mkdir()
(lane / "assets/generated").write_text("candidate\n")
(lane / "assets/added").write_text("candidate\n")
git(lane, "add", "-f", "assets")
git(lane, "commit", "-qm", "add assets")
(main / "assets").mkdir()
owner_asset = main / "assets/generated"
owner_asset.write_text("owner\n")
before = state(main)
rc, result = _lanes.land("demo", "ignored")
expect(rc == 1 and state(main) == before and owner_asset.read_text() == "owner\n",
       "host merge refuses to overwrite ignored owner files", result)
owner_asset.unlink()
cfg["demo"].update(gate="exit 1", gate_timeout_s=30)
repos.write_text(json.dumps(cfg))
rc, result = _lanes.land("demo", "ignored")
expect(rc == 1 and result.get("stage") == "gate" and state(main) == before
       and (main / "assets").is_dir() and not list((main / "assets").iterdir()),
       "red host merge restores an owner's pre-existing empty directory", result)

lane = Path(_lanes.ensure_lane(_lanes.repo_config("demo"), "isolated")["path"])
(lane / "candidate").write_text("candidate\n")
_lanes.commit_lane(lane, "candidate", main=main, lane="isolated")
cfg["demo"].update(land_check="sandbox", gate='test -f candidate && test ! -e "%s/candidate"' % main)
repos.write_text(json.dumps(cfg))
expect(_lanes.repo_config("demo").get("land_check") == "sandbox", "owner can opt into sandboxed landing")

import _land_check  # noqa: E402
import _land_host_probes  # noqa: E402

# This fixture has no dependency manifests or owner toolchains.
toolchains = patch.multiple(_land_check._toolchains, discover_path_dirs=lambda **_: (),
                           discover=lambda **_: (), discover_sandbox_links=lambda **_: ())
toolchains.start()

estate = fixture_home / ".contextstack"
probe_file = estate / "config/land-host-probes.json"
probe_file.parent.mkdir(parents=True)
probe_policy = {"probes": {"fixture": []},
                "trigger_paths": [*_land_host_probes.DIRECT, "cs/tasks/regressions/fixture/"]}
probe_file.write_text(json.dumps(probe_policy))
renderer = estate / "current/bin/cs-sandbox"
renderer.parent.mkdir(parents=True)
renderer.write_text('#!/bin/sh\ntest "$*" = "--profile land-check --request -" || exit 2\n'
                    'cat >/dev/null\nprintf "(version 1)\\n(allow default)\\n'
                    '(deny file-write* (subpath \\"%s\\"))\\n"\n' % main)
renderer.chmod(0o755)
launcher = tmp / "fixture-launcher"
launcher.write_text('#!/bin/sh\ntest "$1" = -f && test -s "$2" || exit 2\nshift 2\nexec "$@"\n')
launcher.chmod(0o755)

# The fixture launcher lets the worktree/receipt flow run even under an outer sandbox.
darwin_temp = tmp / "darwin/T"
darwin_temp.mkdir(parents=True)
real_check_output = subprocess.check_output


def check_output(argv, **kwargs):
    if argv == ["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"]:
        return str(darwin_temp)
    return real_check_output(argv, **kwargs)


before = state(main)
state_dir = estate / "state"
scratch_root = _land_check.land_scratch(state_dir, Path(git(main, "rev-parse", "--path-format=absolute", "--git-common-dir")))
bwrap = tmp / "bwrap"
bwrap.write_text("#!/bin/sh\ntest \"$1\" = --unshare-user && test \"$2\" = --ro-bind\n")
bwrap.chmod(0o755)
mac_renderer = renderer.read_bytes()
renderer.write_text('#!/bin/sh\n'
                    'test "$1" = --policy && test "$3" = --profile && test "$4" = lane-check-run '
                    '&& test "$5" = --request-json && test "$7" = -- || exit 99\n'
                    'shift 7\necho "Linux sandbox launcher invoked"\nexec "$@"\n')
with patch.object(_land_check, "sys", SimpleNamespace(platform="linux", path=sys.path)), \
        patch.object(_land_check, "LINUX_BWRAP_PATH", str(tmp)):
    custom_hooks = tmp / "custom-hooks"
    custom_hooks.mkdir()
    hook = custom_hooks / "pre-commit"
    hook.write_text("#!/bin/sh\necho unexpected host hook\nexit 1\n")
    hook.chmod(0o755)
    cfg["demo"]["hooks_path"] = str(custom_hooks)
    repos.write_text(json.dumps(cfg))
    rc, result = _lanes.land("demo", "isolated")
    expect(rc == 2 and "cannot run this pre-commit hook" in result.get("reason", "")
           and state(main) == before,
           "Linux refuses a custom pre-commit hook that would run on the host", result)
    del cfg["demo"]["hooks_path"]
    cfg["demo"]["gate"] = ('test -f candidate && test ! -e "%s/candidate" && '
                           '{ echo "detached gate ran"; exit 42; }' % main)
    repos.write_text(json.dumps(cfg))
    rc, result = _lanes.land("demo", "isolated")
    expect(rc == 1 and result.get("stage") == "gate" and result["gate"]["mode"] == "sandbox"
           and "Linux sandbox launcher invoked\ndetached gate ran" in result.get("output_tail", "")
           and state(main) == before,
           "Linux runs a non-trigger gate through the installed bubblewrap launcher in a detached worktree", result)
renderer.write_bytes(mac_renderer)

with patch.object(_land_check, "sys", SimpleNamespace(platform="darwin")), \
        patch.object(_land_check.subprocess, "check_output", side_effect=check_output):
    with patch.object(_land_check, "SEATBELT", str(tmp / "missing-seatbelt")):
        rc, result = _lanes.land("demo", "isolated")
        expect(rc == 2 and result.get("stage") == "sandbox-readiness"
               and '"land_check": "host"' in result.get("reason", "") and state(main) == before
               and not list((state_dir / "land-check").glob("land-*")),
               "unavailable Seatbelt refuses the land before creating a check worktree", result)
    with patch.object(_land_check, "SEATBELT", str(launcher)):
        cfg["demo"]["gate"] = "echo changed > candidate"
        repos.write_text(json.dumps(cfg))
        rc, result = _lanes.land("demo", "isolated")
        expect(rc == 2 and "modified the tracked merge tree" in result.get("reason", "") and state(main) == before,
               "green gate that mutates tracked bytes is refused", result)

    if sys.platform == "darwin" and subprocess.run(
            [_land_check.SEATBELT, "-p", "(version 1)(allow default)", "/usr/bin/true"],
            capture_output=True, env=ENV).returncode == 0:
        cfg["demo"]["gate"] = 'echo escape > "%s/README"' % main
        repos.write_text(json.dumps(cfg))
        rc, result = _lanes.land("demo", "isolated")
        expect(rc == 1 and result.get("stage") == "gate" and state(main) == before,
               "Seatbelt denies a gate write to main", result)
    else:
        print("cs-land/write-main: skipped: needs host (nested Seatbelt)")

    with patch.object(_land_check, "SEATBELT", str(launcher)):
        pid_file = tmp / "detached-gate-child"
        daemon_fixture = tmp / "daemon.py"
        daemon_fixture.write_text(
            "import subprocess, sys\nfrom pathlib import Path\n"
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'],"
            " start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,"
            " stderr=subprocess.DEVNULL)\n"
            f"Path({str(pid_file)!r}).write_text(str(child.pid))\nraise SystemExit(1)\n")
        cfg["demo"]["gate"] = f'"{sys.executable}" "{daemon_fixture}"'
        repos.write_text(json.dumps(cfg))
        rc, result = _lanes.land("demo", "isolated", state=state_dir)
        daemon_pid = pid_file.read_text().strip() if pid_file.exists() else ""
        daemon = subprocess.run(["ps", "-o", "stat=", "-p", daemon_pid],
                                capture_output=True, text=True) if daemon_pid else None
        expect(rc == 1 and result.get("stage") == "gate" and daemon is not None
               and (daemon.returncode == 1 or daemon.stdout.lstrip().startswith("Z")),
               "sandbox gate sweeps descendants that leave its process group", result)
        stale = scratch_root / "land-stale"
        stale.mkdir(parents=True)
        git(main, "worktree", "add", "--detach", str(stale / "wt"), "HEAD")
        stale_receipt = Path(common) / ("cs-land-check-" + "0" * 32)
        stale_receipt.write_text("stale")
        other = scratch_root.with_name("another-repo") / "land-active"
        other.mkdir(parents=True)
        cfg["demo"].update(land="merge", gate='test -f candidate && test ! -e "%s/candidate" && '
                           'test "${CS_REPOS_FILE-unset}" = unset && test -d "$HOME" && '
                           'test -d "$DARWIN_USER_TEMP_DIR" && test -d "$DARWIN_USER_CACHE_DIR" && '
                           'test -d "$XDG_CACHE_HOME" && '
                           'case "$CARGO_TARGET_DIR" in "%s"/land-*/target) true;; *) false;; esac && '
                           'echo "seatbelt-fixture: skipped: needs host (fresh profile)"'
                           % (main, scratch_root))
        repos.write_text(json.dumps(cfg))
        tip = git(main, "rev-parse", "lane/isolated")
        rc, result = _lanes.land("demo", "isolated")
        expect(rc == 0 and (main / "candidate").is_file() and
               git(main, "show", "-s", "--format=%P", "HEAD") == before[0] + " " + tip,
               "sandbox gate sees candidate while main stays untouched, then lands a merge commit", result)
        expect(not stale.exists() and not stale_receipt.exists() and other.is_dir()
               and str(stale / "wt") not in git(main, "worktree", "list", "--porcelain"),
               "sandbox land reclaims stale state scoped to its repository")
        expect(result.get("gate", {}).get("uncovered") == ["fixture", "seatbelt-fixture"],
               "sandbox result reports the nested probe as uncovered", result)
        expect(result.get("gate", {}).get("coverage_complete") is False,
               "a skipped nested probe leaves the sandbox coverage incomplete", result)

        probe = main / "cs/tasks/regressions/fixture/check"
        probe.parent.mkdir(parents=True)
        (probe.parent / "goal.md").write_text("---\nhost_only: fixture\n---\n")
        probe.write_text("#!/bin/sh\n"
                         'test "$(../target/release/cs-sandbox)" = "(deny file-read*)" && '
                         'test "$(ops/bin/probe-renderer)" = "candidate renderer" || '
                         '{ echo "FAIL Seatbelt rule is missing or the candidate renderer was not used"; exit 1; }\n'
                         "echo 'base probe ran'\n")
        probe.chmod(0o755)
        skipped_probe = probe.with_name("skip")
        skipped_probe.write_text("#!/bin/sh\necho 'SKIP: nested Seatbelt refused'\n")
        skipped_probe.chmod(0o755)
        ops = main / "cs/ops/bin"
        ops.mkdir(parents=True)
        (ops / "probe-renderer").write_text("#!/bin/sh\necho base renderer\n")
        (ops / "probe-renderer").chmod(0o755)
        git(main, "add", "cs")
        git(main, "commit", "-qm", "base probe")
        lane = Path(_lanes.ensure_lane(_lanes.repo_config("demo"), "sandbox-change")["path"])
        (lane / "sandbox").mkdir()
        (lane / "sandbox/policy").write_text("(allow file-read*)")
        (lane / "cs/ops/bin/probe-renderer").write_text("#!/bin/sh\necho candidate renderer\n")
        (lane / probe.relative_to(main)).write_text("#!/bin/sh\nexit 88\n")
        _lanes.commit_lane(lane, "sandbox change", main=main, lane="sandbox-change")
        cfg["demo"]["gate"] = 'echo "check: host-only uncovered: fixture"'
        repos.write_text(json.dumps(cfg))
        for invalid in (None, "{}", '{"probes": {"../bad": []}, "trigger_paths": []}'):
            probe_file.unlink(missing_ok=True)
            if invalid is not None:
                probe_file.write_text(invalid)
            rc, result = _lanes.land("demo", "sandbox-change", allow_agent_config=True)
            expect(rc == 2 and "invalid or missing owner probe list" in result.get("reason", ""),
                   "missing or invalid probe list fails closed", result)
        probe_file.write_text(json.dumps(probe_policy))
        rc, result = _lanes.land("demo", "sandbox-change", allow_agent_config=True)
        expect(rc == 2 and "needs owner host probes" in result.get("reason", ""),
               "a sandbox change is refused unattended", result)
        with patch.object(_land_check, "sys", SimpleNamespace(platform="linux")):
            rc, result = _lanes.land("demo", "sandbox-change", allow_agent_config=True)
            expect(rc == 2 and result.get("stage") == "host-probes"
                   and "needs owner host probes" in result.get("reason", ""),
                   "Linux refuses trigger paths before checking Seatbelt availability", result)
        with patch.dict(os.environ, {"CS_STATE_ROOT": str(tmp / "relocated-state")}):
            rc, result = _lanes.land("demo", "sandbox-change", allow_agent_config=True)
            expect(rc == 2 and "relocated-state/config/land-host-probes.json" in result.get("reason", ""),
                   "owner probe policy lookup honours CS_STATE_ROOT", result)
        with patch.object(_land_host_probes.sys, "stdin", SimpleNamespace(isatty=lambda: False)):
            rc, result = _lanes.land("demo", "sandbox-change", allow_agent_config=True, host_probes=True)
            expect(rc == 2 and "interactive terminal" in result.get("reason", ""),
                   "host probes need an interactive terminal or explicit confirmation", result)
        cargo = tmp / "cargo"
        cargo.write_text('#!/bin/sh\n'
                         'test "$*" = "build --locked --release -p cs-sandbox --target-dir $7" || exit 98\n'
                         'mkdir -p "$7/release" && '
                         '{ printf "#!/bin/sh\\n"; printf "echo \\047"; cat sandbox/policy; printf "\\047\\n"; } '
                         '> "$7/release/cs-sandbox" && chmod +x "$7/release/cs-sandbox"\n')
        cargo.chmod(0o755)
        with patch.dict(os.environ, {"PATH": str(tmp) + os.pathsep + os.environ["PATH"]}), \
                patch.object(_land_host_probes, "SEATBELT", "/usr/bin/true"), \
                patch.object(_land_host_probes.sys, "platform", "darwin"):
            base = state(main)[0]
            rc, result = _lanes.land("demo", "sandbox-change", allow_agent_config=True,
                                     host_probes=True, yes_host_probes=True)
            expect(rc == 1 and result.get("stage") == "host-probes"
                   and "FAIL Seatbelt rule is missing" in result.get("output_tail", "")
                   and result["gate"]["coverage_complete"] is False and state(main)[0] == base,
                   "attended probes reject a weakened candidate Seatbelt rule", result)
            (lane / "sandbox/policy").write_text("(deny file-read*)")
            _lanes.commit_lane(lane, "restore deny rule", main=main, lane="sandbox-change")
            probe_policy["probes"]["fixture/skip"] = []
            probe_file.write_text(json.dumps(probe_policy))
            rc, result = _lanes.land("demo", "sandbox-change", allow_agent_config=True,
                                     host_probes=True, yes_host_probes=True)
            expect(rc == 2 and result["gate"]["uncovered"] == ["fixture/skip"]
                   and result["gate"]["coverage_complete"] is False and state(main)[0] == base,
                   "zero-exit skip during an attended probe remains uncovered and refuses land", result)
            del probe_policy["probes"]["fixture/skip"]
            probe_file.write_text(json.dumps(probe_policy))
            rc, result = _lanes.land("demo", "sandbox-change", allow_agent_config=True,
                                     host_probes=True, yes_host_probes=True)
            expect(rc == 0 and result["gate"]["host_probe_base"] == base
                   and result["gate"]["host_probe_sandbox_sha256"] ==
                   hashlib.sha256(b"#!/bin/sh\necho '(deny file-read*)'\n").hexdigest()
                   and result["gate"]["host_only_covered"] == ["fixture"] and not result["gate"]["uncovered"],
                   "attended land runs the frozen base probe despite the lane's edit", result)
        expect(not list(scratch_root.glob("land-*")) and not list(Path(common).glob("cs-land-check-*")),
               "land removes the temporary worktree and receipt")

        for relative, contents in (
            ("scripts/git-hooks/pre-commit", (KIT.parent / "scripts/git-hooks/pre-commit").read_text()),
            ("scripts/git-hooks/commit-msg", (KIT.parent / "scripts/git-hooks/commit-msg").read_text()),
            ("cs/scripts/hooks/commit-msg", (KIT / "scripts/hooks/commit-msg").read_text()),
            ("cs/scripts/ops-cap.py", "pass\n"),
            ("cs/scripts/check-red-gate", "pass\n"),
            ("cs/scripts/check", "#!/bin/sh\ntest -f cs/candidate\n"),
            ("cs/scripts/hooks/pre-commit", "#!/bin/sh\necho 'FAIL fixture host check ran'\nexit 1\n"),
        ):
            script = main / relative
            script.parent.mkdir(parents=True, exist_ok=True)
            script.write_text(contents)
            script.chmod(0o755)
        git(main, "add", "cs", "scripts")
        git(main, "-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture hooks")
        cfg["demo"].update(land="squash", land_check="host", hooks_path="scripts/git-hooks",
                           hook_gated=["cs/"], gate=None)
        repos.write_text(json.dumps(cfg))
        lane = Path(_lanes.ensure_lane(_lanes.repo_config("demo"), "hooked")["path"])
        (lane / "cs/candidate").write_text("candidate\n")
        _lanes.commit_lane(lane, "hook candidate", main=main, lane="hooked")
        invalid_receipt = subprocess.run(
            [str(main / "scripts/git-hooks/pre-commit")], cwd=main, capture_output=True, text=True,
            env={**ENV, "CS_LAND_COMMIT": "1", "CS_LAND_CHECK_RECEIPT": "invalid"},
        )
        expect(invalid_receipt.returncode == 1 and "invalid sandboxed land receipt" in invalid_receipt.stderr,
               "an invalid receipt fails closed before the hook can run a host check", invalid_receipt.stderr)
        before = state(main)
        rc, result = _lanes.land("demo", "hooked")
        expect(rc == 1 and result.get("stage") == "commit" and state(main) == before,
               "the host kill switch still runs the rejecting hook gate", result)
        cfg["demo"]["land_check"] = "sandbox"
        repos.write_text(json.dumps(cfg))
        marker = main.parent / "candidate-hook-ran"
        (lane / "cs/scripts/check-red-gate").write_text(
            f"from pathlib import Path\nPath({str(marker)!r}).touch()\n")
        _lanes.commit_lane(lane, "candidate hook callee", main=main, lane="hooked")
        rc, result = _lanes.land("demo", "hooked", state=state_dir)
        expect(rc == 2 and result.get("trigger_paths") == ["cs/scripts/check-red-gate"] and not marker.exists(),
               "changing check-red-gate requires owner probes", result)
        (lane / "cs/scripts/check-red-gate").write_text("pass\n")
        (lane / "cs/scripts/ops-cap.py").write_text(
            f"from pathlib import Path\nPath({str(marker)!r}).touch()\n")
        _lanes.commit_lane(lane, "candidate ops-cap callee", main=main, lane="hooked")
        tip = git(main, "rev-parse", "lane/hooked")
        rc, result = _lanes.land("demo", "hooked", expected=(tip, before[0]), state=state_dir)
        expect(rc == 0 and result.get("gate", {}).get("command") == "cs/scripts/check"
               and result.get("removed", {}).get("branch") == "lane/hooked",
               "a pinned hook-gated tree runs in the sandbox, lands and removes the pinned branch",
               result)
        expect("commit_hooks_skipped" in result.get("gate", {}), "sandbox land records why commit hooks were skipped")
        expect(not marker.exists(), "sandboxed land never executes the candidate hook callee on the host")

toolchains.stop()
if problems:
    print("cs-land: %d failed" % len(problems))
    raise SystemExit(1)
print("cs-land: ok")
