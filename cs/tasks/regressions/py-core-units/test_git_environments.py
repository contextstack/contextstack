"""Git callers discard inherited overrides without launching git."""
import os
import runpy
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
OPS = HERE.parents[2] / "ops/bin"
sys.path[:0] = [str(HERE.parent / "_lib"), str(OPS)]

from hermetic_env import hermetic_env
from scratch import scratch_dir
import _harvest_git


class GitEnvironmentTests(unittest.TestCase):
    def parent_env(self):
        return {**hermetic_env(), "GIT_DIR": "hostile-dir", "GIT_WORK_TREE": "hostile-tree",
                "GIT_INDEX_FILE": "hostile-index", "GIT_CONFIG_COUNT": "hostile-count",
                "GIT_CONFIG_GLOBAL": "hostile-config", "GIT_PAGER": "hostile-pager",
                "GIT_UNKNOWN_OVERRIDE": "hostile-unknown", "UNRELATED_SECRET": "secret"}

    def assert_clean(self, env):
        self.assertEqual({k: v for k, v in env.items() if k.startswith("GIT_")
                          and k != "GIT_INDEX_FILE"}, {
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_TERMINAL_PROMPT": "0", "GIT_PAGER": "cat",
        })
        self.assertNotEqual(env.get("GIT_INDEX_FILE"), "hostile-index")

    def test_publish_git_environment_and_private_index(self):
        publish = runpy.run_path(str(OPS / "publish-evidence"))
        with scratch_dir() as root, patch.dict(os.environ, self.parent_env(), clear=True):
            ws = Path(root) / "workspace"
            (ws / ".git").mkdir(parents=True)
            with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, "", "")) as run:
                publish["_workspace_git"](ws, "status")
                self.assert_clean(run.call_args.kwargs["env"])
                self.assertNotIn("GIT_INDEX_FILE", run.call_args.kwargs["env"])
                run.reset_mock()
                result = publish["candidate_diff"](ws, {"kind": "git", "commit": "base"})
            self.assertTrue(result[2])
            for call in run.call_args_list:
                self.assert_clean(call.kwargs["env"])
            indexes = [call.kwargs["env"]["GIT_INDEX_FILE"] for call in run.call_args_list
                       if "GIT_INDEX_FILE" in call.kwargs["env"]]
            self.assertEqual(len(indexes), 5)
            self.assertEqual(len(set(indexes)), 1)
            self.assertFalse(Path(indexes[0]).exists())

    def test_harvest_git_and_initialize_environments(self):
        with scratch_dir() as root, patch.dict(os.environ, self.parent_env(), clear=True):
            ws = Path(root) / "workspace"
            with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, "", "")) as run:
                _harvest_git.git(ws, "status")
                _harvest_git.initialize(ws, commit="base")
            self.assertEqual(run.call_count, 3)
            for call in run.call_args_list:
                env = call.kwargs["env"]
                self.assert_clean(env)
                self.assertNotIn("GIT_INDEX_FILE", env)
                self.assertNotIn("UNRELATED_SECRET", env)
                self.assertLessEqual(set(env), set(_harvest_git.ENV_ALLOW) | {
                    "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL", "GIT_TERMINAL_PROMPT", "GIT_PAGER"})
                self.assertEqual(env["HOME"], os.environ["HOME"])

    def test_owner_tools_bootstrap_identity_once_and_harden_git(self):
        # check-red-gate runs as a git hook: it must keep the environment git gives hooks (GIT_INDEX_FILE, author).
        for path in (OPS / "estate-commit",):
            with self.subTest(tool=path.name), patch.dict(os.environ, self.parent_env(), clear=True):
                tool = runpy.run_path(str(path))
                identity = "user.name\nOwner\0user.email\nowner@example.test\0"
                with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, identity, "")) as run, \
                     patch("subprocess.check_output", return_value=b"") as output:
                    tool["staged_paths"]()
                    tool["staged_paths"]()
                    self.assertEqual(run.call_count, 1)
                    bootstrap = run.call_args
                    self.assertIn("--global", bootstrap.args[0])
                    self.assertFalse(any(k.startswith("GIT_") for k in bootstrap.kwargs["env"]))
                    for call in output.call_args_list:
                        env = call.kwargs["env"]
                        for role in ("AUTHOR", "COMMITTER"):
                            self.assertEqual(env["GIT_" + role + "_NAME"], "Owner")
                            self.assertEqual(env["GIT_" + role + "_EMAIL"], "owner@example.test")
                        self.assertNotIn("GIT_DIR", env)
                        self.assertNotIn("GIT_INDEX_FILE", env)
                        self.assertEqual(env["GIT_CONFIG_GLOBAL"], "/dev/null")
                        self.assertEqual(env["GIT_CONFIG_NOSYSTEM"], "1")
