"""Mock and replay upstreams, and the egress proxy every gateway runs behind.

Nothing here opens a connection to the outside world. The egress proxy is
the gateway's HTTP(S)_PROXY: a CONNECT to a known provider host is
TLS-terminated locally with the suite's own CA and served by a mock; any
other destination is recorded and refused. Loopback mocks are reached
directly (Go and most HTTP stacks never proxy loopback).
"""
import http.server
import json
import select
import socket
import ssl
import subprocess
import threading
import time
from pathlib import Path


class Recorder:
    """Thread-safe request log with a condition for waiting on arrivals."""

    def __init__(self):
        self.items = []
        self.cond = threading.Condition()

    def add(self, item):
        with self.cond:
            self.items.append(item)
            self.cond.notify_all()

    def wait(self, pred, timeout=10):
        end = time.monotonic() + timeout
        with self.cond:
            while not pred(self.items):
                left = end - time.monotonic()
                if left <= 0:
                    raise AssertionError("upstream wait timed out; saw %d request(s)" % len(self.items))
                self.cond.wait(left)

    def clear(self):
        with self.cond:
            self.items.clear()


def client_gone(conn):
    """True once the peer closed its side (a disconnect while held)."""
    try:
        r, _, _ = select.select([conn], [], [], 0)
        if not r:
            return False
        return conn.recv(1, socket.MSG_PEEK) == b""
    except (OSError, ValueError):
        return True


class Mock:
    """A scripted HTTP upstream. `routes` maps (method, path) to a responder:
    a callable(handler, body) -> spec, or a spec. A spec is one of
      {"json": obj, "status": 200}
      {"raw": bytes, "status": 200, "type": "text/plain"}
      {"sse": [str, ...], "delay_s": 0}   frames written verbatim, flushed one by one
      {"hold": "name", ...then}          wait for release("name") (records disconnects)
      {"drop": true}                      close the socket without a response
      {"redirect": url, "status": 307}    a redirect with no body
    Every request is recorded: method, path, headers (lower-case), json/raw body."""

    def __init__(self, name):
        self.name = name
        self.routes = {}
        self.requests = Recorder()
        self.gates = {}
        self.events = Recorder()  # hold/disconnect notes
        mock = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _serve(self):
                n = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(n) if n else b""
                try:
                    body = json.loads(raw) if raw else None
                except ValueError:
                    body = None
                path = self.path
                mock.requests.add({"method": self.command, "path": path,
                                   "headers": {k.lower(): v for k, v in self.headers.items()},
                                   "json": body, "raw": raw, "t": time.monotonic()})
                spec = mock.routes.get((self.command, path.split("?")[0]))
                if callable(spec):
                    spec = spec(self, body)
                if spec is None:
                    spec = {"json": {"error": "no route"}, "status": 404}
                mock.respond(self, spec)

            do_GET = do_POST = do_DELETE = _serve

        self.handler = Handler

    def release(self, name):
        self.gates.setdefault(name, threading.Event()).set()

    def respond(self, h, spec):
        if "hold" in spec:
            gate = self.gates.setdefault(spec["hold"], threading.Event())
            self.events.add({"held": spec["hold"]})
            while not gate.wait(0.02):
                if client_gone(h.connection):
                    self.events.add({"disconnected": spec["hold"]})
                    h.close_connection = True
                    return
            spec = spec.get("then") or {"json": {}}
        if spec.get("delay_s"):
            end = time.monotonic() + spec["delay_s"]
            while time.monotonic() < end:
                if client_gone(h.connection):
                    self.events.add({"disconnected": "delay"})
                    h.close_connection = True
                    return
                time.sleep(0.02)
        if spec.get("abort_body"):
            h.send_response(spec.get("status", 200))
            h.send_header("Content-Type", spec.get("type", "application/json"))
            h.send_header("Content-Length", str(spec.get("content_length", 4096)))
            h.end_headers()
            h.close_connection = True
            return
        if spec.get("drop"):
            h.close_connection = True
            try:
                h.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            return
        status = spec.get("status", 200)
        if "redirect" in spec:
            h.send_response(spec.get("status", 307))
            h.send_header("Location", spec["redirect"])
            h.send_header("Content-Length", "0")
            h.end_headers()
            return
        if "sse" in spec:
            h.send_response(status)
            h.send_header("Content-Type", "text/event-stream")
            h.send_header("Connection", "close")
            for k, v in (spec.get("headers") or {}).items():
                h.send_header(k, v)
            h.end_headers()
            for frame in spec["sse"]:
                if isinstance(frame, dict):  # {"sleep": s} or {"cut": true}
                    if frame.get("cut"):
                        h.close_connection = True
                        return
                    time.sleep(frame.get("sleep", 0))
                    continue
                try:
                    h.wfile.write(frame.encode())
                    h.wfile.flush()
                except OSError:
                    self.events.add({"disconnected": "sse"})
                    return
            h.close_connection = True
            return
        if "raw" in spec:
            data, ctype = spec["raw"], spec.get("type", "text/plain")
        else:
            data, ctype = json.dumps(spec.get("json", {})).encode(), "application/json"
        h.send_response(status)
        h.send_header("Content-Type", ctype)
        h.send_header("Content-Length", str(len(data)))
        for k, v in (spec.get("headers") or {}).items():
            h.send_header(k, v)
        h.end_headers()
        try:
            h.wfile.write(data)
        except OSError:
            pass


