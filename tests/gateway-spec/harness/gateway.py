"""Run a gateway binary as a black box: isolated env, temp HOME and cwd,
mock upstreams, the egress proxy, and HTTP clients over TCP or a unix socket.

The implementation profile (impls/<impl>.json) says how this binary is told
where things are; the cases never see it. Today both knobs the Go binary has
are env vars; a Rust binary may use the same names or declare its own.
"""
import http.client
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SPEC = Path(__file__).resolve().parents[1]
FIXTURES = SPEC / "fixtures"
V2 = "agent.gateway.model_call.v2"
V1 = "cs.gateway.model_call.v1"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self, path, timeout=30):
        super().__init__("localhost", timeout=timeout)
        self.unix_path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.unix_path)


class Response:
    def __init__(self, status, headers, raw):
        self.status, self.headers, self.raw = status, headers, raw

    @property
    def json(self):
        return json.loads(self.raw)

    @property
    def text(self):
        return self.raw.decode("utf-8", "replace")

    def sse(self):
        """data: frames as parsed JSON, in order."""
        out = []
        for block in self.text.split("\n\n"):
            for line in block.splitlines():
                if line.startswith("data: "):
                    out.append(json.loads(line[6:]))
        return out


def model_call(hint="local-coder", messages=None, schema=V2, **extra):
    body = {"schema_version": schema, "request_id": extra.pop("request_id", "spec-req-1"),
            "capability": {"kind": "model.call", "model_tier_hint": hint},
            "messages": messages if messages is not None else [{"role": "user", "content": "hi"}],
            "budget": {"timeout_ms": 10000, "max_output_tokens": 16}}
    for k, v in extra.items():
        if k == "budget":
            body["budget"].update(v)
        else:
            body[k] = v
    return body


# Pay-as-you-go spend (source budgets): the limit file and the per-source counter under HOME.
LIMITS = Path(".contextstack/config/gateway/limits.json")
SPEND = Path(".contextstack/gateway/spend.json")
# Every gateway the suite starts allows paid calls unless a case says otherwise: the other areas test
# routing, pricing and translation, not the limit (an absent file is every source off, the spend area's subject).
# The legacy shape is the shared "*" daily budget, so it opens every source.
OPEN_LIMITS = {"api_usd_per_day": 1000}


