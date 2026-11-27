"""Hosted landing node tests using a fixture repo and a stub land command."""
import json
import unittest
from unittest.mock import Mock, patch

import test_lanes
import _lead_land as landing


class LandRequestTests(unittest.TestCase):
    # Reuse the fixture checkout setup without collecting LanesTests a second time.
    setUp = test_lanes.LanesTests.setUp
    g = test_lanes.LanesTests.g
    configure = test_lanes.LanesTests.configure
    commit = test_lanes.LanesTests.commit
    lane = test_lanes.LanesTests.lane

    def test_inspection_uses_pinned_git_paths(self):
        ws = self.lane()
        info = landing.inspect("repo", "safe")
        self.assertEqual(info, {"commit": self.g("rev-parse", "HEAD", cwd=ws),
                                "base": self.g("rev-parse", "HEAD"),
                                "protected": False, "tracing": False})
        for path, key in (("tracing/probe", "tracing"), ("cs/agents/coder.md", "protected")):
            target = ws / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("fixture")
            self.commit(ws, path)
            self.assertTrue(landing.inspect("repo", "safe")[key])

    def test_pin_mismatch_refuses_before_running_a_gate(self):
        ws = self.lane()
        pin = landing.inspect("repo", "safe")
        (ws / "file").write_text("new candidate")
        self.commit(ws, "candidate changed")
        result = landing.land(test_lanes.HERE.parents[2], dict(pin, repo="repo", lane="safe"))
        self.assertEqual(result["status"], "refused")
        self.assertIn("commit or main base changed", result["reason"])
        self.assertEqual(self.g("rev-parse", "HEAD"), pin["base"])

    def test_landing_keeps_branch_advanced_during_cleanup(self):
        self.lane()
        pin = landing.inspect("repo", "safe")
        tip = self.g("commit-tree", pin["commit"] + "^{tree}", "-p", pin["commit"], "-m", "new work")
        git = landing.lanes.git

        def advance_after_remove(main, *args, **kwargs):
            result = git(main, *args, **kwargs)
            if args[:2] == ("worktree", "remove") and result.returncode == 0:
                self.g("update-ref", "refs/heads/lane/safe", tip, pin["commit"])
            return result

        with patch.object(landing.lanes, "git", side_effect=advance_after_remove):
            code, result = landing.lanes.land("repo", "safe", expected=(pin["commit"], pin["base"]))
        self.assertEqual(code, 0, result)
        self.assertIsNone(result["removed"]["branch"])
        self.assertEqual(self.g("rev-parse", "refs/heads/lane/safe"), tip)

    def test_node_claims_before_two_attempts_and_reports_both(self):
        self.lane()
        inspected = landing.inspect("repo", "safe")
        state = self.root
        config = self.root / "config"
        (config / "policy").mkdir(parents=True)
        (config / "policy/land-retry.json").write_text(json.dumps({"known_flaky": ["fixture flake"]}))
        pending = {"id": "act_fixture", "status": "pending", "repo": "repo", "lane": "safe",
                   "target_run_id": "fixture-run"}
        events = []

        def rpc(method, path, body=None):
            events.append((method, path, body))
            if path == "land-requests":
                return {"land_requests": [pending]}
            if path.endswith("/inspect"):
                self.assertEqual(body, inspected)
                return dict(pending, status="approved", **inspected)
            if path.endswith("/claim"):
                return dict(pending, status="approved", claimed_at="now", **inspected)
            if path.endswith("/outcome"):
                return {}

        client = Mock()
        client.call.side_effect = rpc
        miss = {"status": "red", "stage": "gate", "main_unchanged": True,
                "failed": [landing.FAILURE + "fixture flake"]}
        success = {"status": "landed", "commit": "f" * 40}

        def fake_land(_code, row):
            self.assertEqual(events[-1][1], "land-requests/act_fixture/claim")
            self.assertEqual(row["commit"], inspected["commit"])
            return next(results)

        results = iter((miss, success))
        with patch.object(landing, "land", side_effect=fake_land) as run, patch.object(
                landing, "post_land_record", return_value=("recorded", "test-land")) as record:
            landing.poll(client, state, self.root, config)
        self.assertEqual(run.call_count, 2)
        self.assertEqual([call.args[3] for call in record.call_args_list], ["act_fixture:1", "act_fixture:2"])
        self.assertTrue(all(call.args[1] == "fixture-run" for call in record.call_args_list))
        self.assertEqual([a["state"] for a in events[-1][2]["attempts"]], ["failed", "landed"])
        self.assertEqual(list((state / "node/land-requests").glob("*.json")), [])

    def test_retry_requires_known_flake_or_isolated_timeout_pass(self):
        red = {"status": "red", "stage": "gate", "main_unchanged": True,
               "failed": [landing.FAILURE + "fixture"]}
        standalone = Mock(return_value=True)
        self.assertTrue(landing.retry_rule(red, ["fixture"], standalone))
        self.assertFalse(landing.retry_rule(red, [], standalone))
        standalone.assert_not_called()
        unrelated = dict(red, output_tail="other check timed out\n" + landing.FAILURE + "fixture\n"
                         "assertion failed\n" + landing.FAILURE_OUTPUT_END + "fixture.log")
        self.assertFalse(landing.retry_rule(unrelated, [], standalone))
        standalone.assert_not_called()
        timeout = dict(red, output_tail=landing.FAILURE + "fixture\nfixture timed out\n"
                       + landing.FAILURE_OUTPUT_END + "fixture.log")
        self.assertTrue(landing.retry_rule(timeout, [], standalone))
        standalone.assert_called_once_with("fixture")
        self.assertFalse(landing.retry_rule(timeout, [], lambda _: False))
        self.assertFalse(landing.retry_rule(dict(timeout, main_unchanged=False), ["fixture"], standalone))
        self.assertFalse(landing.retry_rule(dict(timeout, failed=[*red["failed"], landing.FAILURE + "other"]),
                                           ["fixture"], standalone))

    def test_busy_preflight_releases_claim_without_recording_a_land_attempt(self):
        row = dict(id="act_fixture", revision=12)
        client = Mock()
        receipt = self.root / "busy.json"
        with patch.object(landing, "land", return_value={"status": "refused", "busy": True}), patch.object(
                landing, "post_land_record") as record:
            landing.execute(client, self.root, row, receipt, {})
        record.assert_not_called()
        client.call.assert_called_once_with("POST", "land-requests/act_fixture/outcome",
                                            {"status": "busy", "claim_revision": 12})
        self.assertFalse(receipt.exists())


if __name__ == "__main__":
    unittest.main()
