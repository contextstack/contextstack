#!/usr/bin/env python3
"""repo-lanes regression (goal.md): executor lane runs and `cs land`, on temp repos with a fake agent. Network-free."""
import hashlib
import fcntl
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

KIT = Path(__file__).resolve().parents[3]
MONOREPO = KIT.parent
sys.path.insert(0, str(KIT / "tasks/regressions/_lib"))
from hermetic_env import hermetic_env
from cli_execution import command as cli_command

ENV = {k: v for k, v in hermetic_env().items() if not k.startswith(("GIT_", "CS_"))}
ENV.update(GIT_TERMINAL_PROMPT="0", GIT_CONFIG_GLOBAL="/dev/null", GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t",
           GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
ENV.setdefault("CS_TOOLS_BIN", str(KIT.parent / "target/release/cs-tools"))  # the copied code_root has no target/
problems = []


def expect(cond, msg, detail=""):
    print(("ok   " if cond else "FAIL ") + msg + ("" if cond else "  " + str(detail)[:1500]))
    cond or problems.append(msg)


def git(cwd, *args, check=True):
    done = subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, text=True, env=ENV)
    if check and done.returncode:
        raise SystemExit("git %s in %s: %s" % (args, cwd, done.stderr))
    return done.stdout.strip()


def seed(work):
    work.mkdir(parents=True)
    git(work, "init", "-q", "-b", "main")
    (work / "README").write_text("seed\n")
    (work / "src").mkdir()
    (work / "src/app.txt").write_text("one\n")
    (work / "hooks").mkdir()
    hook = work / "hooks/pre-commit"
    hook.write_text("#!/bin/sh\n[ -f HOOK_RED ] || exit 0\necho 'FAIL rule-hook: the hook is red'\nexit 1\n")
    hook.chmod(0o755)
    git(work, "add", "-A")
    git(work, "commit", "-q", "-m", "seed")


def gwt(root):
    """~/src/<repo> as the owner has it: a bare .bare/ and a worktree per branch."""
    seed(root / "seed")
    git(root, "clone", "-q", "--bare", str(root / "seed"), str(root / ".bare"))
    git(root / ".bare", "worktree", "add", "-q", str(root / "main"), "main")
    return root / "main"


def plain(root):
    seed(root / "repo")
    return root / "repo"


