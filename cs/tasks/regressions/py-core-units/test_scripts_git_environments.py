"""Git isolation for scripts used outside the hook chain."""
import ast
import importlib.machinery
import importlib.util
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

CS = Path(__file__).resolve().parents[3]
SCRIPTS = CS / "scripts"
sys.path.insert(0, str(CS / "tasks/regressions/_lib"))
from hermetic_env import hermetic_env
from scratch import scratch_dir


def load(name):
    loader = importlib.machinery.SourceFileLoader("scripts_git_" + name, str(SCRIPTS / name))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    with patch.object(sys, "path", [str(SCRIPTS), *sys.path]):
        loader.exec_module(module)
    return module


class ScriptsGitEnvironmentTests(unittest.TestCase):
    def test_helper_matches_ops_body(self):
        def definition(path):
            tree = ast.parse(path.read_text())
            return next(ast.dump(node) for node in tree.body
                        if isinstance(node, ast.FunctionDef) and node.name == "hardened_git_env")
        self.assertEqual(definition(SCRIPTS / "_atomic_write.py"),
                         definition(CS / "ops/bin/_estate_io.py"))

    def hostile_env(self):
        return {**hermetic_env(), "GIT_DIR": "/hostile", "GIT_WORK_TREE": "/hostile-tree",
                "GIT_INDEX_FILE": "/hostile-index", "GIT_AUTHOR_NAME": "Hostile",
                "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.hooksPath",
                "GIT_CONFIG_VALUE_0": "/hostile-hooks", "GIT_FUTURE_OVERRIDE": "hostile",
                "GIT_CONFIG_GLOBAL": "/hostile-config", "GIT_PAGER": "hostile-pager"}

    def assert_clean_calls(self, calls):
        self.assertTrue(calls)
        for call in calls:
            self.assertEqual(call.args[0][0], "git")
            env = call.kwargs["env"]
            self.assertEqual({k: v for k, v in env.items() if k.startswith("GIT_")}, {
                "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
                "GIT_TERMINAL_PROMPT": "0", "GIT_PAGER": "cat",
            })
            self.assertEqual(env["LC_ALL"], "C")

    def test_report_git_calls_are_hardened(self):
        report = load("report")
        parent = self.hostile_env()
        with patch.dict(os.environ, parent, clear=True), patch.object(
                report.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "head\n")) as run:
            self.assertEqual(report.git("/fixture", "rev-parse", "HEAD"), ("head", 0))
            report.git("/fixture", "merge-base", "--is-ancestor", "head", "HEAD")
            self.assert_clean_calls(run.call_args_list)
            self.assertEqual(dict(os.environ), parent)

    def test_estate_identity_git_calls_are_hardened(self):
        identity = load("estate-identity.py")
        with scratch_dir(prefix="identity-git-") as raw:
            code_root = Path(raw)
            (code_root / "ledger").mkdir()
            (code_root / "ledger/estate-cutover.json").write_text(json.dumps({
                "schema": "estate-cutover-v1", "estate": "fixture", "historical_prefixes": {}}))
            (code_root / "ledger/rows.jsonl").write_text('{"estate":"fixture"}\n')
            parent = self.hostile_env()
            with patch.dict(os.environ, parent, clear=True), patch.object(identity, "estate_id", return_value="fixture"), patch.object(
                    identity.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, b"")) as run:
                self.assertTrue(identity.check(code_root)["ok"])
                self.assertEqual(len(run.call_args_list), 3)
                self.assert_clean_calls(run.call_args_list)
                self.assertEqual(dict(os.environ), parent)
