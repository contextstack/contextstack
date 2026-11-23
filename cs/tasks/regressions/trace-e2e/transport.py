"""Loopback transport controls for independent harness and gateway deadlines."""
import http.client
import json
import threading
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def start(port, scenario):
    class Transport(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=5)) as gateway:
                gateway.request("GET", self.path)
                reply = gateway.getresponse()
                body = reply.read()
                if scenario == "O" and self.path.partition("?")[0] == "/v1/models":
                    # Simulate stale discovery advertising streaming for this deployment.
                    catalog = json.loads(body)
                    for model in catalog["models"]:
                        if model["name"] == "box-model":
                            model["streaming"] = True
                    body = json.dumps(catalog).encode()
                self.send_response(reply.status)
                self.send_header("Content-Type", reply.headers["Content-Type"])
                self.end_headers()
                self.wfile.write(body)

        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            if scenario == "J":
                # The gateway deadline must fire before the harness's run deadline.
                data = json.loads(body)
                data["budget"]["timeout_ms"] = 750
                body = json.dumps(data).encode()
            with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=10)) as gateway:
                gateway.request("POST", self.path, body, {"Content-Type": self.headers["Content-Type"]})
                reply = gateway.getresponse()
                self.send_response(reply.status)
                self.send_header("Content-Type", reply.headers["Content-Type"])
                self.end_headers()
                try:
                    while chunk := reply.read1(65536):
                        self.wfile.write(chunk)
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Transport)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server