tmp = Path(tempfile.mkdtemp(prefix="repo-lanes-")).resolve()
try:
    fixture_home = tmp / "host-home"
    fixture_home.mkdir()
    os.environ["HOME"] = str(fixture_home)
    repos = tmp / "repos.json"
    ENV["CS_REPOS_FILE"] = str(repos)
    os.environ["CS_REPOS_FILE"] = str(repos)
    sys.path.insert(0, str(KIT / "ops/bin"))
    import _lanes  # noqa: E402

    def write_repos(**entries):
        repos.write_text(json.dumps({
            name: {"land_check": "host", **entry} if isinstance(entry, dict) else entry
            for name, entry in entries.items()
        }))

    def cs_land(repo, lane, *extra):
        # cs reads node.env at startup; fixture landing must never consult host config.
        fixture_home = tmp / "cs-home"
        fixture_home.mkdir(exist_ok=True)
        done = subprocess.run([sys.executable, "-I", str(KIT / "scripts/cs"), "land", repo, lane, "--json", *extra],
                              capture_output=True, text=True, env={**ENV, "HOME": str(fixture_home)}, timeout=60)
        try:
            return done.returncode, json.loads(done.stdout)
        except ValueError:
            return done.returncode, {"stdout": done.stdout, "stderr": done.stderr}

    def state(repo_main):
        return git(repo_main, "rev-parse", "HEAD"), git(repo_main, "status", "--porcelain")

    if "--policy-only" in sys.argv:
        from policy_checks import run as check_config_policy
        check_config_policy(_lanes, expect, git, state, cs_land, gwt, write_repos, tmp)
        raise SystemExit(1 if problems else 0)

    from land_git_checks import run as check_land_git
    check_land_git(KIT, expect, tmp)
    if "--land-git-only" in sys.argv:
        raise SystemExit(1 if problems else 0)

    # ---- the executor runs a lane --------------------------------------------------------------------------------
    main = gwt(tmp / "gwt")
    plain_main = plain(tmp / "plain")
    write_repos(demo={"path": str(main)}, flat=str(plain_main))
    from maintenance_checks import seed as seed_maintenance
    seed_maintenance(main, git, ENV)
    other = _lanes.ensure_lane(_lanes.repo_config("demo"), "other")  # a sibling lane the agent must not touch
    # Model a repository beneath HOME even when the host's temp root is elsewhere.
    from _estate_io import native_caller
    sys.path.insert(0, str(next(p / 'tasks/regressions/_lib' for p in Path(__file__).resolve().parents if (p / 'tasks/regressions/_lib').is_dir())))
    from sandbox_profile import render_profile
    read_lane = _lanes.ensure_lane(_lanes.repo_config("demo"), "read-profile")
    roots = _lanes.sandbox_read_roots(main, "read-profile", read_lane["path"])
    caller, _ = native_caller({}, tmp, tmp / 'run', read_lane['path'], tmp / 'private', dict(main=main, lane='read-profile'))
    profile = render_profile(tmp, read_lane['path'], tmp / 'private', read_roots=roots, caller=caller).splitlines()
    expect(roots == (Path(read_lane["path"]), _lanes.expected_lane_gitdir(main, "read-profile"),
                     _lanes.git_common_dir(main)), "lane read roots cover worktree, git dir and common dir", roots)
    expect(all('(allow file-read-metadata (literal %s))' % json.dumps(str(parent)) in profile
               for root in roots for parent in root.parents if parent.is_relative_to(tmp)),
           "HOME lane grants include traversal metadata for every Git ancestor", profile)
    expect(all('(allow file-read* (subpath %s))' % json.dumps(str(root)) in profile for root in roots),
           "HOME lane grants keep Git objects, refs, config, HEAD and info readable")
    (plain_main / ".gitattributes").write_text("README export-ignore\n")
    git(plain_main, "add", ".gitattributes")
    git(plain_main, "commit", "-qm", "set archive export attributes")
    archive_dir = tmp / "archive"
    archive_dir.mkdir()
    _lanes.archive_main(_lanes.repo_config("flat"), archive_dir / "repo")
    expect((archive_dir / "repo/README").read_text() == "seed\n"
           and not (archive_dir / "repo/.git").exists(),
           "read-only snapshot includes committed files marked export-ignore and excludes Git metadata")
    if "--read-profile-only" in sys.argv:
        raise SystemExit(1 if problems else 0)
    code_root = tmp / "code_root"
    (code_root / "ops/bin").mkdir(parents=True)
    for f in (KIT / "ops/bin").iterdir():
        if f.is_file():
            shutil.copy2(f, code_root / "ops/bin" / f.name)
    sys.path.insert(0, str(KIT / "tasks/regressions/_lib"))
    from native_runtime import stage_native_runtime
    stage_native_runtime(code_root / "ops/bin")
    (code_root / "ESTATE").write_text("dev\n")
    (code_root / "specs").mkdir()
    owner_home = tmp / "owner-home"
    owner_home.mkdir()
    (owner_home / '.gitconfig').write_text('[user]\n name = Lane Owner\n email = lane@example.test\n[credential]\n helper = fake-private-helper\n[core]\n excludesFile = ~/.gitignore\n')
    (owner_home / '.gitignore').write_text('owner-ignored\n')
    from _cli_profile import stage_git
    private_git = stage_git(tmp / 'private-git', owner_home)
    config = Path(private_git['GIT_CONFIG_GLOBAL']).read_text()
    expect('Lane Owner' in config and 'fake-private-helper' not in config,
           'private Git config stages identity without credential helpers', config)
    expect((tmp / 'private-git/gitignore').read_text() == 'owner-ignored\n',
           'private Git config stages owner ignore patterns')
    from maintenance_checks import run as check_maintenance
    check_maintenance(tmp, gwt, git, ENV, private_git, expect)
    from commit_checks import run as check_commits, accepted as accepted_commit
    check_commits(tmp, gwt, git, private_git, expect)
    if "--commit-only" in sys.argv:
        raise SystemExit(1 if problems else 0)
    agent = tmp / "agent"
    agent.write_text(r'''#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
targets = json.loads(Path(%r).read_text())
Path("hello.txt").write_text("hello\n")
Path("src/app.txt").write_text("one\ntwo\n")
probe = {}
probe['owner_config'] = 'denied' if subprocess.run(
    ['cat', str(Path(%r) / '.gitconfig')], capture_output=True).returncode else 'readable'
Path('owner-ignored').write_text('ignored')
for name, args in (("git_status", ["status", "--porcelain"]), ("git_diff", ["diff"]),
                   ("git_log", ["log", "-1"])):
    result = subprocess.run(["git", *args], capture_output=True, text=True)
    probe[name] = "ok" if result.returncode == 0 else result.stderr
common = Path(subprocess.check_output(["git", "rev-parse", "--git-common-dir"], text=True).strip())
for rel in ("objects", "refs/heads/lane", "info"):
    try:
        list((common / rel).iterdir())
        probe[rel] = "ok"
    except OSError as error:
        probe[rel] = str(error)
for rel in ("config", "HEAD"):
    try:
        (common / rel).read_text()
        probe[rel] = "ok"
    except OSError as error:
        probe[rel] = str(error)
for name, path in targets.items():
    try:
        with open(path, "a") as f:
            f.write("x")
        probe[name] = "written"
    except OSError as error:
        probe[name] = "denied"
        if name == "packed_refs_lock":
            probe["lock_error"] = error.strerror
Path("committed.txt").write_text("by the agent\n")
c = subprocess.run(["git", "add", "committed.txt"], capture_output=True, text=True)
trace = Path(os.environ["TMPDIR"], "commit.trace")
trace2 = Path(os.environ["TMPDIR"], "commit.trace2")
refs_trace = Path(os.environ["TMPDIR"], "commit.refs")
probe['common'] = str(common)
probe['maintenance_config'] = [subprocess.check_output(['git', 'config', '--get', key], text=True).strip()
    for key in ('maintenance.auto', 'gc.auto')]
packed_before = ((common / 'packed-refs').read_bytes() if (common / 'packed-refs').exists() else b'')
probe['cleanup_absent'] = all(subprocess.run(['git', 'rev-parse', '-q', '--verify', ref],
    capture_output=True).returncode == 1 for ref in ('AUTO_MERGE', 'CHERRY_PICK_HEAD', 'REVERT_HEAD'))
cleanup = subprocess.run(['git', 'update-ref', '-d', 'AUTO_MERGE'], capture_output=True, text=True)
probe['cleanup_exit'], probe['cleanup_stderr'] = cleanup.returncode, cleanup.stderr
c = c.returncode == 0 and subprocess.run(["git", "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", "agent commit"], capture_output=True, text=True, env={**os.environ, "GIT_TRACE": str(trace), "GIT_TRACE2_EVENT": str(trace2), "GIT_TRACE_REFS": str(refs_trace), "GIT_TRACE2_CONFIG_PARAMS": "maintenance.auto,gc.auto,core.fsmonitor,fetch.writeCommitGraph"})
probe['other_ref_git'] = 'denied' if subprocess.run(
    ['git', 'update-ref', 'refs/heads/lane/other', 'HEAD'], capture_output=True).returncode else 'written'
probe['owner_ignore'] = 'ok' if subprocess.run(['git', 'check-ignore', '-q', 'owner-ignored']).returncode == 0 else 'failed'
Path('owner-ignored').unlink()
probe["commit_stderr"] = c.stderr if c else "add failed"
probe["commit_trace"] = trace.read_text() if trace.exists() else ""
probe["commit_trace2"] = trace2.read_text() if trace2.exists() else ""
probe["commit_refs"] = refs_trace.read_text() if refs_trace.exists() else ""
probe["packed_unchanged"] = ((common / "packed-refs").read_bytes() if (common / "packed-refs").exists() else b"") == packed_before
probe["agent_commit"] = "ok" if c and c.returncode == 0 else "failed: %%s" %% (c and c.stderr)
Path(os.environ["TMPDIR"], "probe.json").write_text(json.dumps(probe))
print("done")
''' % (str(tmp / "targets.json"), str(owner_home)))
    agent.chmod(0o755)
    common = main.parent / ".bare"
    (tmp / "targets.json").write_text(json.dumps({
        "main_checkout": str(main / "README"), "other_lane": str(Path(other["path"]) / "README"),
        "repo_config": str(common / "config"), "other_ref": str(common / "refs/heads/lane/other"),
        "packed_refs_lock": str(common / "packed-refs.lock"), "main_ref": str(common / "refs/heads/main"),
        "new_file_in_main": str(main / "src/new.txt")}))
    (code_root / "ops/bin/executor-templates.json").write_text(json.dumps({"fake-agent": {
        "binary": str(agent), "args": ["{{prompt}}"], "cost_class": "subscription", "cost_usd": 0,
        "auth": "subscription"}}))
    task = code_root / "tasks/lane-task"
    task.mkdir(parents=True)
    (task / "goal.md").write_text("Write hello.txt.\n")
    (task / "check").write_text("#!/bin/sh\ntest \"$(cat hello.txt)\" = hello\n")
    (task / "check").chmod(0o755)

    def launch_executor(spec_text, name, task_rel):
        (code_root / "specs" / (name + ".yaml")).write_text("id: %s\nmodel: fake-agent\n%s" % (name, spec_text))
        order = {"executor": "fake-agent", "task": task_rel, "spec": "specs/%s.yaml" % name, "exp": "FIXTURE-LANE",
                 "trial": 1, "timeout_s": 60, "check_window": 30}
        proc = subprocess.Popen(cli_command(sys.executable, code_root / "ops/bin/_executor.py"),
                                cwd=code_root, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, env={**ENV, "HOME": str(owner_home),
                                    "GIT_DIR": str(common), "GIT_WORK_TREE": str(main), "GIT_PREFIX": "",
                                    "GIT_INDEX_FILE": str(tmp / "hook.index"), "GIT_EDITOR": ":",
                                    "GIT_CONFIG_COUNT": "2", "GIT_CONFIG_KEY_0": "maintenance.auto",
                                    "GIT_CONFIG_VALUE_0": "true", "GIT_CONFIG_KEY_1": "gc.auto",
                                    "GIT_CONFIG_VALUE_1": "1",
                                    "GIT_CONFIG_PARAMETERS": "'maintenance.auto'='true' 'gc.auto'='1'"})
        proc.stdin.write(json.dumps(order))
        proc.stdin.close()
        proc.stdin = None
        return proc

    def run_executor(spec_text, name="lane", task_rel="tasks/lane-task", *, cancel=False):
        with launch_executor(spec_text, name, task_rel) as proc:
            if cancel:
                runs = code_root / "ledger/runs"
                deadline = time.monotonic() + 30
                while proc.poll() is None and time.monotonic() < deadline:
                    if any(runs.glob("*/tmp/research-ready")):
                        proc.send_signal(signal.SIGTERM)
                        break
                    time.sleep(0.05)
            try:
                stdout, stderr = proc.communicate(timeout=80)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.communicate()
                raise
            done = subprocess.CompletedProcess(proc.args, proc.returncode, stdout, stderr)
        try:
            return done, json.loads(done.stdout.strip().splitlines()[-1])
        except (IndexError, ValueError):
            return done, {}

    if "--land-only" in sys.argv:
        print("skip executor tests: --land-only (land regressions still run)")
    elif sys.platform == "linux":
        done, value = run_executor("repo: demo\nlane: fix-x\n")
        text = done.stdout + done.stderr
        expect(done.returncode != 0 and "executor sandbox not available on linux" in text.lower(),
               "Linux refuses coding-agent executor jobs without CS_EXECUTOR_ALLOW_UNSANDBOXED", text[:1500])
    elif sys.platform == "darwin" and "--host-only" not in sys.argv:
        main_head = git(main, "rev-parse", "HEAD")
        _lanes.ensure_lane(_lanes.repo_config("demo"), "fix-x")
        git(common, "pack-refs", "--all")
        done, value = run_executor("repo: demo\nlane: fix-x\n")
        lane_dir = main.parent / "fix-x"
        expect(done.returncode == 0 and value.get("stop") == "done", "a lane job passes", (value, done.stderr))
        expect(value.get("branch") == "lane/fix-x" and value.get("lane", {}).get("path") == str(lane_dir),
               "its result names lane/fix-x and the worktree beside main", value)
        expect(git(lane_dir, "symbolic-ref", "--short", "HEAD") == "lane/fix-x", "the worktree is on lane/fix-x")
        expect(value.get("lane", {}).get("base") == main_head, "the lane starts from main's head", value)
        log = git(main, "log", "--format=%s", "main..lane/fix-x").splitlines()
        expect(len(log) == 2 and log[1] == "agent commit" and log[0].startswith("lane fix-x: lane-task"),
               "the agent's commit and the executor's commit of the rest are on the lane branch", log)
        expect(not git(lane_dir, "status", "--porcelain"), "the lane worktree is clean after the pass")
        expect(git(main, "rev-parse", "HEAD") == main_head and not git(main, "status", "--porcelain"),
               "main's head and work tree are untouched")
        expect((main / "README").read_text() == "seed\n" and not (main / "src/new.txt").exists(),
               "main's files are untouched")
        run_dir = code_root / "ledger/runs" / value.get("run_id", "?")
        evidence = json.loads((run_dir / "evidence/manifest.json").read_text()) if run_dir.is_dir() else {}
        paths = sorted(f["path"] for f in evidence.get("files", []))
        expect(evidence.get("capture_state") == "complete" and paths == ["committed.txt", "hello.txt", "src/app.txt"],
               "evidence: complete, the diff against the base covers committed and uncommitted changes", evidence)
        expect(evidence.get("base", {}).get("commit") == main_head, "evidence base is the base commit")
        if done.returncode == 0 and value.get("stop") == "done":
            probe = json.loads((run_dir / "tmp/probe.json").read_text())
            for key in ("main_checkout", "other_lane", "repo_config", "main_ref", "new_file_in_main", "other_ref", "packed_refs_lock"):
                expect(probe.get(key) == "denied", "the sandbox denies writing " + key, probe)
            expect(probe.get('other_ref_git') == 'denied', 'Git cannot update another lane ref', probe)
            expect(probe.get('owner_config') == 'denied', 'cat of owner Git config is denied', probe)
            expect(git(lane_dir, 'log', '-1', '--skip=1', '--format=%an <%ae>') == 'Lane Owner <lane@example.test>',
                   'agent commit uses staged owner identity')
            expect(probe.get('owner_ignore') == 'ok',
                   'agent Git uses staged ignore patterns')
            expect(accepted_commit(probe), "lane commit has no lane-ref or maintenance errors (only proven AUTO_MERGE cleanup)",
                   (probe.get("commit_stderr"), probe.get("commit_refs"), probe.get("commit_trace2")))
            expect(probe.get("agent_commit") == "ok", "the sandbox lets the agent commit in its worktree", probe)
            for key in ("git_status", "git_diff", "git_log", "objects", "refs/heads/lane", "info", "config", "HEAD"):
                expect(probe.get(key) == "ok", "the sandbox permits lane Git read: " + key, probe)
            expect(not (common / "config").read_text().endswith("x") and not (common / "refs/heads/main").exists(),
                   "the repo's config and main ref are intact (main's ref stays packed)")
        else:
            print("skip sandbox probes: nested sandbox-exec unavailable (executor exit %s)" % done.returncode)

        lane_head = git(main, "rev-parse", "lane/fix-x")
        agent.write_text(agent.read_text().replace('Path("hello.txt").write_text("hello\\n")',
                                                   'Path("hello.txt").write_text("hello\\n"); Path("more.txt").write_text("m")'))
        done, value = run_executor("repo: demo\nlane: fix-x\n", name="lane-again")
        expect(done.returncode == 0 and value.get("lane", {}).get("base") == lane_head,
               "a second job reuses the worktree, its base the lane's head", (value, done.stderr))

        done, value = run_executor("repo: flat\nlane: fix-y\n", name="lane-flat")
        expect(done.returncode == 0 and value.get("lane", {}).get("path") == str(plain_main.parent / "fix-y"),
               "a plain repo's lane sits beside it", (value, done.stderr))

        original_agent, original_check = agent.read_text(), (task / "check").read_text()
        (plain_main / "README").write_text("uncommitted\n")
        (plain_main / "local-only").touch()
        before_refs = git(plain_main, "show-ref")
        before_worktrees = git(plain_main, "worktree", "list", "--porcelain")
        (task / "check").write_text("#!/bin/sh\nexit 0\n")
        agent.write_text(r"""#!/usr/bin/env python3
import os
from pathlib import Path
import subprocess
import sys
import time

repo = Path(sys.argv[1].split("snapshot at ", 1)[1].split(". Do not commit", 1)[0])
assert (repo / "README").read_text() == "seed\n"
assert not (repo / "local-only").exists()
try:
    (repo / "README").write_text("changed")
except PermissionError:
    pass
else:
    raise AssertionError("repo snapshot is writable")
assert subprocess.run(["git", "commit", "--allow-empty", "-m", "no"]).returncode != 0
if "cancel" in sys.argv[1]:
    Path(os.environ["TMPDIR"], "research-ready").touch()
    time.sleep(120)
if "fail" in sys.argv[1]:
    sys.exit(23)
print("research probe passed")
""")
        for mode, expected in (("pass", "done"), ("fail", "executor_exit_23"), ("cancel", "cancelled")):
            (task / "goal.md").write_text("research " + mode)
            done, value = run_executor("repo: flat\nrepo_read_only: true\n", name="research-" + mode,
                                       cancel=mode == "cancel")
            expect(value.get("stop") == expected and "branch" not in value,
                   "read-only repo workspace handles " + mode, (value, done.stderr))
            expect(git(plain_main, "show-ref") == before_refs
                   and git(plain_main, "worktree", "list", "--porcelain") == before_worktrees
                   and not list((code_root / "ledger/runs").glob("*/read-only-*"))
                   and (plain_main / "README").read_text() == "uncommitted\n",
                   "read-only repo leaves no branch, worktree or snapshot after " + mode)
        agent.write_text(original_agent)
        (task / "check").write_text(original_check)

        ledger = code_root / "ledger/ledger.jsonl"
        rows = ledger.read_text().count("\n")
        for spec_text, why in (("repo: demo\n", "both repo: and lane:"), ("repo: nope\nlane: a\n", "not in"),
                               ("repo: demo\nlane: Bad/Name\n", "want lowercase"), ("repo: demo\nlane: main\n", "main")):
            done, _ = run_executor(spec_text, name="refused")
            expect(done.returncode != 0 and why in done.stdout + done.stderr, "refused: " + spec_text.strip().replace("\n", "; "),
                   done.stdout + done.stderr)
        (code_root / "tasks/with-setup/setup").mkdir(parents=True)
        for f in ("goal.md", "check"):
            shutil.copy2(task / f, code_root / "tasks/with-setup" / f)
        done, _ = run_executor("repo: demo\nlane: z\n", name="setup", task_rel="tasks/with-setup")
        expect(done.returncode != 0 and "no setup/" in done.stdout + done.stderr, "a lane task with setup/ is refused")
        expect(ledger.read_text().count("\n") == rows, "refusals precede any ledger row")
    else:
        print("skip executor lane tests: %s" % sys.platform)

    pe_path = KIT / "ops/bin/publish-evidence"
    pe_ns = {"__name__": "publish_evidence", "__file__": str(pe_path)}
    exec(compile(pe_path.read_text(), str(pe_path), "exec"), pe_ns)
    # Independent verification 2026-11-11: attacks.py A/B (admin HEAD and branch symref).
    for attack_kind in ("head", "branch", "symlink"):
        main = gwt(tmp / ("tamper-" + attack_kind))
        write_repos(demo={"path": str(main)})
        info = _lanes.ensure_lane(_lanes.repo_config("demo"), "ref-" + attack_kind)
        ws = Path(info["path"])
        admin = _lanes.expected_lane_gitdir(main, info["lane"])
        ref_path = _lanes.git_common_dir(main) / ("refs/heads/" + info["branch"])
        before = git(main, "rev-parse", "main")
        (ws / "README").write_text("PWNED\n")
        if attack_kind == "head":
            (admin / "HEAD").write_text("ref: refs/heads/main\n")
        elif attack_kind == "branch":
            ref_path.write_text("ref: refs/heads/main\n")
        else:
            ref_path.unlink()
            ref_path.symlink_to(_lanes.git_common_dir(main) / "HEAD")
        for action in (
            lambda: _lanes.commit_lane(ws, "attack", main=main, lane=info["lane"]),
            lambda: _lanes.commit_tracked_lane_changes(ws, "attack", main=main, lane=info["lane"]),
            lambda: _lanes.workspace_changed_paths(ws, before, lane_ctx={"main": main, "lane": info["lane"]}),
        ):
            try:
                action()
                expect(False, attack_kind + " tampering refused by host Git")
            except _lanes.LaneError:
                expect(True, attack_kind + " tampering refused by host Git")
        _, _, complete, note, _ = pe_ns["candidate_diff"](
            ws, {"kind": "git", "commit": before}, lane_git={"main": str(main), "lane": info["lane"]})
        expect(not complete and ("HEAD" in note or "ref" in note),
               attack_kind + " tampering refused by evidence capture", note)
        code, result = _lanes.land("demo", info["lane"])
        expect(code == 2, attack_kind + " tampering refused by land", result)
        expect(git(main, "rev-parse", "main") == before, attack_kind + " attack leaves main unchanged")

    # Inject races after validation but before Git executes. Neither can update main.
    for race_kind in ("head", "branch", "cas"):
        main = gwt(tmp / ("race-" + race_kind))
        write_repos(demo={"path": str(main)})
        info = _lanes.ensure_lane(_lanes.repo_config("demo"), "race-" + race_kind)
        ws = Path(info["path"])
        (ws / "README").write_text("raced\n")
        before = git(main, "rev-parse", "main")
        ref_path = _lanes.git_common_dir(main) / ("refs/heads/" + info["branch"])
        original_run = _lanes.subprocess.run
        raced = []
        def racing_run(cmd, *args, **kwargs):
            if "update-ref" in cmd and not raced:
                assert cmd[-3] == "refs/heads/" + info["branch"], cmd
                assert all(re.fullmatch(r"[0-9a-f]{40}", oid) for oid in cmd[-2:]), cmd
                raced.append(True)
                if race_kind == "head":
                    (_lanes.expected_lane_gitdir(main, info["lane"]) / "HEAD").write_text("ref: refs/heads/main\n")
                elif race_kind == "branch":
                    ref_path.write_text("ref: refs/heads/main\n")
                else:
                    other = original_run(["git", "-C", str(main), "commit-tree", before + "^{tree}",
                                          "-p", before, "-m", "concurrent"], capture_output=True, text=True, env=ENV)
                    if other.returncode or not re.fullmatch(r"[0-9a-f]{40}", other.stdout.strip()):
                        raise AssertionError("CAS fixture commit-tree failed: " + other.stderr)
                    ref_path.write_text(other.stdout.strip() + "\n")
            return original_run(cmd, *args, **kwargs)
        _lanes.subprocess.run = racing_run
        try:
            try:
                _lanes.commit_lane(ws, "race", main=main, lane=info["lane"])
                expect(race_kind != "cas", race_kind + " race updates only the explicit lane ref")
            except _lanes.LaneError:
                expect(race_kind != "head", race_kind + " race refuses changed branch")
        finally:
            _lanes.subprocess.run = original_run
        expect(bool(raced) and git(main, "rev-parse", "main") == before,
               race_kind + " race cannot move main")

    # ---- cs land -------------------------------------------------------------------------------------------------
    def lane_with(cfg_name, lane, files, message="lane work"):
        info = _lanes.ensure_lane(_lanes.repo_config(cfg_name), lane)
        for rel, text in files.items():
            (Path(info["path"]) / rel).write_text(text)
        _lanes.commit_lane(Path(info["path"]), message, main=info["main"], lane=lane)
        return Path(info["path"])

    land_main = gwt(tmp / "land")
    write_repos(demo={"path": str(land_main), "gate": "test -f green.txt || { echo 'FAIL rule-green: no green.txt'; "
                                                      "exit 1; }",
                      "hooks_path": "hooks", "hook_gated": ["src/"]})
    before = state(land_main)
    red_lane = lane_with("demo", "red", {"red.txt": "r\n"})
    code, out = cs_land("demo", "red")
    expect(code == 1 and out.get("status") == "red" and out.get("stage") == "gate"
           and any("FAIL rule-green" in line for line in out.get("failed", [])),
           "a red gate: exit 1 naming the FAIL line", out)
    expect(state(land_main) == before and out.get("main_unchanged") is True and red_lane.is_dir(),
           "a red gate leaves main exactly as it was, and the lane", (state(land_main), before))

    hook_lane = lane_with("demo", "hooked", {"src/app.txt": "changed\n", "HOOK_RED": "1"})
    code, out = cs_land("demo", "hooked")
    expect(code == 1 and out.get("stage") == "commit" and out.get("gate", {}).get("command") is None
           and any("FAIL rule-hook" in line for line in out.get("failed", [])),
           "a hook-gated change skips the explicit gate, and a red hook is red", out)
    expect(state(land_main) == before and hook_lane.is_dir(), "a red hook leaves main as it was")

    (land_main / "stray.txt").write_text("dirty\n")
    green_lane = lane_with("demo", "green", {"green.txt": "g\n"}, "Add green.txt")
    code, out = cs_land("demo", "green")
    expect(code == 2 and "uncommitted" in out.get("reason", ""), "a dirty main is refused", out)
    (land_main / "stray.txt").unlink()
    expect(state(land_main) == before, "the refusal changed nothing")

    (green_lane / "loose.txt").write_text("loose\n")
    code, out = cs_land("demo", "green")
    expect(code == 2 and "untracked files" in out.get("reason", "") and out.get("paths") == ["loose.txt"],
           "lane untracked files are refused with their paths", out)
    (green_lane / "loose.txt").unlink()
    code, out = cs_land("demo", "green", "-m", "check-red: rule-green")
    expect(code == 2 and "check-red" in out.get("reason", ""), "a check-red: message is refused", out)

    code, out = cs_land("demo", "green")
    head = git(land_main, "rev-parse", "HEAD")
    expect(code == 0 and out.get("status") == "landed" and out.get("commit") == head and out.get("files") == ["green.txt"],
           "green: lands one squash commit with the lane's change", out)
    expect(git(land_main, "rev-list", "--count", "%s..HEAD" % before[0]) == "1"
           and git(land_main, "log", "-1", "--format=%s") == "Add green.txt" and (land_main / "green.txt").is_file(),
           "main has one new commit carrying the lane's message")
    expect(not green_lane.exists() and not git(land_main, "branch", "--list", "lane/green")
           and out.get("removed") == {"worktree": str(green_lane), "branch": "lane/green"},
           "the lane worktree and branch are removed", out)
    expect(not git(land_main, "status", "--porcelain"), "main is clean after landing")
    expect(git(tmp / "land/seed", "rev-parse", "main") == before[0], "nothing was pushed to origin")

    tracked_lane = lane_with("demo", "tracked", {"green.txt": "base\n"}, "Tracked base")
    (tracked_lane / "green.txt").write_text("base\nplus\n")
    tracked_before = state(land_main)
    code, out = cs_land("demo", "tracked")
    expect(code == 0 and out.get("status") == "landed" and out.get("lane_commit"),
           "tracked lane changes are committed on the lane before landing", out)
    expect((land_main / "green.txt").read_text() == "base\nplus\n"
           and git(land_main, "rev-parse", "HEAD") != tracked_before[0],
           "the tracked lane edits are included in main", out)
    expect(not tracked_lane.exists() and not git(land_main, "branch", "--list", "lane/tracked"),
           "tracked lane cleanup removes worktree and branch")

    empty = _lanes.ensure_lane(_lanes.repo_config("demo"), "empty")
    code, out = cs_land("demo", "empty")
    expect(code == 2 and "nothing to land" in out.get("reason", ""), "an empty lane is refused", out)

    lane_with("demo", "clash", {"README": "lane side\n"})
    (land_main / "README").write_text("main side\n")
    git(land_main, "commit", "-qam", "main moves")
    moved = state(land_main)
    code, out = cs_land("demo", "clash")
    expect(code == 1 and out.get("status") == "conflict" and out.get("files") == ["README"],
           "a conflict: exit 1 naming the file", out)
    expect(state(land_main) == moved, "a conflict leaves main as it was")

    code, out = cs_land("nope", "x")
    expect(code == 2 and "not in" in out.get("reason", ""), "an unknown repo is refused", out)

    from policy_checks import run as check_config_policy
    check_config_policy(_lanes, expect, git, state, cs_land, gwt, write_repos, tmp)

    lock_main = gwt(tmp / "landlock")
    write_repos(lockdemo={"path": str(lock_main), "gate": "touch gate-started; while [ ! -f gate-release ]; do sleep 0.05; done; rm gate-started gate-release",
                          "hooks_path": str(MONOREPO / "scripts/git-hooks"), "gate_timeout_s": 20})
    lane_with("lockdemo", "slow", {"slow.txt": "s\n"}, "slow land")
    hooks = ["-c", "core.hooksPath=%s" % (MONOREPO / "scripts/git-hooks")]
    land_out = {}
    attack = {}

    def land_slow():
        land_out["code"], land_out["body"] = cs_land("lockdemo", "slow")

    def attack_main():
        lock = lock_main.parent / ".bare/cs-land.lock"
        deadline = time.monotonic() + 15
        while not (lock_main / "gate-started").exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        try:
            owner = lock.read_text()
            code, body = cs_land("lockdemo", "slow")
            expect(code == 2 and "another landing" in body.get("reason", ""),
                   "a second land is refused while the first gate runs", body)
            expect(lock.exists() and lock.read_text() == owner,
                   "a refused land preserves the running land's lock and owner")
        except OSError as error:
            expect(False, "running land lock remains readable", error)
        attack["done"] = subprocess.run(["git", "-C", str(lock_main), *hooks, "commit", "-q", "-am", "attack during land"],
                                      capture_output=True, text=True, env=ENV)
        (lock_main / "gate-release").touch()

    land_t = threading.Thread(target=land_slow)
    attack_t = threading.Thread(target=attack_main)
    land_t.start()
    attack_t.start()
    land_t.join(timeout=30)
    attack_t.join(timeout=30)
    attack = attack.get("done")
    expect(attack and attack.returncode != 0 and "cs land" in (attack.stderr + attack.stdout).lower(),
           "a commit on main during cs land is refused by pre-commit", attack and (attack.stderr + attack.stdout))
    code, out = land_out.get("code"), land_out.get("body")
    expect(code == 0 and out and out.get("status") == "landed" and (lock_main / "slow.txt").is_file(),
           "cs land finishes and lands slow.txt", out)
    lock = lock_main.parent / ".bare/cs-land.lock"
    with open(lock, "a") as held:
        try:
            fcntl.flock(held.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(held.fileno(), fcntl.LOCK_UN)
            released = True
        except BlockingIOError:
            released = False
    expect(released, "the finished land releases its lock")
    lock.write_text("99999\n")
    code, body = cs_land("lockdemo", "slow")
    expect("another landing" not in body.get("reason", ""), "a lock file left by a dead land does not block the next land", body)

    # ---- lane .git hijack: host git refuses a rewritten pointer; attacker hooks never run ---------------
    def fresh_hijack(case):
        main = gwt(tmp / ("hijack-" + case))
        write_repos(hijack={"path": str(main)})
        return main

    canary = tmp / "hijack-canary.txt"
    canary.unlink(missing_ok=True)

    def attacker_repo(filter_cmd):
        att = Path(tempfile.mkdtemp(prefix="attacker-", dir=tmp))  # a fresh repo per call: cases reuse filter commands
        git(att, "init", "-q", "-b", "main")
        git(att, "config", "core.hooksPath", "/dev/null")
        git(att, "commit", "-q", "--allow-empty", "-m", "attacker")
        if filter_cmd:
            cfg = att / ".git" / "config"
            cfg.write_text(cfg.read_text() + "\n[filter.attack.clean] smudge=cat\n"
                           "[filter.attack.clean] clean=%s\n" % filter_cmd)
        else:
            hook = att / ".git/hooks/pre-commit"
            hook.write_text("#!/bin/sh\necho pwned >> %s\n" % canary)
            hook.chmod(0o755)
        return att

    def rewrite_git(lane_path, gitdir):
        (lane_path / ".git").write_text("gitdir: %s\n" % Path(gitdir).resolve())

    def rewrite_commondir(main, lane, attacker_git):
        admin = _lanes.expected_lane_gitdir(Path(main), lane)
        (admin / "commondir").write_text("%s\n" % Path(attacker_git).resolve())

    def rewrite_admin_gitdir(main, lane, attacker_git):
        admin = _lanes.expected_lane_gitdir(Path(main), lane)
        (admin / "gitdir").write_text("%s\n" % Path(attacker_git).resolve())

    hijack_main = fresh_hijack("hook")
    hook_info = _lanes.ensure_lane(_lanes.repo_config("hijack"), "hooked")
    hook_lane = Path(hook_info["path"])
    rewrite_git(hook_lane, attacker_repo("").resolve() / ".git")
    (hook_lane / "x.txt").write_text("change\n")
    try:
        _lanes.commit_lane(hook_lane, "hijack", main=hook_info["main"], lane="hooked")
        expect(False, "rewritten .git with pre-commit hook is refused")
    except _lanes.LaneError as error:
        expect(".git" in str(error) and "expected" in str(error),
               "rewritten .git pointer is refused naming the file", str(error))
    expect(not canary.exists(), "attacker pre-commit hook never ran (canary absent)")

    hijack_main = fresh_hijack("filt")
    filt_info = _lanes.ensure_lane(_lanes.repo_config("hijack"), "filtered")
    filt_lane = Path(filt_info["path"])
    rewrite_git(filt_lane, attacker_repo("touch %s" % canary).resolve() / ".git")
    (filt_lane / ".gitattributes").write_text("*.txt filter=attack\n")
    (filt_lane / "z.txt").write_text("z\n")
    try:
        _lanes.commit_lane(filt_lane, "filter", main=filt_info["main"], lane="filtered")
        expect(False, "hijacked .git with clean filter is refused")
    except _lanes.LaneError as error:
        expect(".git" in str(error), "hijacked .git for clean-filter attack is refused", str(error))
    expect(not canary.exists(), "attacker clean filter never ran (canary absent)")

    canary.unlink(missing_ok=True)
    hijack_main = fresh_hijack("attr")
    attr_info = _lanes.ensure_lane(_lanes.repo_config("hijack"), "attronly")
    attr_lane = Path(attr_info["path"])
    (attr_lane / ".gitattributes").write_text("*.txt filter=attack\n")
    (attr_lane / "only.txt").write_text("only\n")
    head_before = git(hijack_main, "rev-parse", attr_info["branch"])
    commit = _lanes.commit_lane(attr_lane, "attrs ok", main=attr_info["main"], lane="attronly")
    expect(commit and not canary.exists(),
           "gitattributes for a filter only in an attacker config does not run under the real git-dir")
    expect(git(hijack_main, "rev-parse", attr_info["branch"]) != head_before
           and git(attr_lane, "show", "HEAD:.gitattributes") == "*.txt filter=attack"
           and git(attr_lane, "show", "HEAD:only.txt") == "only",
           "protected .gitattributes and raw file contents commit on the lane branch")

    hijack_main = fresh_hijack("sb")
    sb_info = _lanes.ensure_lane(_lanes.repo_config("hijack"), "sb-rules")
    caller, roots = native_caller({}, tmp, tmp / 'run', sb_info['path'], tmp / 'private', dict(main=hijack_main, lane='sb-rules'))
    rules = render_profile(tmp, sb_info['path'], tmp / 'private', caller=caller, read_roots=roots).splitlines()
    expect(any("hooks" in r and "deny file-write*" in r for r in rules),
           "Rust profile denies writes under the worktree git dir hooks/", rules)
    expect(any("config" in r and "deny file-write*" in r for r in rules),
           "Rust profile denies writes to worktree git dir config", rules)
    admin = _lanes.expected_lane_gitdir(Path(hijack_main), "sb-rules")
    expect(any("deny file-write*" in r and json.dumps(str(admin / "commondir")) in r for r in rules),
           "Rust profile denies writes to worktree git dir commondir", rules)
    expect(any("deny file-write*" in r and json.dumps(str(admin / "gitdir")) in r for r in rules),
           "Rust profile denies writes to worktree git dir gitdir", rules)

    hijack_main = fresh_hijack("commondir")
    commondir_info = _lanes.ensure_lane(_lanes.repo_config("hijack"), "commondir-atk")
    commondir_lane = Path(commondir_info["path"])
    rewrite_commondir(commondir_info["main"], "commondir-atk",
                      attacker_repo("touch %s #commondir" % canary).resolve() / ".git")
    (commondir_lane / ".gitattributes").write_text("*.txt filter=attack\n")
    (commondir_lane / "c.txt").write_text("c\n")
    try:
        _lanes.commit_lane(commondir_lane, "commondir hijack", main=commondir_info["main"], lane="commondir-atk")
        expect(False, "rewritten commondir with attacker config is refused")
    except _lanes.LaneError as error:
        expect("commondir" in str(error), "rewritten commondir is refused naming commondir", str(error))
    expect(not canary.exists(), "attacker config via commondir never ran (canary absent)")

    hijack_main = fresh_hijack("gitdir")
    gitdir_info = _lanes.ensure_lane(_lanes.repo_config("hijack"), "gitdir-atk")
    gitdir_lane = Path(gitdir_info["path"])
    rewrite_admin_gitdir(gitdir_info["main"], "gitdir-atk", attacker_repo("").resolve() / ".git")
    (gitdir_lane / "g2.txt").write_text("g2\n")
    try:
        _lanes.commit_lane(gitdir_lane, "gitdir hijack", main=gitdir_info["main"], lane="gitdir-atk")
        expect(False, "rewritten admin gitdir is refused")
    except _lanes.LaneError as error:
        expect("gitdir" in str(error) and "expected" in str(error),
               "rewritten admin gitdir is refused naming gitdir", str(error))
    expect(not canary.exists(), "rewritten admin gitdir never ran attacker hooks (canary absent)")

    canary.unlink(missing_ok=True)
    hijack_main = fresh_hijack("ev")
    ev_info = _lanes.ensure_lane(_lanes.repo_config("hijack"), "publish")
    ev_lane = Path(ev_info["path"])
    rewrite_git(ev_lane, attacker_repo("touch %s #publish-evidence" % canary).resolve() / ".git")
    (ev_lane / ".gitattributes").write_text("*.txt filter=attack\n")
    (ev_lane / "pub.txt").write_text("pub\n")
    base_commit = git(hijack_main, "rev-parse", ev_info["branch"])
    pe_path = KIT / "ops/bin/publish-evidence"
    pe_ns = {"__name__": "publish_evidence", "__file__": str(pe_path)}
    exec(compile(pe_path.read_text(encoding="utf-8"), str(pe_path), "exec"), pe_ns)
    text, files, complete, note, _tree = pe_ns["candidate_diff"](
        ev_lane, {"kind": "git", "commit": base_commit},
        lane_git={"main": ev_info["main"], "lane": "publish"})
    expect(not canary.exists(), "publish-evidence with hijacked .git never runs attacker clean filter", note)
    expect(not complete and ".git" in note and "expected" in note,
           "publish-evidence refuses a rewritten .git pointer", note)

    canary.unlink(missing_ok=True)
    hijack_main = fresh_hijack("grant")
    grant_info = _lanes.ensure_lane(_lanes.repo_config("hijack"), "grant")
    grant_lane = Path(grant_info["path"])
    rewrite_git(grant_lane, attacker_repo("touch %s #grant-paths" % canary).resolve() / ".git")
    (grant_lane / "g.txt").write_text("g\n")
    grant_base = git(hijack_main, "rev-parse", grant_info["branch"])
    try:
        _lanes.workspace_changed_paths(grant_lane, grant_base,
                                       lane_ctx={"main": grant_info["main"], "lane": "grant"})
        expect(False, "grant changed_paths with hijacked .git is refused")
    except _lanes.LaneError as error:
        expect(".git" in str(error), "grant changed_paths refuses rewritten .git", str(error))
    expect(not canary.exists(), "grant changed_paths never runs attacker clean filter (canary absent)")
finally:
    shutil.rmtree(tmp, ignore_errors=True)

if problems:
    print("repo-lanes: %d failed" % len(problems))
    sys.exit(1)
print("repo-lanes: ok")
