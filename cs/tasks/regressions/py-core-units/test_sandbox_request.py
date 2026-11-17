"""The unused Rust request transport fails closed before any executor can start."""
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ops/bin"))
from _estate_io import render_sandbox_request, sandbox_request


class SandboxRequestTests(unittest.TestCase):
    def test_explicit_inputs_and_transport(self):
        request = sandbox_request(workspace="/ws", tmpdir="/tmp/private", home="/owner",
                                  executable="/bin/tool", lookup_path=["/bin"], ssh_auth_sock=None)
        self.assertEqual(request["version"], 1)
        self.assertEqual(request["lookup_path"], [str(Path("/bin").resolve())])
        with patch("_estate_io.subprocess.run", return_value=subprocess.CompletedProcess(
                [], 0, b"(version 1)\nprofile", b"")) as run:
            self.assertEqual(render_sandbox_request("binary", "policy", request), b"(version 1)\nprofile")
            self.assertEqual(run.call_args.args[0][-1], "-")
            self.assertEqual(run.call_args.kwargs["timeout"], 10)

    def test_failures(self):
        for failure in (FileNotFoundError("missing"), subprocess.TimeoutExpired("binary", 1)):
            with self.subTest(failure=failure), patch("_estate_io.subprocess.run", side_effect=failure):
                with self.assertRaises(RuntimeError):
                    render_sandbox_request("binary", "policy", {})
        for code, stdout in ((101, b"partial"), (2, b""), (0, b"old binary")):
            with self.subTest(code=code), patch("_estate_io.subprocess.run", return_value=
                    subprocess.CompletedProcess([], code, stdout, b"refused")):
                with self.assertRaises(RuntimeError):
                    render_sandbox_request("binary", "policy", {})
