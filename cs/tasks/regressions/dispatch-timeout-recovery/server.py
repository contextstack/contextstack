#!/usr/bin/env python3
"""Loopback OpenAI wire that holds until the client disconnects."""
import argparse
import json
import select
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def client_gone(connection):
    try:
        ready, _, _ = select.select([connection], [], [], 0)
        if not ready:
            return False
        data = connection.recv(1, socket.MSG_PEEK)
        return data == b""
    except OSError:
        return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready", required=True, type=Path)
    parser.add_argument("--log", required=True, type=Path)
    parser.add_argument("--state", required=True, type=Path)
    args = parser.parse_args()
    lock = threading.Lock()
    state = {"in_flight": 0, "completed": 0, "cancelled": 0}

    def persist():
        args.state.write_text(json.dumps(state) + "\n")

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def respond(self, status, body):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path != "/v1/models":
                return self.respond(404, {"error": "unknown path"})
            self.respond(200, {"object": "list", "data": [
                {"id": "fake-paid", "object": "model", "owned_by": "fixture"}]})

        def do_POST(self):
            if self.path != "/v1/chat/completions":
                return self.respond(404, {"error": "unknown path"})
            size = int(self.headers.get("Content-Length", "0"))
            if size < 1 or size > 65536:
                return self.respond(413, {"error": "bounded fixture request required"})
            try:
                body = json.loads(self.rfile.read(size))
            except ValueError:
                return self.respond(400, {"error": "invalid JSON"})
            hold = "TIMEOUT_HOLD" in json.dumps(body)
            with lock:
                state["in_flight"] += 1
                persist()
                args.log.open("a").write(json.dumps({
                    "path": self.path, "hold": hold, "body": body}) + "\n")
            try:
                if hold:
                    deadline = __import__("time").monotonic() + 60
                    while __import__("time").monotonic() < deadline:
                        if client_gone(self.connection):
                            with lock:
                                state["cancelled"] += 1
                                persist()
                            return
                        __import__("time").sleep(0.02)
                    self.respond(504, {"error": "fixture hold timed out"})
                    return
                self.respond(200, {
                    "id": "fixture-completion", "object": "chat.completion",
                    "model": body.get("model"),
                    "choices": [{"index": 0, "message": {
                        "role": "assistant", "content": "canned completion"},
                        "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2,
                              "total_tokens": 5, "cost": 0}})
                with lock:
                    state["completed"] += 1
                    persist()
            finally:
                with lock:
                    state["in_flight"] = max(0, state["in_flight"] - 1)
                    persist()

    persist()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    args.ready.write_text("http://127.0.0.1:" + str(server.server_port))
    server.serve_forever()


if __name__ == "__main__":
    main()
