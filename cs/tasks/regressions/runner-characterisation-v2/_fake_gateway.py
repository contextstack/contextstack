"""Fake model gateway for the runner characterisation fixtures.

Speaks the `agent.gateway.model_call.v2` wire the Go runner's gw.go
expects (gateway/WIRE.md): one POST per model call, a JSON envelope
with `status`, `model`, `message.content`, `tool_calls[]`, `usage` and
`cost{currency, amount, class, charged, est_usd, charged_usd}`, or
`status: "error"` with `error{class, code, message}`. Responses are
scripted per case (gateway.json); every request is recorded for the
fixture to pin.

Script entries (a JSON list; the last entry repeats once exhausted):
  {"ok": {"text", "tool_calls", "model", "cost", "usage"}}  shorthand
  {"ok": {..., "cost_numbers": false}}  a gateway before 1c1f172f3: no
                                        est_usd/charged_usd (a contract
                                        error for the runner)
  {"error": {"class", "code", "message"}, "http_status": 200}
  {"body": {...}}            literal envelope
  {"raw": "not json"}        literal bytes
  {"delay_s": 30}            hold the response (deadline / interrupt cases)
  "before_respond": [{"write": {"path", "content"}}]  estate mutation
"""
import http.server
import json
import threading
import time

SCHEMA = "agent.gateway.model_call.v2"


def cost_numbers(cost):
    """est_usd/charged_usd as the real gateway computes them (modelgateway
    agentCostFor, 1c1f172f3): est = amount; charged = `charged` when given,
    else amount when class is api, else 0. Every ok response carries both;
    a scripted cost must name its `class` (the post-wire)."""
    est = float(cost["amount"])
    charged = cost.get("charged")
    if charged is None:
        charged = est if cost["class"] == "api" else 0
    return {"est_usd": est, "charged_usd": charged}


def ok_body(spec):
    cost = spec.get("cost", {"currency": "USD", "amount": "0.010000", "class": "api"})
    if cost is not None and spec.get("cost_numbers", True):
        cost = dict(cost, **cost_numbers(cost))
    body = {
        "schema_version": SCHEMA, "status": "ok",
        "model": spec.get("model", "fake-model"),
        "message": {"role": "assistant", "content": spec.get("text", ""),
                    "stop_reason": "end_turn"},
        "tool_calls": spec.get("tool_calls", []),
        "usage": spec.get("usage", {"input_tokens": 10, "output_tokens": 5}),
    }
    if cost is not None:
        body["cost"] = cost
    return body


def error_body(spec):
    return {"schema_version": SCHEMA, "status": "error",
            "error": {"class": spec.get("class", "fatal_session"),
                      "code": spec.get("code", "gateway.unknown_model"),
                      "message": spec.get("message", ""),
                      "boundary": "gateway", "recoverable": False}}


class FakeGateway:
    def __init__(self, script, estate):
        self.script = list(script) or [{"ok": {"text": "done"}}]
        self.estate = estate
        self.index = 0
        self.lock = threading.Lock()
        self.requests = []
        self.arrived = threading.Condition()
        self.stop = threading.Event()
        gateway = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                try:
                    raw = self.rfile.read(length)
                except (ConnectionResetError, BrokenPipeError):
                    return
                try:
                    body = json.loads(raw)
                except ValueError:
                    body = raw.decode("utf-8", "replace")
                with gateway.lock:
                    entry = gateway.script[min(gateway.index, len(gateway.script) - 1)]
                    gateway.index += 1
                    gateway.requests.append({
                        "path": self.path,
                        "content_type": self.headers.get("Content-Type"),
                        "body": body})
                with gateway.arrived:
                    gateway.arrived.notify_all()
                for action in entry.get("before_respond", []):
                    if "write" in action:
                        target = gateway.estate / action["write"]["path"]
                        target.write_text(action["write"]["content"], encoding="utf-8")
                if entry.get("delay_s"):
                    gateway.stop.wait(float(entry["delay_s"]))
                if "raw" in entry:
                    payload = entry["raw"].encode("utf-8")
                elif "error" in entry:
                    payload = dumps(error_body(entry["error"]))
                elif "body" in entry:
                    payload = dumps(entry["body"])
                else:
                    payload = dumps(ok_body(entry.get("ok", {})))
                try:
                    self.send_response(int(entry.get("http_status", 200)))
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        class Quiet(http.server.ThreadingHTTPServer):
            def handle_error(self, request, client_address):
                # The runner hangs up mid-exchange on deadline and interrupt
                # cases; that is the behaviour under test, not a server fault.
                pass

        self.server = Quiet(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.endpoint = "http://127.0.0.1:%d/v1/agent/model.call" % self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def wait_requests(self, n, timeout):
        end = time.monotonic() + timeout
        with self.arrived:
            while len(self.requests) < n:
                remaining = end - time.monotonic()
                if remaining <= 0:
                    return False
                self.arrived.wait(remaining)
        return True

    def close(self):
        self.stop.set()
        self.server.shutdown()
        self.server.server_close()


def dumps(body):
    # Compact and key-sorted so `args` reaches the runner as stable bytes:
    # the runner echoes tool_call args verbatim into the transcript.
    return json.dumps(body, separators=(",", ":"), sort_keys=True).encode("utf-8")