class Listener:
    """Serves a Mock on 127.0.0.1:<ephemeral>."""

    def __init__(self, mock):
        self.mock = mock
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), mock.handler)
        self.server.daemon_threads = True
        self.url = "http://127.0.0.1:%d" % self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


# --- OpenAI-compatible (Ollama /api/tags, vLLM /v1/models, chat/completions)

def chat_completion(text="ok", model="m", tool_calls=None, finish="stop", usage=(3, 2), cost=None, extra_message=None, cached_tokens=0):
    msg = {"role": "assistant", "content": text}
    if tool_calls:
        msg["tool_calls"] = tool_calls
    msg.update(extra_message or {})
    u = {"prompt_tokens": usage[0], "completion_tokens": usage[1], "total_tokens": usage[0] + usage[1]}
    if cached_tokens:
        u["prompt_tokens_details"] = {"cached_tokens": cached_tokens}
    if cost is not None:
        u["cost"] = cost
    return {"id": "chatcmpl-spec", "object": "chat.completion", "model": model,
            "choices": [{"index": 0, "finish_reason": finish, "message": msg}], "usage": u}


def openai_compat(name, ollama_models=None, vllm_models=None):
    """One local OpenAI-compatible server. Tests set `chat` (model -> spec) and
    the discovery lists; `discovery` overrides the listing response."""
    m = Mock(name)
    m.chat = {}
    m.default_chat = {"json": chat_completion()}
    m.models = list(vllm_models or [])
    m.tags = list(ollama_models or [])
    m.discovery = None  # spec overriding /v1/models or /api/tags

    def chat(h, body):
        spec = m.chat.get((body or {}).get("model"), m.default_chat)
        return spec(h, body) if callable(spec) else spec

    m.routes[("POST", "/v1/chat/completions")] = chat
    m.routes[("GET", "/v1/models")] = lambda h, b: m.discovery or {"json": {"object": "list", "data": [{"id": x, "object": "model"} for x in m.models]}}
    m.routes[("GET", "/api/tags")] = lambda h, b: m.discovery or {"json": {"models": [{"name": x} for x in m.tags]}}
    return m


def chat_chunk(delta=None, finish=None, usage=None, choices=True, cost=None):
    """One chat.completion.chunk payload (choices=False: the usage-only chunk)."""
    c = {"id": "chatcmpl-spec", "object": "chat.completion.chunk", "model": "m",
         "choices": [{"index": 0, "delta": delta or {}, "finish_reason": finish}] if choices else []}
    if usage is not None:
        c["usage"] = {"prompt_tokens": usage[0], "completion_tokens": usage[1], "total_tokens": usage[0] + usage[1]}
        if cost is not None:
            c["usage"]["cost"] = cost
    return c


def sse_data(obj):
    return "data: %s\n\n" % json.dumps(obj)


def chat_stream(text_parts=(), tool_deltas=(), finish="stop", usage=(3, 2), terminal=True, cost=None):
    """An OpenAI-compatible SSE stream: a role delta, content deltas, tool-call deltas (each a
    `tool_calls` list entry), the finish chunk, the usage-only chunk, then `data: [DONE]`."""
    data = sse_data
    frames = [data(chat_chunk({"role": "assistant", "content": ""}))]
    frames += [data(chat_chunk({"content": p})) for p in text_parts]
    frames += [data(chat_chunk({"tool_calls": [d]})) for d in tool_deltas]
    if terminal:
        frames += [data(chat_chunk({}, finish)), data(chat_chunk(usage=usage, choices=False, cost=cost)), "data: [DONE]\n\n"]
    return frames


# --- Anthropic Messages (served behind the TLS-terminating egress proxy)

