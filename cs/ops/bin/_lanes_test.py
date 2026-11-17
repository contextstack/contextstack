"""Focused lane CAS checks; no executor, release binaries, or sandbox required."""
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import _lanes as lanes


class LaneCasTest(unittest.TestCase):
    def test_rejects_non_oid_git_output_before_update(self):
        for answers in (("main",), ("a" * 40, "b" * 40, "not-an-oid")):
            with self.subTest(answers=answers), patch.object(lanes, "lane_out", side_effect=answers) as out, patch.object(
                lanes, "_lane_commit_idents", return_value=[]
            ):
                with self.assertRaises(lanes.LaneError):
                    lanes._commit_lane_index("unused", "safe", "unused", "message")
                self.assertFalse(any("update-ref" in call.args for call in out.call_args_list))

    def test_ref_races_preserve_main(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        env.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="test",
                   GIT_AUTHOR_EMAIL="test@example.com", GIT_COMMITTER_NAME="test",
                   GIT_COMMITTER_EMAIL="test@example.com")
        for race in ("none", "head", "branch", "cas"):
            with self.subTest(race=race), tempfile.TemporaryDirectory() as raw, patch.dict(
                os.environ, env, clear=True
            ):
                main = Path(raw) / "main"
                main.mkdir()
                def git(*args):
                    return subprocess.run(["git", "-C", str(main), "-c", "core.hooksPath=/dev/null", *args],
                                          env=env, capture_output=True, text=True, check=True).stdout.strip()
                git("init", "-q", "-b", "main")
                git("commit", "-q", "--allow-empty", "-m", "seed")
                before = git("rev-parse", "refs/heads/main")
                cfg = {**lanes.DEFAULTS, "name": "test", "path": main}
                info = lanes.ensure_lane(cfg, "safe")
                ws = Path(info["path"])
                (ws / "change").write_text("change\n")
                ref = lanes.git_common_dir(main) / "refs/heads/lane/safe"
                run = subprocess.run
                updates = []
                def racing_run(cmd, *args, **kwargs):
                    if "update-ref" in cmd:
                        updates.append(cmd)
                        self.assertEqual(cmd[-3], "refs/heads/lane/safe")
                        for oid in cmd[-2:]:
                            self.assertRegex(oid, r"^[0-9a-f]{40}$")
                        if race == "head":
                            (lanes.expected_lane_gitdir(main, "safe") / "HEAD").write_text("ref: refs/heads/main\n")
                        elif race == "branch":
                            ref.write_text("ref: refs/heads/main\n")
                        elif race == "cas":
                            ref.write_text(git("commit-tree", before + "^{tree}", "-p", before, "-m", "concurrent") + "\n")
                    return run(cmd, *args, **kwargs)
                with patch.object(lanes.subprocess, "run", side_effect=racing_run):
                    if race == "cas":
                        with self.assertRaises(lanes.LaneError):
                            lanes.commit_lane(ws, "change", main=main, lane="safe")
                    else:
                        lanes.commit_lane(ws, "change", main=main, lane="safe")
                self.assertEqual(len(updates), 1)
                self.assertEqual(git("rev-parse", "--verify", "refs/heads/main^{commit}"), before)


class TrackedSecretLaneTest(unittest.TestCase):
    def test_tracked_secret_names_require_explicit_allowlist(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "LAB_"))}
        env.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="test",
                   GIT_AUTHOR_EMAIL="test@example.com", GIT_COMMITTER_NAME="test",
                   GIT_COMMITTER_EMAIL="test@example.com")
        cases = [(name, False, False) for name in
                 (".env", "nested/.env.production", ".env_local", ".env-local", ".envrc.local",
                  "secrets.env", "nested/secrets.env.production", "nested/.env.production\nextra")]
        cases += [(".env.example", False, True), ("secrets.env.sample", False, True),
                  (".env", True, True)]
        for name, allowlisted, allowed in cases:
            with self.subTest(name=name, allowlisted=allowlisted), tempfile.TemporaryDirectory(
                prefix="lane-tracked-secret-"
            ) as raw, patch.dict(os.environ, env, clear=True):
                main = Path(raw) / "main"
                main.mkdir()
                def git(*args):
                    return subprocess.run(["git", "-C", str(main), "-c", "core.hooksPath=/dev/null", *args],
                                          env=env, capture_output=True, text=True, check=True).stdout.strip()
                git("init", "-q", "-b", "main")
                # Build decoy history without opening a secret-named file, even in the fixture.
                blob = subprocess.run(["git", "-C", str(main), "hash-object", "-w", "--stdin"],
                                      input="DECOY ONLY\n", env=env, capture_output=True,
                                      text=True, check=True).stdout.strip()
                git("update-index", "--add", "--cacheinfo", "100644", blob, name)
                git("commit", "-q", "-m", "decoy fixture")
                # A staged removal must not conceal a secret still present in HEAD.
                git("update-index", "--force-remove", "--", name)
                repos = Path(raw) / "repos.json"
                repos.write_text(json.dumps({"test": {"path": str(main),
                    "tracked_secret_allowlist": [name] if allowlisted else []}}))
                os.environ["CS_REPOS_FILE"] = str(repos)
                cfg = lanes.repo_config("test")
                if allowed:
                    info = lanes.ensure_lane(cfg, "safe")
                    self.assertTrue(Path(info["path"]).is_dir())
                else:
                    with self.assertRaises(lanes.LaneError) as refused:
                        lanes.ensure_lane(cfg, "safe")
                    self.assertIn("filter-repo", str(refused.exception))
                    self.assertIn("BFG", str(refused.exception))
                    self.assertIn(repr(name), str(refused.exception))
                    self.assertFalse((Path(raw) / "safe").exists())
                    self.assertEqual(git("branch", "--list", "lane/safe"), "")


if __name__ == "__main__":
    unittest.main()
