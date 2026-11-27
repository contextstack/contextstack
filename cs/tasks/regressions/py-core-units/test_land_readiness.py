import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ops/bin"))
import _land_check
from _land_host_probes import TEMPLATE


class LandReadinessTest(unittest.TestCase):
    def test_doctor_reports_modes_owner_controls_and_backend_readiness(self):
        loader = importlib.machinery.SourceFileLoader("cs_cli", str(_land_check.KIT / "scripts/cs"))
        cli = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
        loader.exec_module(cli)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo = root / "repo"
            (repo / ".git").mkdir(parents=True)
            config = root / "config"
            config.mkdir()
            repos = config / "repos.json"
            repos.write_text(json.dumps({"default": str(repo),
                                        "explicit": {"path": str(repo), "land_check": "sandbox"},
                                        "legacy": {"path": str(repo), "land_check": "host"}}))
            binary = root / "current/bin/cs-sandbox"
            binary.parent.mkdir(parents=True)
            self.script(binary, "exit 0")
            seatbelt = root / "sandbox-exec"
            self.script(seatbelt, 'test "$1" = -f && test "$2" = "%s" && test "$3" = /usr/bin/true'
                        % _land_check.SEATBELT_PROBE)
            bwrap = root / "bwrap"
            self.script(bwrap, 'test "$1" = --unshare-user && test "$2" = --ro-bind')
            with patch.dict(os.environ, {"CS_REPOS_FILE": str(repos), "CS_STATE_ROOT": str(root)}), \
                    patch.object(cli, "ROOT", root), patch.object(_land_check, "SEATBELT", str(seatbelt)), \
                    patch.object(_land_check, "LINUX_BWRAP_PATH", str(root)):
                mac = self.findings(cli, "macos")
                self.assertEqual(mac["sandboxed land check"]["detail"], "Seatbelt ready")
                self.assertFalse(mac["land check owner probe list"]["ok"])
                self.assertIn("invalid or missing", mac["land check owner probe list"]["detail"])
                self.assertIn('"land_check": "host"', mac["land check owner kill switch"]["detail"])
                self.assertEqual([mac["land check repo " + name]["detail"]
                                  for name in ("default", "explicit", "legacy")], ["sandbox", "sandbox", "host"])
                owner_list = config / "land-host-probes.json"
                owner_list.touch()
                linux = self.findings(cli, "linux")
                self.assertIn("bubblewrap (bwrap), user namespaces and seccomp ready",
                              linux["sandboxed land check"]["detail"])
                self.assertFalse(linux["land check owner probe list"]["ok"])
                policy = json.loads(TEMPLATE.read_text())
                del policy["probes"]["land-check-sandbox"]
                owner_list.write_text(json.dumps(policy))
                drift = self.findings(cli, "linux")["land check owner probe list"]
                self.assertFalse(drift["ok"])
                self.assertIn("missing or changed probes: land-check-sandbox", drift["detail"])
                owner_list.write_bytes(TEMPLATE.read_bytes())
                self.assertTrue(self.findings(cli, "linux")["land check owner probe list"]["ok"])
                self.script(bwrap, 'echo "userns disabled" >&2; exit 1')
                unavailable = self.findings(cli, "linux")["sandboxed land check"]
                self.assertFalse(unavailable["ok"])
                self.assertIn("userns disabled", unavailable["detail"])
                self.assertIn("cs init", unavailable["fix"])

    @staticmethod
    def script(path, body):
        path.write_text("#!/bin/sh\n" + body + "\n")
        path.chmod(0o755)

    @staticmethod
    def findings(cli, platform):
        with patch.object(cli, "PLATFORM", platform):
            return {row["check"]: row for row in cli.land_check_findings()}
