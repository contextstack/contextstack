"""Git callers discard inherited overrides without launching git."""
import os
import runpy
import socket
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
from _machine_finish import check_environment


class GitEnvironmentTests(unittest.TestCase):
    def parent_env(self):
        return {**hermetic_env(), "GIT_DIR": "hostile-dir", "GIT_WORK_TREE": "hostile-tree",
                "GIT_INDEX_FILE": "hostile-index", "GIT_CONFIG_COUNT": "hostile-count",
                "GIT_CONFIG_GLOBAL": "hostile-config", "GIT_PAGER": "hostile-pager",
                "GIT_UNKNOWN_OVERRIDE": "hostile-unknown", "UNRELATED_SECRET": "secret"}

    def assert_clean(self, env):
        self.assertEqual({k: v for k, v in env.items() if k.startswith("GIT_")
                          and k not in {"GIT_INDEX_FILE", "GIT_OPTIONAL_LOCKS", "GIT_ATTR_NOSYSTEM", "GIT_CEILING_DIRECTORIES"}}, {
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_TERMINAL_PROMPT": "0", "GIT_PAGER": "cat",
        })
        self.assertNotEqual(env.get("GIT_INDEX_FILE"), "hostile-index")

    def test_check_uses_short_private_scratch_and_explicit_git_identity(self):
        with scratch_dir() as root, patch.dict(os.environ, {
            **self.parent_env(), "GIT_AUTHOR_NAME": "Fixture Author",
        }, clear=True):
            scratch = Path(root) / ("long-component-" * 8)
            scratch.mkdir()
            inherited = {"TMPDIR": str(scratch), "GIT_CONFIG_GLOBAL": "hostile-config"}
            with check_environment(inherited, scratch) as env:
                tmp = Path(env["TMPDIR"])
                self.assertLess(len(os.fsencode(tmp)), 32)
                self.assertEqual(tmp.resolve().parent, scratch.resolve())
                self.assertEqual(env["TMP"], str(tmp))
                self.assertEqual(env["TEMP"], str(tmp))
                self.assertEqual(env["GIT_CONFIG_GLOBAL"], os.devnull)
                self.assertEqual(env["GIT_CONFIG_NOSYSTEM"], "1")
                self.assertEqual(env["GIT_AUTHOR_NAME"], "Fixture Author")
                self.assertNotIn("GIT_COMMITTER_NAME", env)
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                    listener.bind(str(tmp / "check.sock"))
                self.assertTrue((tmp.resolve() / "check.sock").is_socket())
            self.assertFalse(tmp.is_symlink())
            self.assertEqual(list(scratch.iterdir()), [])
            self.assertEqual(inherited["GIT_CONFIG_GLOBAL"], "hostile-config")

    def test_native_check_scratch_remains_short_when_resolved(self):
        with patch.dict(os.environ, hermetic_env(), clear=True), check_environment({}, "/tmp") as env:
            tmp = Path(env["TMPDIR"])
            self.assertEqual(tmp.resolve().parent, Path("/tmp").resolve())
            self.assertLess(len(os.fsencode(tmp.resolve())), 32)
            self.assertFalse(tmp.is_symlink())

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
                       if "GIT_INDEX_FILE" in call.kwargs["env"] and "config" not in call.args[0]]
            self.assertEqual(len(indexes), 5)
            self.assertEqual(len(set(indexes)), 1)
            self.assertFalse(Path(indexes[0]).exists())

    def test_harvest_git_and_initialize_environments(self):
        with scratch_dir() as root, patch.dict(os.environ, self.parent_env(), clear=True):
            ws = Path(root) / "workspace"
            with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, "", "")) as run:
                _harvest_git.git(ws, "status")
                _harvest_git.initialize(ws, commit="base")
            self.assertEqual(run.call_count, 5)
            for call in run.call_args_list:
                env = call.kwargs["env"]
                self.assert_clean(env)
                self.assertNotIn("GIT_INDEX_FILE", env)
                self.assertNotIn("UNRELATED_SECRET", env)
                self.assertLessEqual(set(env), set(_harvest_git.ENV_ALLOW) | {
                    "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL", "GIT_TERMINAL_PROMPT", "GIT_PAGER",
                    "GIT_OPTIONAL_LOCKS", "GIT_ATTR_NOSYSTEM", "GIT_CEILING_DIRECTORIES"})
                self.assertEqual(env["HOME"], os.environ["HOME"])
