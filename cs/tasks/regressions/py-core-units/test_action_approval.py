"""Host-side action review context, one-shot execution, and the claim-outcome time budget."""
import contextlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

CS = Path(__file__).resolve().parents[3]
OPS = CS / "ops/bin"
sys.path.insert(0, str(OPS))

import _action_approval as approval


class ActionApprovalTests(unittest.TestCase):
    def setUp(self):
        contexts = contextlib.ExitStack()
        self.addCleanup(contexts.close)
        fixture = contexts.enter_context(tempfile.TemporaryDirectory())
        self.home = Path(fixture).resolve()
        self.workspace = self.home / "workspace"
        self.run = self.home / "runs" / "run"
        (self.run / "stimulus").mkdir(parents=True)
        (self.run / "stimulus/goal.md").write_text("Verify the lane changes.")
        self.script = self.workspace / "cs/tasks/regressions/trace-e2e/check"
        self.script.parent.mkdir(parents=True)
        self.script.write_text("#!/bin/sh\nexit 0\n")
        self.request = {
            "argv": [str(self.script.relative_to(self.workspace))],
            "needs": ["network:loopback", "write_paths"],
            "write_paths": [str(self.run / "tmp")],
            "reason": "This check starts a loopback server and writes scratch files.",
        }
        self.client = contexts.enter_context(patch.object(approval, "director_client")).return_value
        # execute() runs the approved command with Popen in its own process group; other
        # subprocess use (git diff for the review context) stays real.
        real_popen = subprocess.Popen
        self.execute = Mock()
        self.execute.return_value.wait.return_value = 0

        def popen(argv, *args, **kwargs):
            if argv and argv[0] == "cs-sandbox":
                return self.execute(argv, *args, **kwargs)
            return real_popen(argv, *args, **kwargs)

        contexts.enter_context(patch.object(approval.subprocess, "Popen", side_effect=popen))
        contexts.enter_context(patch.object(Path, "home", return_value=self.home))
        self.stdout = contexts.enter_context(contextlib.redirect_stdout(io.StringIO()))

    def request_action(self):
        return approval.request_action(self.home, self.run, "coder", "", self.request,
                                       job="job", workspace=self.workspace,
                                       prefix=["cs-sandbox", "--policy", "policy", "--"])

    def test_routine_regression_sends_source_and_executes_with_only_the_grants(self):
        def director_call(method, path, *, body):
            if path.endswith("/claim"):
                return submitted.copy()
            if path.endswith("/outcome"):
                self.assertEqual(body["exit_code"], 0)
                return {}
            self.assertEqual(path, "jobs/job/action-approvals")
            context = body["context"]
            self.assertEqual(context["goal"], "Verify the lane changes.")
            self.assertEqual(context["job_tmp"], self.request["write_paths"][0])
            self.assertEqual(context["scripts"], [{
                "path": self.request["argv"][0], "contents": "#!/bin/sh\nexit 0\n",
            }])
            self.assertEqual(body["script_sha256"], approval.script_digest(context))
            submitted.update(body, id="first", status="approved", reviewer_model="reviewer",
                             verdict="go", review_reason="Local test infrastructure.")
            return submitted.copy()

        submitted = {}
        self.client.call.side_effect = director_call
        self.assertEqual(self.request_action(), 0)
        argv = self.execute.call_args.args[0]
        widening = json.loads(argv[argv.index("--action-widening") + 1])
        self.assertEqual(widening, dict(network=False, loopback=True, delete=False,
                                        write_paths=self.request["write_paths"]))
        self.assertIn('"reviewer_model": "reviewer"', self.stdout.getvalue())
        self.assertEqual(self.client.call.call_count, 3)

    def test_hard_line_and_reviewer_timeout_wait_for_owner(self):
        regression_argv = self.request["argv"]
        for model, reason in [(None, "Git push requires an owner decision."),
                              ("reviewer", "The action reviewer timed out.")]:
            with self.subTest(reason=reason):
                record = dict(id="first", status="needs_owner", verdict="no_go", review_reason=reason)
                if model:
                    record["reviewer_model"] = model
                self.client.call = Mock(return_value=record)
                self.execute.reset_mock()
                self.request["argv"] = regression_argv if model else ["git", "push"]
                self.assertEqual(self.request_action(), 0)
                self.client.call.assert_called_once()
                self.assertFalse(any("--action-widening" in call.args[0]
                                     for call in self.execute.call_args_list))
                self.assertIn(reason, self.stdout.getvalue())

    def test_script_changed_during_review_requires_a_new_request(self):
        def change_script(*args, **kwargs):
            self.script.write_text("#!/bin/sh\nexit 1\n")
            return dict(id="first", status="approved")

        self.client.call.side_effect = change_script
        with self.assertRaisesRegex(ValueError, "changed during review"):
            self.request_action()
        self.client.call.assert_called_once()


class ActionTimeout(unittest.TestCase):
    def test_timeout_posts_failed_outcome(self):
        action = dict(argv=["/usr/bin/true"], needs=["network"], reason="fixture", write_paths=[])
        claim = dict(action, requesting_model="fake", id="action", status="approved")
        client = Mock()
        client.call.side_effect = [claim, claim, {}]

        process = Mock()
        process.wait.side_effect = [subprocess.TimeoutExpired(["true"], approval.ACTION_TIMEOUT_S), 0]
        context = dict(workspace=str(CS), home="/tmp", job_tmp="/tmp", goal="", diff="",
                       **approval.command_context(action["argv"], CS.resolve()))

        with (patch.object(approval, "director_client", return_value=client),
              patch.object(approval, "lane_context", return_value=context),
              patch.object(approval.subprocess, "Popen", return_value=process),
              patch("_process_group.stop_group"),
              contextlib.redirect_stdout(io.StringIO())):
            tmp = tempfile.TemporaryDirectory()
            self.addCleanup(tmp.cleanup)
            run_dir = Path(tmp.name) / "run"
            run_dir.mkdir()
            result = approval.request_action(
                CS, run_dir, "fake", None, action,
                job="job", workspace=CS, prefix=["cs-sandbox", "--policy", "policy", "--"],
            )
        self.assertEqual(result, 1)
        client.call.assert_called_with(
            "POST", "jobs/job/action-approvals/action/outcome",
            body={"run_id": "run", "exit_code": 124},
        )

    def test_tool_budget_includes_poll_and_outcome(self):
        descriptor = json.loads((CS / "tools/bin/request_action.json").read_text())
        self.assertGreater(approval.ACTION_TIMEOUT_S, 0)
        self.assertGreater(descriptor.get("timeout_seconds", 60), 22 + 21 + approval.ACTION_TIMEOUT_S)