def anthropic_message(content, stop="end_turn", usage=(11, 7), model="claude-spec"):
    return {"id": "msg_spec", "type": "message", "role": "assistant", "model": model, "content": content,
            "stop_reason": stop, "stop_sequence": None,
            "usage": {"input_tokens": usage[0], "output_tokens": usage[1]}}


def sse(event, data):
    return "event: %s\ndata: %s\n\n" % (event, json.dumps(data))


def anthropic_stream(text_parts, stop="end_turn", usage=(11, 7), terminal=True):
    frames = [sse("message_start", {"type": "message_start", "message": anthropic_message([], None, (usage[0], 1))}),
              sse("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})]
    for part in text_parts:
        frames.append(sse("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": part}}))
    if terminal:
        frames += [sse("content_block_stop", {"type": "content_block_stop", "index": 0}),
                   sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": usage[1]}}),
                   sse("message_stop", {"type": "message_stop"})]
    return frames


def anthropic(name="anthropic", models=()):
    m = Mock(name)
    m.messages = {"json": anthropic_message([{"type": "text", "text": "hello from anthropic"}])}
    m.models = list(models)
    m.discovery = None
    m.routes[("POST", "/v1/messages")] = lambda h, b: m.messages(h, b) if callable(m.messages) else m.messages
    m.routes[("GET", "/v1/models")] = lambda h, b: m.discovery or {"json": {"data": [{"id": x, "type": "model"} for x in m.models], "has_more": False}}
    return m


# --- Egress proxy: intercept known provider hosts, refuse everything else

class TLSMaterial:
    """A throwaway CA and one leaf for the intercepted hosts (openssl CLI)."""

    def __init__(self, root, hosts):
        root = Path(root)
        root.mkdir(parents=True, exist_ok=True)
        self.ca, self.cert, self.key = root / "ca.pem", root / "leaf.pem", root / "leaf.key"
        ca_key = root / "ca.key"
        ext = root / "leaf.ext"
        ext.write_text("basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\n"
                       "extendedKeyUsage=serverAuth\nsubjectAltName=" + ",".join("DNS:" + h for h in hosts) + "\n")
        run = lambda *a: subprocess.run(a, check=True, capture_output=True, timeout=30)
        run("openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-pkeyopt", "ec_param_enc:named_curve", "-nodes",
            "-keyout", str(ca_key), "-out", str(self.ca), "-days", "2", "-subj", "/CN=modelgateway-spec CA",
            "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
        csr = root / "leaf.csr"
        run("openssl", "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-pkeyopt", "ec_param_enc:named_curve", "-nodes",
            "-keyout", str(self.key), "-out", str(csr), "-subj", "/CN=" + hosts[0])
        run("openssl", "x509", "-req", "-in", str(csr), "-CA", str(self.ca), "-CAkey", str(ca_key),
            "-CAcreateserial", "-out", str(self.cert), "-days", "2", "-sha256", "-extfile", str(ext))


class EgressProxy:
    """HTTP proxy. `intercept` maps host -> Mock (served over local TLS).
    `seen` records every CONNECT or absolute-URI request: {"host", "port",
    "method", "intercepted"}. Nothing is ever forwarded."""

    def __init__(self, tls, intercept):
        self.seen = Recorder()
        self.intercept = intercept
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(str(tls.cert), str(tls.key))
        ctx.set_alpn_protocols(["http/1.1"])
        proxy = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_CONNECT(self):
                host, _, port = self.path.rpartition(":")
                mock = proxy.intercept.get(host)
                proxy.seen.add({"host": host, "port": port, "method": "CONNECT", "intercepted": mock is not None})
                if mock is None or port != "443":
                    self.send_response(403, "egress refused by modelgateway-spec")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    self.close_connection = True
                    return
                self.send_response(200, "Connection Established")
                self.end_headers()
                self.wfile.flush()
                try:
                    tls_sock = ctx.wrap_socket(self.connection, server_side=True)
                except (ssl.SSLError, OSError):
                    self.close_connection = True
                    return
                try:
                    mock.handler(tls_sock, self.client_address, self.server)
                except (ssl.SSLError, OSError):
                    pass
                self.close_connection = True

            def _refuse(self):
                proxy.seen.add({"host": self.headers.get("Host", ""), "port": "", "method": self.command,
                                "path": self.path, "intercepted": False})
                self.send_response(403, "egress refused by modelgateway-spec")
                self.send_header("Content-Length", "0")
                self.end_headers()
                self.close_connection = True

            do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = _refuse

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = "http://127.0.0.1:%d" % self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def refused(self):
        return [s for s in self.seen.items if not s["intercepted"]]

    def close(self):
        self.server.shutdown()
        self.server.server_close()
