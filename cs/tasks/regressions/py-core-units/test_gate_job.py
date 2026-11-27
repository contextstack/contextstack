import sys
import unittest
from pathlib import Path
from subprocess import CompletedProcess, TimeoutExpired
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ops" / "bin"))

from _gate_job import run_steps, select_steps, step


class GateJobTests(unittest.TestCase):
    def test_contextstack_director_diff_selects_contract(self):
        steps = select_steps("contextstack", {}, ["director/src/main.rs"], Path.cwd())
        self.assertIn(
            ["tests/director-spec/run", "--side", "both"],
            [step["command"] for step in steps],
        )

    def test_contextstack_selects_touched_regressions(self):
        root = Path(__file__).resolve().parents[4]
        steps = select_steps("contextstack", {}, [
            "cs/tasks/regressions/py-core-units/test_gate_job.py",
            "cs/tasks/regressions/py-core-units/check",
            "gateway/src/main.rs",
        ], root)
        self.assertEqual([item["name"] for item in steps],
                         ["rustfmt", "clippy", "rust tests", "size limits", "py-core-units"])

    def test_result_and_host_only_skip(self):
        grader = SimpleNamespace(host_only={"probe": "nested Seatbelt"}, skips=lambda _: [])
        steps = [step("probe", ["./probe"], regression="probe"), step("fmt", ["fake-fmt"])]

        def run(item, timeout):
            self.assertEqual(item["name"], "fmt")
            self.assertGreater(timeout, 0)
            return CompletedProcess(item["command"], 0, "formatted", "")

        result = run_steps(steps, run, 10, grader)
        self.assertEqual(result["verdict"], "pass")
        self.assertFalse(result["complete"])
        self.assertTrue(result["uncovered"])
        self.assertEqual(result["steps"][0], {
            "name": "probe", "command": ["./probe"], "exit": None, "seconds": 0,
            "tail": "nested Seatbelt", "skipped": "host_only",
        })
        self.assertEqual(set(result["steps"][1]), {"name", "command", "exit", "seconds", "tail"})
        self.assertEqual(run_steps(steps[:1], run, 10, grader)["verdict"], "fail")

    def test_nonzero_and_timeout_fail(self):
        steps = select_steps("fixture", {"gate": "./check"}, [], Path.cwd())
        result = run_steps(steps, lambda *_: CompletedProcess([], 7, "", "bad"), 10)
        self.assertEqual(result["verdict"], "fail")
        self.assertEqual(result["steps"][0]["exit"], 7)
        self.assertEqual(result["steps"][0]["tail"], "bad")

        def timeout(*_):
            raise TimeoutExpired("./check", 1, output=b"partial")

        result = run_steps(steps, timeout, 1)
        self.assertEqual(result["verdict"], "fail")
        self.assertEqual(result["steps"][0]["exit"], 124)
        self.assertIn("partial", result["steps"][0]["tail"])

    def test_nested_probe_skip_is_not_a_pass(self):
        steps = [step("probe", ["./probe"], regression="probe")]
        result = run_steps(steps, lambda *_: CompletedProcess(
            [], 0, "SKIP: nested Seatbelt is unavailable", ""), 10)
        self.assertEqual(result["verdict"], "fail")
        self.assertEqual(result["steps"][0]["skipped"], "host_only")
        self.assertIsNone(result["steps"][0]["exit"])
