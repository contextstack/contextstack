"""Unit coverage for the installed client-web service definition."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "scripts"))
from _node_services import CLIENT_WEB_WORKERS, client_web_service  # noqa: E402


class ClientWebServiceTest(unittest.TestCase):
    def test_service_env_uses_worker_count(self):
        service = client_web_service({
            "client_web_php": "/usr/bin/php",
            "serve_client_web": "127.0.0.1:1",
        })

        self.assertEqual(CLIENT_WEB_WORKERS, "16")
        self.assertEqual(service.env["PHP_CLI_SERVER_WORKERS"], CLIENT_WEB_WORKERS)
