import fcntl
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

CS = Path(__file__).resolve().parents[3]
sys.path[:0] = [str(CS / "scripts"), str(CS / "ops/bin")]

import _action_approval
import _node_layout
import _node_intake


class NodeReview(unittest.TestCase):
    def test_drain_keeps_jobs_on_both_sides_of_torn_row(self):
        with tempfile.TemporaryDirectory() as root, patch.object(_node_layout, "ROOT", Path(root)):
            shelf = Path(root) / "state/dispatch.jsonl"
            shelf.parent.mkdir()
            shelf.write_text('{"id":"a","state":"pending"}\n{"id":\n{"id":"b","state":"pending"}\n')
            self.assertEqual([job["id"] for job in _node_layout.active_jobs()], ["a", "b"])

    def test_restart_holds_tick_intake_and_admission_locks(self):
        with tempfile.TemporaryDirectory() as root, patch.object(_node_layout, "ROOT", Path(root)):
            state = Path(root) / "state"
            with _node_layout.restart_window(True, {"no_wait": True}):
                for name in ("restart-intake.lock", "dispatch.lock"):
                    with (state / name).open() as contender:
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
            for name in ("restart-intake.lock", "dispatch.lock"):
                with (state / name).open() as contender:
                    fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_listener_offers_wait_for_restart(self):
        with tempfile.TemporaryDirectory() as root, patch.object(_node_layout, "ROOT", Path(root)):
            state = Path(root) / "state"
            with patch.object(_node_intake, "intake_blocked", return_value=None):
                with _node_layout.restart_window(True, {"no_wait": True}):
                    self.assertEqual(_node_intake.take(None, [{"job_id": "invalid!"}], state), ([], [], []))
                self.assertEqual(len(_node_intake.take(None, [{"job_id": "invalid!"}], state)[2]), 1)

    def test_empty_offers_do_not_create_state(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root) / "state"
            self.assertEqual(_node_intake.take(None, [], state), ([], [], []))
            self.assertFalse(state.exists())

    def test_doctor_repins_installed_cli(self):
        with tempfile.TemporaryDirectory() as root:
            home = Path(root)
            ops = home / ".contextstack/current/cs/ops/bin"
            ops.mkdir(parents=True)
            (home / ".contextstack/config").mkdir()
            for name in ("_cli_pins.py", "_estate_io.py", "_roots.py", "_env_policy.py"):
                shutil.copy2(CS / "ops/bin" / name, ops)
            (ops / "executor-templates.json").write_text('{"opencode":{"profile":"cli-profiles.json"}}')
            binary = home / "fake"
            binary.write_text('#!/bin/sh\n[ -z "${OPENAI_API_KEY:-}" ] || exit 1\necho fixture-v2\n')
            binary.chmod(0o755)
            (ops / "cli-profiles.json").write_text(json.dumps({"opencode": {"binary": str(binary)}}))
            run = subprocess.run([sys.executable, "-I", CS / "scripts/cs", "doctor", "--repin", "opencode"],
                                 env={"HOME": root, "PATH": "/usr/bin:/bin", "OPENAI_API_KEY": "fixture-only"},
                                 capture_output=True, text=True, timeout=10)
            self.assertEqual(run.returncode, 0, run.stderr)
            pins = json.loads((home / ".contextstack/config/cli-pins.json").read_text())
            self.assertEqual(pins["opencode"]["version"], "fixture-v2")

    def test_approved_action_timeout_kills_descendants(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root)
            record = {}
            request = {"needs": ["network"], "write_paths": [], "argv": [
                "/bin/sh", "-c", "trap '' TERM; (sleep 0.8; touch escaped) & wait"]}
            with patch.object(_action_approval, "ACTION_TIMEOUT_S", 0.25, create=True):
                code, output = _action_approval.execute(record, request, ["/bin/sh", "-c", 'shift 2; exec "$@"', "sandbox", "--"], {}, None, path)
            self.assertEqual(code, 124, output)
            self.assertEqual(record["outcome"]["status"], "failed")
            time.sleep(0.4)
            self.assertFalse((path / "escaped").exists())
