"""Native sandbox request and launcher transport."""
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ops/bin"))
from _estate_io import land_check_request, render_land_check_request, render_sandbox_request, sandbox_request


class SandboxRequestTests(unittest.TestCase):
    def test_linux_executor_delegates_to_native_sandbox(self):
        import _executor

        with tempfile.TemporaryDirectory() as directory, patch.object(sys, "platform", "linux"), \
                patch("_executor.require_executor_sandbox"), \
                patch("_estate_io.native_sandbox_request", return_value={"version": 1}), \
                patch("_estate_io.native_sandbox_binary", return_value=Path("/native/cs-sandbox")), \
                patch("_action_approval.reserved_loopback_ports", return_value=[8780, 9200, 53123]):
            root = Path(directory)
            command = _executor.sandbox_cmd("/bin/tool", ["arg"], root, root, root / "profile",
                                            check_scratch=root / "scratch", loopback_ports=(53124,))
        self.assertEqual(command[0], "/native/cs-sandbox")
        self.assertEqual(command[3:5], ["--profile", "lane-check-run"])
        request = json.loads(command[6])
        self.assertEqual(request["check_scratch"], str(root.resolve() / "scratch"))
        self.assertEqual(request["loopback_ports"], [53124])
        self.assertEqual(request["reserved_ports"], [8780, 9200, 53123])
        self.assertEqual(command[-3:], ["--", "/bin/tool", "arg"])

    def test_land_check_request_uses_host_reserved_port_discovery(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(
                os.environ, {"CS_RESERVED_LOOPBACK_PORTS": "53123", "DIRECTOR_LISTEN": "127.0.0.1:53124"},
                clear=True):
            root = Path(directory)
            (root / "install.json").write_text('{"director_rs_listen": ["127.0.0.1:53125"]}')
            request = land_check_request(
                root=root, home=root / "home", main_checkout=root / "main", worktree=root / "land",
                run_tmp=root / "run", cargo_target_dir=root / "target", tmpdir=root / "run/tmp",
                scratch=root / "run/scratch", darwin_user_temp="/private/var/folders/zz/test/T",
                cache_dirs=[root / "run/cache"], toolchain_roots=[root / "tools"],
                ssh_auth_sock=root / "agent.sock")
        self.assertEqual(request["reserved_ports"], [8780, 9200, 53123, 53124, 53125])
        self.assertEqual(request["cargo_target_dir"], str(root / "target"))
        self.assertEqual(request["cache_dirs"], [str(root / "run/cache")])
        with patch("_estate_io.subprocess.run", return_value=subprocess.CompletedProcess(
                [], 0, b"(version 1)\nprofile", b"")) as run:
            self.assertEqual(render_land_check_request("binary", request), b"(version 1)\nprofile")
            self.assertEqual(run.call_args.args[0], ["binary", "--profile", "land-check", "--request", "-"])
            self.assertEqual(json.loads(run.call_args.kwargs["input"]), request)

    def test_explicit_inputs_and_transport(self):
        request = sandbox_request(workspace="/ws", tmpdir="/tmp/private", home="/owner",
                                  executable="/bin/tool", lookup_path=["/bin"], ssh_auth_sock=None)
        self.assertEqual(request["version"], 1)
        # Preserve lookup aliases in the request so Rust can resolve them and bind their targets.
        self.assertEqual(request["lookup_path"], ["/bin"])
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
