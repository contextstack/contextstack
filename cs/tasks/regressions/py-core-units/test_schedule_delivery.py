"""Delivery with a temporary owner grant."""
import json
import sys
import tempfile
import unittest
from io import BytesIO
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ops/bin"))
from _schedule_delivery import deliver, deliver_job, result_markdown


class ScheduleDelivery(unittest.TestCase):
    def test_scheduled_job_delivers_final_reply_and_run_cost(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = root / "run1"
            run.mkdir()
            (run / "messages.jsonl").write_text(
                '{"event":"turn","text":"Working"}\n'
                '{"event":"turn","text":"The scheduled task result"}\n'
            )
            (root / "delivery.json").write_text(json.dumps({
                "file_root": str(root), "schedules": {"daily": [{"file": "daily.md"}]},
            }))
            with patch.dict("os.environ", {"CS_CONFIG_ROOT": str(root)}):
                deliver_job({"id": "job1", "schedule_id": "daily", "run_id": "run1", "state": "done"},
                            {"verdict": "pass", "cost_usd": 0.12}, root)
            report = (root / "daily.md").read_text()
            self.assertIn("The scheduled task result", report)
            self.assertIn("Cost: $0.12 (actual)", report)

    def test_conversation_and_file_receive_the_same_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            config = base / "config"
            config.mkdir()
            credential = base / "state/director/local-credential"
            credential.parent.mkdir(parents=True)
            credential.write_text("fixture-token")
            (config / "install.json").write_text(json.dumps({
                "director": "http://127.0.0.1:1", "node": "fixture-node",
            }))
            (config / "delivery.json").write_text(json.dumps({"file_root": str(base)}))
            report = result_markdown("# Result", {"est_usd": 0.25, "est_basis": "rate"})
            self.assertIn("Cost: $0.25 (estimated)", report)
            response = MagicMock()
            response.__enter__.return_value = response
            response.status = 201
            response.fp.raw._sock = MagicMock()
            response.read1 = BytesIO(b'{}').read
            response.isclosed.return_value = True
            with patch("urllib.request.urlopen", return_value=response) as director:
                for _ in range(2):
                    deliver([{"conversation": "conv_test"}, {"file": "result.md"}], report, "slot", config,
                            schedule_id="daily")
            first, second = (call.args[0] for call in director.call_args_list)
            self.assertEqual(first.full_url, "http://127.0.0.1:1/api/v1/client/conversations/conv_test/notes")
            self.assertEqual(first.get_method(), "POST")
            self.assertEqual(first.get_header("Authorization"), "Bearer fixture-token")
            self.assertEqual(json.loads(first.data), {"message": report, "schedule_key": "fixture-node/daily"})
            self.assertEqual(first.get_header("Idempotency-key"), second.get_header("Idempotency-key"))
            self.assertEqual((base / "result.md").read_text(), report)

    def test_file_grant_and_cost(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            root = base / "delivery"
            root.mkdir()
            (root / "escape").symlink_to(base, target_is_directory=True)
            (base / "delivery.json").write_text(json.dumps({"file_root": str(root)}))
            report = result_markdown("# Result", {"cost_usd": 0.125})
            self.assertIn("Cost: $0.125 (actual)", report)
            deliver([{"file": "result.md"}], report, "slot", base)
            self.assertEqual((root / "result.md").read_text(), report)
            for path in ("../outside.md", "escape/outside.md"):
                with self.subTest(path=path), self.assertRaises(ValueError):
                    deliver([{"file": path}], report, "slot", base)
            self.assertFalse((base / "outside.md").exists())
