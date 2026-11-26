"""A lane's regression definitions come from its committed base."""
import json
import os
import runpy
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE.parent / "_lib"), str(HERE.parents[2] / "ops/bin")]

from hermetic_env import hermetic_env
from scratch import scratch_dir


class CheckGraderTests(unittest.TestCase):
    def test_candidate_cannot_replace_or_skip_base_regressions(self):
        from _check_grader import frozen_grader
        from _estate_io import hardened_git_env
        from _machine_finish import record_check, Check

        with scratch_dir() as scratch:
            root = Path(scratch).resolve()
            ws = root / "candidate"
            ws.mkdir()

            def git(*args):
                return subprocess.check_output(
                    ["git", "-C", str(ws), *args], env=hardened_git_env({"HOME": str(root)}), text=True,
                ).strip()

            git("init", "-q")
            failing = ws / "cs/tasks/regressions/failing"
            host = failing.with_name("host")
            for test in (failing, host):
                test.mkdir(parents=True)
                (test / "check").write_text("#!/bin/sh\nexit 7\n")
            (host / "goal.md").write_text("---\nhost_only: nested sandbox\n---\n")
            (failing / "goal.md").write_text("Base regression\n")
            check = ws / "cs/scripts/check"
            check.parent.mkdir()
            check.write_text(
                "#!/bin/sh\ncd \"$(dirname \"$0\")/..\"\n"
                "test \"$(cat product)\" = candidate || exit 8\n"
                "sh tasks/regressions/host/check || exit 9\n"
                "if [ -f run-new ]; then sh tasks/regressions/new-thing/check; exit $?; fi\n"
                "sh tasks/regressions/failing/check\n"
            )
            (ws / "cs/product").write_text("base")
            (ws / "cs/tasks/regressions/_lib").mkdir()
            (ws / "cs/tasks/regressions/_lib/helper").write_text("base helper")
            git("add", ".")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                "commit", "-qm", "base")
            base = git("rev-parse", "HEAD")
            (failing / "check").write_text("#!/bin/sh\nexit 0\n")
            (failing / "goal.md").write_text("---\nhost_only: candidate edit\n---\n")

            check.write_text("#!/bin/sh\nexit 0\n")
            (ws / "cs/product").write_text("candidate")
            (ws / "cs/tasks/regressions/_lib/helper").write_text("candidate helper")
            new_check = ws / "cs/tasks/regressions/new-thing/check"
            new_check.parent.mkdir()
            new_check.write_text("#!/bin/sh\nexit 0\n")
            fixture = failing / "fixture.txt"
            fixture.write_text("candidate fixture")
            git("add", str(fixture))
            additions = ["cs/tasks/regressions/failing/fixture.txt", "cs/tasks/regressions/new-thing/check"]

            binary = ws / "target/release/cs-sandbox"
            binary.parent.mkdir(parents=True)
            binary.write_text("#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
            runtime = root / "installed"
            installed = runtime / "bin/cs-sandbox"
            installed.parent.mkdir(parents=True)
            installed.write_text("#!/bin/sh\nexit 0\n")
            installed.chmod(0o755)
            os.utime(installed, (1, 1))
            os.utime(binary, (2, 2))
            operation = ws / "cs/ops/bin/fixture.py"
            operation.parent.mkdir(parents=True)
            operation.write_text("from pathlib import Path\nCODE_ROOT = Path(__file__).resolve().parents[2]\n")

            with (patch("_check_grader.sandbox_runtime_code_root", return_value=runtime / "cs"),
                  frozen_grader(ws, base, root) as grader):
                self.assertEqual((grader.root / "target/release/cs-sandbox").resolve(), binary.resolve())
                self.assertEqual(grader.report("", 0)["grader_binaries"]["cs-sandbox"], str(binary.resolve()))
                loaded = runpy.run_path(str(grader.root / "cs/ops/bin/fixture.py"))
                self.assertEqual(loaded["CODE_ROOT"], (grader.root / "cs").resolve())
                self.assertEqual(grader.environment({"CS_CODE_ROOT": str(ws / "cs")})["CS_CODE_ROOT"],
                                 str(loaded["CODE_ROOT"]))
                regressions = grader.root / "cs/tasks/regressions"
                self.assertFalse((regressions / "new-thing").exists())
                self.assertEqual((regressions / "_lib/helper").read_text(), "base helper")
                self.assertFalse((regressions / "failing/fixture.txt").exists())
                done = subprocess.run(
                    ["/bin/sh", "cs/scripts/check"], cwd=grader.root, env=hermetic_env(),
                    text=True, capture_output=True,
                )
                self.assertEqual(done.returncode, 7, done.stderr)
                self.assertEqual(grader.report(done.stdout + done.stderr, done.returncode)["check_summary"], "fail")
                self.assertEqual(grader.host_only, {"host": "nested sandbox"})

                passed = subprocess.run(
                    ["/bin/sh", str(regressions / "host/check")],
                    cwd=grader.root, env=hermetic_env(), text=True, capture_output=True,
                )
                self.assertEqual(passed.returncode, 0)
                recorded = Check(passed.returncode, passed.stdout, passed.stderr,
                                 grader=grader.report(passed.stdout, passed.returncode))
                record_check(root, {"stimulus_sha": "fixture"}, 60, lambda: recorded, recover=True)
                report = json.loads((root / "check.json").read_text())
                self.assertEqual(report["verdict"], "pass")
                self.assertEqual(report["check_summary"], "pass with 1 host-only skips")
                self.assertEqual(report["new_grader_paths"], additions)
                self.assertEqual(report["grader_binaries"], {"cs-sandbox": str(binary.resolve())})
                self.assertEqual(report["host_only_skips"], [{
                    "regression": "host",
                    "outcome": "skipped",
                    "reason": "nested sandbox",
                    "message": "host: skipped: needs host (nested sandbox)",
                }])

            (ws / "cs/run-new").touch()
            run = root / "second-run"
            run.mkdir()
            binary.unlink()
            with (patch("_check_grader.sandbox_runtime_code_root", return_value=runtime / "cs"),
                  frozen_grader(ws, base, root) as grader):
                self.assertEqual((grader.root / "target/release/cs-sandbox").resolve(), installed)
                self.assertEqual(grader.report("", 0)["grader_binaries"], {"cs-sandbox": str(installed)})
                done = subprocess.run(
                    ["/bin/sh", "cs/scripts/check"], cwd=grader.root, env=hermetic_env(),
                    text=True, capture_output=True,
                )
                self.assertNotEqual(done.returncode, 0)
                recorded = Check(done.returncode, done.stdout, done.stderr,
                                 grader=grader.report(done.stdout + done.stderr, done.returncode))
                record_check(run, {"stimulus_sha": "fixture"}, 60, lambda: recorded, recover=True)
                report = json.loads((run / "check.json").read_text())
                self.assertEqual(report["verdict"], "fail")
                self.assertEqual(report["check_summary"], "pending land: new regression")
                self.assertEqual(report["new_grader_paths"], additions)
                mixed = "check: FAIL — new regression\n" + done.stderr + "\ncheck: FAIL — unrelated rule"
                self.assertEqual(grader.report(mixed, done.returncode)["check_summary"], "fail")
                missing = "cat: tasks/regressions/failing/fixture.txt: No such file or directory"
                self.assertEqual(grader.report(missing, 1)["check_summary"], "pending land: new regression")
                unrelated = missing.replace("fixture.txt", "fixture.txt.bak")
                self.assertEqual(grader.report(unrelated, 1)["check_summary"], "fail")
