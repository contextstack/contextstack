"""Wire helpers shared by the cases and the stand-in Director: HTTP calls, hashes, fixture bundles and runs."""
import hashlib
import http.client
import json
import re
import urllib.parse

PROTOCOL = 1
H_PROTOCOL, H_SUPPORTED, H_INSTALLATION = "ContextStack-Protocol", "ContextStack-Protocol-Supported", "ContextStack-Installation"
PREFIX = "/api/v1/nodes/me/"
INSTALLATION_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
UTC_RE = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ")


def sha(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode("utf-8")).hexdigest()


def bundle_sha(files):
    return sha("".join("%s:%s:%o\n" % (f["path"], f["sha256"], int(f["mode"])) for f in sorted(files, key=lambda f: f["path"])))


def envelope_sha(files):
    return sha("".join("%s:%s\n" % (f["path"], f["sha256"]) for f in sorted(files, key=lambda f: f["path"])))


class Answer:
    def __init__(self, status, headers, body):
        self.status, self.headers, self.body = status, headers, body

    def header(self, name):
        return self.headers.get(name.lower())

    @property
    def json(self):
        try:
            return json.loads(self.body or b"null")
        except ValueError:
            return None

    def __repr__(self):
        return "HTTP %s %s %s" % (self.status, {k: v for k, v in self.headers.items() if k.startswith("contextstack")},
                                  self.body[:300])


def call(base, method, path, token=None, headers=None, body=None, raw=None, timeout=30):
    """One request to <base>/api/v1/nodes/me/<path>. headers: extra headers (a None value omits that header)."""
    return request(base, method, PREFIX + path, token, headers, body, raw, timeout)


def request(base, method, path, token=None, headers=None, body=None, raw=None, timeout=30):
    """One request to <base><path> (path from the root, e.g. /api/v1/client/me)."""
    url = urllib.parse.urlsplit(base)
    conn = http.client.HTTPConnection(url.hostname, url.port or 80, timeout=timeout)
    send = {"Accept": "application/json"}
    if token is not None:
        send["Authorization"] = "Bearer " + token
    data = None
    if raw is not None:
        data, send["Content-Type"] = raw, "application/octet-stream"
    elif body is not None:
        data = body if isinstance(body, bytes) else json.dumps(body).encode("utf-8")
        send["Content-Type"] = "application/json"
    for k, v in (headers or {}).items():
        if v is None:
            send.pop(k, None)
        else:
            send[k] = v
    try:
        conn.request(method, url.path.rstrip("/") + path, body=data, headers=send)
        response = conn.getresponse()
        return Answer(response.status, {k.lower(): v for k, v in response.getheaders()}, response.read())
    finally:
        conn.close()


SPEC = """id: dspec-fixture
model: fake
system: prompts/p.md
context: []
tools: []
mcp: []
budget: {turns: 2, tool_calls: 0, usd: 0.25, seconds: 60}
tasks: [tasks/t]
trials: 1
"""
JOB_FILES = {"spec.yaml": (SPEC.encode(), 0o644), "prompts/p.md": (b"You are a fixture.\n", 0o644),
             "tasks/t/goal.md": (b"Say done.\n", 0o644), "tasks/t/check": (b"#!/bin/sh\ntrue\n", 0o755)}


def usage(**over):
    u = {"input_tokens": 1200, "output_tokens": 300, "est_usd": 0.0045, "est_basis": "rate", "charged_usd": 0,
         "source": {"class": "free_local", "model": "local-coder"}}
    u.update(over)
    return u


def run_fixture(run_id, overrides=None, drop=(), with_usage=True):
    """A finalized run as a node's outbox carries it: {files: {path: bytes}, envelope, row_line}."""
    files = {"terminal.json": json.dumps({"run_id": run_id, "state": "done"}).encode(), "transcript.md": b"# transcript\n",
             "check.json": b'{"pass": true}', "stimulus/manifest.json": b"{}", "stimulus/prompt.md": b"prompt\n"}
    files.update(overrides or {})
    for d in drop:
        files.pop(d, None)
    row_line = json.dumps({"run_id": run_id, "transcript": "ledger/runs/" + run_id, "verdict": "pass", "cost_usd": 0,
                           "est_usd": 0.0045, "est_basis": "rate", "cost_class": "free_local", "model": "local-coder"})
    listed = [{"path": p, "sha256": sha(b), "bytes": len(b)} for p, b in sorted(files.items())]
    envelope = {"envelope_version": 1, "event_id": "run_finalized:" + run_id, "run_id": run_id,
                "transcript": "ledger/runs/" + run_id, "row_sha256": sha(row_line), "row_version": 2, "error_class": None,
                "files": listed, "envelope_sha256": envelope_sha(listed)}
    if with_usage:
        envelope["usage"] = usage()
    return {"files": files, "envelope": envelope, "row_line": row_line}


def terminal_fixture(job_id, state="done"):
    row_line = json.dumps({"state": state, "director_job": job_id, "run_id": None})
    envelope = {"envelope_version": 1, "event_id": "job_terminal:" + job_id, "kind": "job_terminal", "job_id": job_id,
                "run_id": None, "files": [], "envelope_sha256": sha(b""), "row_sha256": sha(row_line)}
    return {"envelope": envelope, "row_line": row_line}
