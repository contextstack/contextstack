"""Unit coverage for smoke selection."""
import io
import json
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import Mock

CS = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(CS / "scripts"))
import cs_smoke  # noqa: E402


class SmokeTest(unittest.TestCase):
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
