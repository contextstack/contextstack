import unittest
from io import BytesIO
from pathlib import Path
from unittest.mock import Mock, patch

import _browser_action_jobs as actions


class BrowserActionJobsTests(unittest.TestCase):
    def setUp(self):
        self.job = {
            "job_id": "job", "approval_id": "approval",
            "origin": "https://example.test", "action": "submit", "target": "#order",
            "field_values_digest": "a" * 64,
            "execution": {"origin_set": "public", "page_url": "https://example.test/order"},
        }
        self.proof = {"claim_proof": "b" * 64}
        self.client = Mock()
        self.client.call.side_effect = [{"jobs": [self.job]}, self.proof, {}]
        self.path = "jobs/job/action-approvals/approval"

    @patch.object(Path, "open")
    @patch.object(actions, "configured_sockets", return_value=Path("/fixture/sockets"))
    @patch.object(actions, "broker_call")
    def test_proposal_provides_only_intent_and_forwards_attested_evidence(self, broker, _, opened):
        self.client.token = "fixture"
        self.client.call.side_effect = [{"ok": True}, {"id": "approval", "status": "needs_owner"}]
        evidence = {"screenshot_sha": "a" * 64, "field_values_digest": "b" * 64,
                    "values_summary": [{"name": "qty", "value": "100", "hidden": True}]}
        broker.return_value = {"ok": True, "capture": evidence, "capture_proof": "c" * 64}
        opened.return_value = BytesIO(b"image")
        fields = {"url": self.job["execution"]["page_url"], "origin_set": "public",
                  "target": "#order", "action": "submit"}
        with self.assertRaises(ValueError):
            actions.propose(self.client, "job", "run", dict(fields, field_values_digest="spoofed"))
        broker.assert_not_called()
        actions.propose(self.client, "job", "run", fields)
        preflight, submit = self.client.call.call_args_list
        self.assertEqual(preflight.args, ("POST", "jobs/job/action-approvals/capture", dict(fields, run_id="run")))
        self.assertEqual(submit.args, ("POST", "jobs/job/action-approvals", {
            "kind": "browser_action", "run_id": "run", "capture": evidence,
            "capture_proof": "c" * 64, "image": b"image".hex(),
        }))
        self.assertEqual(len(broker.call_args.args[1]["host_proof"]), 64)

    @patch.object(Path, "is_socket", return_value=True)
    @patch.object(Path, "is_dir", return_value=True)
    @patch.object(actions, "execute", return_value={"status": "submitted"})
    def test_claims_before_sending_to_host(self, execute, _is_dir, _is_socket):
        root = Path("/fixture/sockets")
        actions.poll(self.client, root)
        self.assertEqual(self.client.call.call_args_list[1].args, (
            "POST", self.path + "/claim",
            {"run_id": "job", "origin": "https://example.test", "action": "submit",
             "target": "#order", "field_values_digest": "a" * 64},
        ))
        execute.assert_called_once_with(root / "public.sock", "https://example.test/order", self.proof)
        self.client.call.assert_called_with("POST", self.path + "/outcome", {"run_id": "job", "status": "submitted"})

    @patch.object(Path, "is_socket", return_value=True)
    @patch.object(Path, "is_dir", return_value=True)
    @patch.object(actions, "execute", side_effect=TimeoutError)
    def test_lost_host_response_keeps_outcome_unknown(self, execute, _is_dir, _is_socket):
        actions.poll(self.client, Path("/fixture/sockets"))
        execute.assert_called_once()
        self.client.call.assert_called_with("POST", self.path + "/outcome",
                                           {"run_id": "job", "status": "submission_unknown"})

    @patch.object(actions, "broker_call")
    def test_preserves_safe_refusal_reason(self, broker):
        broker.return_value = {"ok": False, "error_kind": "action_refused",
                               "error": "form digest mismatch"}
        self.assertEqual(actions.execute(Path("fixture.sock"), "https://example.test", self.proof),
                         {"status": "refused", "reason": "form digest mismatch"})
