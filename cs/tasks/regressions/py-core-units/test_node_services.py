"""Unit coverage for the service backends and generated definitions."""
import os
import subprocess
import sys
import unittest
from tempfile import TemporaryDirectory
from unittest.mock import patch
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "scripts"))
import _node_services as services  # noqa: E402


class SystemdServiceTest(unittest.TestCase):
    def test_restart_uses_user_manager(self):
        with patch.object(services, "systemctl") as ctl:
            ctl.return_value.returncode = 0
            backend = services.service_backend("linux")
            self.assertEqual(backend.control("restart", services.GATEWAY_LABEL, None), "restarted")
            ctl.assert_called_once_with("restart", services.GATEWAY_LABEL + ".service")

    def test_failed_unit_is_down_with_its_exit_code(self):
        backend = services.service_backend("linux")
        result = subprocess.CompletedProcess([], 0, "LoadState=loaded\nActiveState=failed\nMainPID=0\nExecMainStatus=3\n")
        with patch.object(services, "systemctl", return_value=result):
            view = backend.status(services.GATEWAY_LABEL, {"reason": ""}, {}, None)
        self.assertEqual(view, {"loaded": True, "state": "failed", "pid": None, "last_exit": "3",
                                "up": False, "reason": "failed, last exit 3"})

    def test_reads_generated_unit_metadata(self):
        backend = services.service_backend("linux")
        service = services.Service("fixture", ["/tmp/run%h", "", 'escaped "\\word'],
                                   {"TEST": "with spaces\tand 100%"}, "/tmp/100%ready", "fixture")
        with TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.service"
            path.write_bytes(backend.render(service))
            self.assertEqual(backend.read(path), {"ProgramArguments": service.args, "EnvironmentVariables": service.env,
                                                  "WorkingDirectory": service.cwd})


class LaunchdServiceTest(unittest.TestCase):
    def test_start_and_stop_keep_bootstrap_and_bootout(self):
        backend = services.service_backend("macos")
        label = services.GATEWAY_LABEL
        domain = "gui/%d" % os.getuid()
        target = domain + "/" + label
        path = backend.path(label)
        ok = subprocess.CompletedProcess([], 0, "")
        unloaded = subprocess.CompletedProcess([], 1, "")
        with patch.object(services, "launchctl", side_effect=[ok, unloaded, ok, ok, unloaded]) as ctl:
            self.assertEqual(backend.control("start", label, path), "bootstrapped")
            self.assertEqual(backend.control("stop", label, path), "booted out")
            self.assertEqual([call.args for call in ctl.call_args_list], [
                ("enable", target), ("print", target), ("bootstrap", domain, str(path)),
                ("bootout", target), ("print", target),
            ])

    def test_running_status_and_restart_keep_launchd_commands(self):
        backend = services.service_backend("macos")
        label = services.GATEWAY_LABEL
        target = "gui/%d/%s" % (os.getuid(), label)
        result = subprocess.CompletedProcess([], 0, "\tstate = running\n\tpid = 321\n\tlast exit code = 0\n")
        with patch.object(services, "launchctl", return_value=result) as ctl:
            view = backend.status(label, {"pid": None, "reason": ""}, {}, lambda data: [])
            self.assertEqual(view, {"state": "running", "pid": 321, "last_exit": "0", "loaded": True,
                                    "up": True, "reason": ""})
            ctl.assert_called_once_with("print", target, timeout=10)
            ctl.reset_mock()
            self.assertEqual(backend.control("restart", label, backend.path(label)), "restarted")
            self.assertEqual([call.args for call in ctl.call_args_list],
                             [("enable", target), ("print", target), ("kickstart", "-k", target)])


class ClientWebServiceTest(unittest.TestCase):
    def test_service_env_uses_worker_count(self):
        service = services.client_web_service({
            "client_web_php": "/usr/bin/php",
            "serve_client_web": "127.0.0.1:1",
        })

        self.assertEqual(services.CLIENT_WEB_WORKERS, "16")
        self.assertEqual(service.env["PHP_CLI_SERVER_WORKERS"], services.CLIENT_WEB_WORKERS)
