"""Unit coverage for smoke selection and trial results."""
import io
import json
import sqlite3
import sys
import unittest
from contextlib import closing, redirect_stdout
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import Mock, patch

CS = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(CS / "scripts"))
import cs_smoke  # noqa: E402
import cs_init  # noqa: E402


class SmokeTest(unittest.TestCase):
    def test_held_trial_reports_immediately(self):
        cases = (
            ("waiting", "executor_unavailable", "Pi does not report the template", "login", True,
             "sign-in expired; run `pi` to sign in again"),
            ("waiting", "executor_unavailable", "Pi does not report the template", "quota", True,
             "provider limit reached: allowance exhausted"),
            ("waiting", "executor_unavailable", "Pi does not report the template", None, True,
             "Pi does not report the template"),
            ("held", "budget_exhausted", "trial budget is exhausted", "login", False,
             "held: trial budget is exhausted"),
        )
        for state, reason, detail, via, skipped, message in cases:
            with self.subTest(state=state, reason=reason, via=via), TemporaryDirectory() as tmp:
                root = Path(tmp)
                if via:
                    (root / "provider-limits.json").write_text(json.dumps({
                        "templates": {"pi": {"via": via, "message": "allowance exhausted"}}
                    }))
                with closing(sqlite3.connect(root / "director.sqlite")) as db, db:
                    db.execute("CREATE TABLE jobs(job_id, state, run_id, terminal_row)")
                    db.execute("CREATE TABLE job_budgets(job_id, reason, detail)")
                    db.execute("INSERT INTO jobs VALUES ('trial', ?, NULL, NULL)", (state,))
                    db.execute("INSERT INTO job_budgets VALUES ('trial', ?, ?)", (reason, detail))
                cs = Mock(KIT=CS, ROOT=root, CONFIG=root, state_root=lambda: root,
                          read_body=json.loads, load_script=Mock(return_value=cs_init))
                cs.node_release.return_value = Mock(DIRECTOR_RS_DATA=root, DIRECTOR_RS_LISTEN="unused")
                cs.node_release.return_value.load_record.return_value = {"node": "fixture"}
                init = cs_init.Init(cs, SimpleNamespace(director_listen="unused"))
                init.run = Mock(return_value=SimpleNamespace(stdout='{"job_id":"trial"}', returncode=0))
                init.cancel_job = Mock()
                with patch("cs_smoke.time.sleep", side_effect=AssertionError("held trial should finish before sleeping")) as sleep:
                    result = cs_smoke.measured("pi", lambda: cs_smoke.agent_check(init, {"name": "pi", "template": "pi"}))
                sleep.assert_not_called()
                self.assertEqual(result["ok"], skipped)
                self.assertEqual(result.get("skipped", False), skipped)
                self.assertEqual(result["reason"], message)
                init.cancel_job.assert_called_once_with("trial", "cs smoke trial held: " + reason)

    def test_unavailable_template_is_reported_without_probing(self):
        templates = json.loads((CS / "ops/bin/executor-templates.json").read_text())
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "local-credential").touch()
            init = Mock(data=root)
            module = SimpleNamespace(AGENTS={"gemini": "gemini-cli", "goose": "goose"}, Init=Mock(return_value=init))
            cs = SimpleNamespace(KIT=CS, CONFIG=root, load_script=Mock(return_value=module),
                                 read_json=lambda path, default: templates)
            output = io.StringIO()
            with redirect_stdout(output):
                self.assertEqual(cs_smoke.main(cs, SimpleNamespace(agents="gemini,goose", json=True)), 0)
            init.probe.assert_not_called()
            init.agent_path.assert_not_called()
            self.assertEqual(json.loads(output.getvalue())["checks"], [
                {"name": name, "ok": True, "skipped": True, "seconds": 0,
                 "reason": templates[name]["unavailable_reason"]}
                for name in ("gemini-cli", "goose")
            ])
