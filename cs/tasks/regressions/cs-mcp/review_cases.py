"""Hermetic regressions for the MCP review findings; never execute git."""
import io
import http.server
import json
import os
import socket
import sys
import threading
import time
from pathlib import Path
from unittest.mock import patch


def check(m, scratch_dir):
    def call(tools, name, args):
        output = io.StringIO()
        m.serve(tools, io.StringIO(json.dumps({"id": 1, "method": "tools/call",
                "params": {"name": name, "arguments": args}}) + "\n"), output)
        return output.getvalue()

    secrets = ["-----BEGIN PRIVATE KEY-----\nprivatebody\n-----END PRIVATE KEY-----",
               "https://user:password@example.test", "Authorization: Basic abcdefghi",
               "Authorization: Bearer abcdefghi", "TEST_API_KEY=secretvalue",
               "bearer abcdefghi", "csl_" + "a" * 64, "csn_" + "b" * 64,
               "csd_" + "c" * 64, "cslead_abcdef", "sk-ant-abcdef",
               "sk-abcdefghijk", "ghp_abcdef", "github_pat_abcdef",
               "xoxb-abcdefghijk", "AKIA" + "A" * 16, "AIza" + "a" * 35,
               "eyJ" + "a" * 25 + ".abcdef"]
    payload = {"nested": [{"value": value} for value in secrets]}
    tools = m.Tools(None, "fake", land_command=lambda *a: m.subprocess.CompletedProcess([], 0, json.dumps(payload), ""))
    answer = json.loads(call(tools, "land_task", {"repo": "fake", "lane": "fake"}))
    result = json.loads(answer["result"]["content"][0]["text"])
    assert all("<redacted>" in row["value"] for row in result["nested"]), result
    with patch.object(tools, "task_status", side_effect=m.Refused("sk-abcdefghijk")):
        assert "sk-abcdefghijk" not in call(tools, "task_status", {"job_id": "fake"})

    with scratch_dir(prefix="mcp-timeout-") as root:
        stub = Path(root) / "land.py"
        marker = Path(root) / "terminated"
        stub.write_text("import signal,time,sys\nfrom pathlib import Path\n"
                        "def stop(*a):\n Path(" + repr(str(marker)) + ").write_text('TERM')\n sys.exit(0)\n"
                        "signal.signal(signal.SIGTERM,stop)\ntime.sleep(30)\n")
        tools = m.Tools(None, "fake")
        with patch.dict(os.environ, {"CS_MCP_LAND_STUB": str(stub)}), \
             patch.object(tools, "main_restored", return_value="main was restored (clean)"):
            answer = call(tools, "land_task", {"repo": "fake", "lane": "fake", "timeout_seconds": .2})
        assert marker.read_text() == "TERM" and "timed out" in answer and "main was restored" in answer

    class Delayed:
        def get(self, path, timeout=30):
            time.sleep(min(2, timeout))
            if timeout < 2:
                raise TimeoutError()
            return {"job": {"id": path.split("/")[-1], "state": "running"}}
    tools = m.Tools(Delayed(), "fake")
    with patch.object(tools, "open_stream", return_value=None):
        start = time.monotonic()
        assert tools.wait_tasks(["a", "b"], 1)["timed_out"]
        assert time.monotonic() - start < 3

    # A Director that stalls real HTTP job reads must honor the caller's remaining second.
    class SlowHTTP(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            time.sleep(2)
            try:
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"job":{"id":"a","state":"running"}}')
            except (BrokenPipeError, ConnectionResetError):
                pass
        def log_message(self, *args):
            pass
    with scratch_dir(prefix="mcp-http-") as root:
        credential = Path(root) / "credential"
        credential.write_text("synthetic")
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SlowHTTP)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            director = m.Director("http://127.0.0.1:%s" % server.server_port, credential)
            tools = m.Tools(director, "fake")
            with patch.object(tools, "open_stream", return_value=None):
                start = time.monotonic()
                assert tools.wait_tasks(["a", "b"], 1)["timed_out"]
                assert time.monotonic() - start < 3
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    # Real sockets exercise hello and change reads; polling is too long to satisfy the assertion.
    for race in (False, True):
        left, right = socket.socketpair()
        stream = left.makefile("rb")
        class Stream:
            fp = type("FP", (), {"raw": type("Raw", (), {"_sock": left})()})()
            readline = stream.readline
            def close(self):
                pass
        ended = threading.Event()
        subscribed = threading.Event()
        class Events:
            def request(self, *args, **kwargs):
                subscribed.set()
                if race:
                    ended.set()
                right.sendall(b"event: hello\n\n")
                return 200, Stream()
            def get(self, path, timeout=30):
                return {"job": {"id": "a", "state": "done" if ended.is_set() else "running"}}
        def finish():
            subscribed.wait()
            time.sleep(.15)
            ended.set()
            right.sendall(b'event: change\ndata: {"kind":"job","id":"a","record":{"state":"done"}}\n\n')
        worker = threading.Thread(target=finish)
        worker.start()
        try:
            with patch.dict(os.environ, {"CS_MCP_POLL_SECONDS": "60"}):
                start = time.monotonic()
                result = m.Tools(Events(), "fake").wait_tasks(["a"], 4)
                assert not result["timed_out"] and time.monotonic() - start < 2, result
        finally:
            worker.join()
            stream.close()
            left.close()
            right.close()
    print("ok   MCP timeout, redaction, stream wakeup and subscribe/snapshot race regressions")
