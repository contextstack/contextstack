"""Main-side land Git must ignore host drivers and lane attributes (#44)."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys


def run(kit, expect, tmp):
    root = tmp / "land-git"
    root.mkdir()
    home = root / "home"
    home.mkdir()
    config = home / ".gitconfig"
    config.write_text("")
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "CS_"))}
    env.update(HOME=str(home), GIT_CONFIG_GLOBAL=str(config), GIT_CONFIG_NOSYSTEM="1",
               GIT_TERMINAL_PROMPT="0", CS_REPOS_FILE=str(root / "repos.json"))

    def git(repo, *args, input=None):
        done = subprocess.run(["git", "-C", str(repo), *args], input=input,
                              capture_output=True, text=True, env=env)
        if done.returncode:
            raise AssertionError("git %s: %s" % (args, done.stderr))
        return done.stdout.strip()

    def land(lane):
        done = subprocess.run([sys.executable, "-I", str(kit / "scripts/cs"), "land", "test", lane,
                               "--json", "--allow-agent-config"], capture_output=True, text=True, env=env, timeout=60)
        try:
            result = json.loads(done.stdout)
        except ValueError:
            result = {"stdout": done.stdout, "stderr": done.stderr}
        return done.returncode, result

    def commit_raw(repo, files, message):
        # No attributes or configured filters may transform the attack fixture itself.
        for name, content in files.items():
            path = repo / name
            path.parent.mkdir(exist_ok=True, parents=True)
            path.write_text(content)
            oid = git(repo, "hash-object", "-w", "--stdin", input=content)
            git(repo, "update-index", "--add", "--cacheinfo", "100644," + oid + "," + name)
        git(repo, "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", message)

    for mode in ("squash", "merge"):
        case = root / mode
        case.mkdir()
        main = case / "main"
        main.mkdir()
        git(main, "init", "-q", "-b", "main")
        git(main, "config", "--global", "user.name", "Owner Identity")
        git(main, "config", "--global", "user.email", "owner@example.test")
        # The owner's global ignore file still applies: a file it hides is not "uncommitted changes".
        excludes = case / "owner-gitignore"
        excludes.write_text("*.log\n")
        git(main, "config", "--global", "core.excludesfile", str(excludes))
        (main / "owner-local.log").write_text("ignored by the owner's global excludes\n")
        # Configured hooks remain gate authority even though global hooks are ignored.
        hooks = case / "owner-hooks"
        hooks.mkdir()
        hook_marker = case / "hook-ran"
        hook = hooks / "pre-commit"
        hook.write_text("#!/bin/sh\nprintf hook >> " + shlex.quote(str(hook_marker)) +
                        "\ngit diff --cached > /dev/null\n")
        hook.chmod(0o755)
        commit_raw(main, {"payload.txt": "first\nmiddle\nlast\n"}, "seed")
        expected_author = git(main, "log", "-1", "--format=%an <%ae>")
        (root / "repos.json").write_text(json.dumps({"test": {"path": str(main), "land": mode,
                                                           "hooks_path": str(hooks),
                                                           "gate": "git diff --cached > /dev/null"}}))
        lane = case / "attributes"
        git(main, "worktree", "add", "-q", "-b", "lane/attributes", str(lane))
        attrs = "* filter=evil diff=evil merge=evil ident text eol=crlf\nencoded.txt working-tree-encoding=UTF-16LE\n"
        payload = "first\nmiddle\nlast\n$Id$\n"
        commit_raw(lane, {".gitattributes": attrs, "payload.txt": payload, "encoded.txt": "raw lane bytes\n"}, "lane attributes")
        markers = {kind: case / (kind + "-ran") for kind in ("filter", "merge", "diff")}
        filter_cmd = "touch " + shlex.quote(str(markers["filter"])) + "; cat"
        git(main, "config", "--global", "filter.evil.clean", filter_cmd)
        git(main, "config", "--global", "filter.evil.smudge", filter_cmd)
        git(main, "config", "--global", "merge.evil.driver",
            "touch " + shlex.quote(str(markers["merge"])) + "; exit 1")
        git(main, "config", "--global", "diff.evil.textconv",
            "touch " + shlex.quote(str(markers["diff"])) + "; cat")
        code, result = land("attributes")
        expect(code == 0, mode + " land with malicious attributes succeeds", result)
        expect(not markers["filter"].exists(), mode + " land never invokes global filter")
        expect(not markers["diff"].exists(), mode + " hook diff never invokes global textconv")
        expect(hook_marker.exists(), mode + " land still runs declared gate hook", result)
        expect(git(main, "log", "-1", "--format=%an <%ae>") == expected_author,
               mode + " landed author preserves owner's global identity")
        expect((main / "payload.txt").read_bytes() == payload.encode(),
               mode + " landed worktree bytes equal lane bytes despite ident/eol/filter")
        expect(git(main, "show", "HEAD:payload.txt") == payload.rstrip("\n"),
               mode + " landed committed bytes equal lane bytes")
        expect((main / "encoded.txt").exists() and (main / "encoded.txt").read_bytes() == b"raw lane bytes\n",
               mode + " land ignores working-tree-encoding")
        if code:
            continue
        # Attributes now originate in a previously landed lane. Divergence forces the
        # three-way merge to consult a driver, rather than taking the fast-forward path.
        lane = case / "divergent"
        git(main, "worktree", "add", "-q", "-b", "lane/divergent", str(lane))
        commit_raw(main, {"payload.txt": payload.replace("first", "owner")}, "owner edit")
        lane_payload = payload.replace("last", "lane")
        commit_raw(lane, {"payload.txt": lane_payload}, "lane edit")
        for marker in markers.values():
            marker.unlink(missing_ok=True)
        # Repo-local programs are trusted config too, but lane attributes must not
        # be able to activate them: this isolates attribute suppression from the
        # removal of global config.
        for key, command in (("filter.evil.clean", filter_cmd), ("filter.evil.smudge", filter_cmd),
                             ("merge.evil.driver", "touch " + shlex.quote(str(markers["merge"])) + "; exit 1"),
                             ("diff.evil.textconv", "touch " + shlex.quote(str(markers["diff"])) + "; cat")):
            git(main, "config", "--local", key, command)
        code, result = land("divergent")
        expect(code == 0, mode + " land merges divergent text without custom merge driver", result)
        expect(not any(marker.exists() for marker in markers.values()),
               mode + " divergent land invokes no global or local driver")
        if code:
            continue
        before = git(main, "rev-parse", "HEAD")
        before_bytes = (main / "payload.txt").read_bytes()
        lane = case / "red"
        git(main, "worktree", "add", "-q", "-b", "lane/red", str(lane))
        commit_raw(lane, {"payload.txt": "red lane bytes\n"}, "red lane")
        hook.write_text("#!/bin/sh\necho 'FAIL owner gate'\nexit 1\n")
        for marker in markers.values():
            marker.unlink(missing_ok=True)
        code, result = land("red")
        expect(code == 1 and result.get("status") == "red", mode + " declared hook can reject land", result)
        expect(git(main, "rev-parse", "HEAD") == before and (main / "payload.txt").read_bytes() == before_bytes,
               mode + " rollback preserves HEAD and exact worktree bytes")
        expect(not any(marker.exists() for marker in markers.values()), mode + " rollback invokes no driver")