class Gateway:
    """One gateway process. `config` is the models.json document; `env` adds
    or overrides variables (None removes one); `limits` is the paid-API limit
    file's document (None: no file); `wrap` prefixes the binary's argv (a
    sandbox that denies an exec)."""

    def __init__(self, run, name, config, env=None, auth_token=None, socket_path=None, expect_start=True, limits=OPEN_LIMITS, wrap=()):
        self.run, self.name, self.wrap = run, name, list(wrap)
        self.dir = Path(tempfile.mkdtemp(prefix="gw-%s-" % name, dir=run.tmp))
        self.home = self.dir / "home"
        self.home.mkdir()
        self.limits_path, self.spend_path = self.home / LIMITS, self.home / SPEND
        if limits is not None:
            self.limits_path.parent.mkdir(parents=True)
            self.limits_path.write_text(limits if isinstance(limits, str) else json.dumps(limits))
        self.port = free_port()
        self.url = "http://127.0.0.1:%d" % self.port
        self.token = auth_token
        self.socket_path = socket_path
        self.models_path = self.dir / "models.json"
        self.models_path.write_text(json.dumps(config, indent=2))
        self.log_path = self.dir / "gateway.log"
        self.env = run.base_env(self)
        self.env.update({"CSGATEWAY_PORT": str(self.port), "CSAGENT_MODELS_CONFIG": str(self.models_path)})
        if auth_token:
            self.env["CSGATEWAY_AUTH_REQUIRED"] = "true"
            self.env["CSGATEWAY_AUTH_TOKEN"] = auth_token
        if socket_path:
            self.env["CSGATEWAY_SOCKET"] = str(socket_path)
        for k, v in (env or {}).items():
            if v is None:
                self.env.pop(k, None)
            else:
                self.env[k] = v
        self.proc = None
        if expect_start:
            self.start()

    # -- lifecycle
    def spawn(self):
        self.log = open(self.log_path, "ab")
        self.proc = subprocess.Popen(self.wrap + [self.run.binary], cwd=self.dir, env=self.env,
                                     stdout=self.log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        return self.proc

    def start(self, timeout=20):
        self.spawn()
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if self.proc.poll() is not None:
                raise AssertionError("gateway %s exited %s at startup:\n%s" % (self.name, self.proc.returncode, self.log_text()[-2000:]))
            try:
                if self.get("/v1/health", auth=False, timeout=2).status == 200:
                    return self
            except OSError:
                pass
            time.sleep(0.02)
        self.stop()
        raise AssertionError("gateway %s not ready in %ss" % (self.name, timeout))

    def start_fails(self, timeout=15):
        """For startup refusals: returns (exit code, log text)."""
        self.spawn()
        try:
            code = self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            self.stop()
            raise AssertionError("gateway %s started; a startup refusal was expected" % self.name)
        return code, self.log_text()

    def send(self, sig):
        self.proc.send_signal(sig)

    def stop(self, sig=signal.SIGTERM, timeout=12):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(sig)
            try:
                self.proc.wait(timeout)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(5)
        if self.proc:
            self.log.close()
        return self.proc.returncode if self.proc else None

    def log_text(self):
        try:
            return self.log_path.read_text(errors="replace")
        except OSError:
            return ""

    def log_rows(self, event):
        """JSON rows the gateway logged with this event name (admission_timing, wire_request)."""
        rows = []
        for line in self.log_text().splitlines():
            i = line.find("{")
            if i < 0:
                continue
            try:
                row = json.loads(line[i:])
            except ValueError:
                continue
            if isinstance(row, dict) and row.get("event") == event:
                rows.append(row)
        return rows

    # -- HTTP
    def conn(self, timeout, via_socket=False):
        if via_socket:
            return UnixHTTPConnection(str(self.socket_path), timeout=timeout)
        return http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)

    def request(self, method, path, body=None, headers=None, auth=True, timeout=30, via_socket=False, raw=None):
        h = {"Content-Type": "application/json"}
        if auth and self.token:
            h["Authorization"] = "Bearer " + self.token
        h.update(headers or {})
        data = raw if raw is not None else (None if body is None else json.dumps(body).encode())
        c = self.conn(timeout, via_socket)
        try:
            c.request(method, path, body=data, headers=h)
            r = c.getresponse()
            return Response(r.status, {k.lower(): v for k, v in r.getheaders()}, r.read())
        finally:
            c.close()

    def get(self, path, **kw):
        return self.request("GET", path, **kw)

    def post(self, path, body=None, **kw):
        return self.request("POST", path, body, **kw)

    def call(self, body, path="/v1/agent/model.call", **kw):
        return self.post(path, body, **kw)

    def abandon(self, path, body, after_s):
        """Send a request, then close the connection after_s seconds without
        reading the response: the client disconnect."""
        s = socket.create_connection(("127.0.0.1", self.port))
        data = json.dumps(body).encode()
        head = "POST %s HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: %d\r\n" % (path, len(data))
        if self.token:
            head += "Authorization: Bearer %s\r\n" % self.token
        s.sendall(head.encode() + b"\r\n" + data)
        time.sleep(after_s)
        s.close()

    def admission(self):
        return self.get("/v1/admission").json

    def wait(self, pred, timeout=10, what="condition"):
        end = time.monotonic() + timeout
        last = None
        while time.monotonic() < end:
            try:
                last = pred()
                if last:
                    return last
            except (OSError, ValueError, KeyError):
                pass
            time.sleep(0.02)
        raise AssertionError("timed out waiting for %s (last %r)" % (what, last))

    def cleanup(self):
        self.stop()
        if not self.run.keep:
            shutil.rmtree(self.dir, ignore_errors=True)


def python_exe():
    return os.path.realpath(sys.executable)
