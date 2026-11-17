"""The stand-in Director: node protocol 1 (NODE-PROTOCOL.md) in memory, over real HTTP on 127.0.0.1, and protocol 2
(§9) when `supported` lists 2 (the node side's protocol-2 cases; harness/fake_director2.py).

The node-side cases run the node client against it; `run --side director --impl fake` runs the Director cases
against it too, so the stand-in is held to the same spec as a real Director. Knobs for node-side faults:
  down               every request answers 503
  foreign            answers name another installation (a node pointed at the wrong Director)
  supported          the protocol versions spoken (default [1]); [2] makes a protocol-1 node get 426
  omit_protocol      2xx answers leave out ContextStack-Protocol (a Director that predates the protocol)
  bad_objects        shas whose GET serves wrong bytes
  envelope_answer    a dict returned for every POST envelopes instead of judging it
  accept_status      an HTTP status returned for every accept instead of judging it
  lose               {path prefix: n}: the next n such requests are judged and recorded, then answered 503 (a lost answer)
`calls` records every request: {method, path, headers (lower-cased), body, status}.
"""
import http.server
import json
import re
import secrets
import threading
import time

from harness.fake_director2 import Protocol2
from harness.wire import H_INSTALLATION, H_PROTOCOL, H_SUPPORTED, JOB_FILES, bundle_sha, envelope_sha, sha

SHA = re.compile(r"[a-f0-9]{64}")
PLAIN_RUN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
REASONS = {401: "Unauthorized", 404: "Not Found", 409: "Conflict", 421: "Misdirected Request", 422: "Unprocessable Content",
           426: "Upgrade Required", 503: "Service Unavailable"}
CODES = {401: "unauthenticated", 404: "not_found", 409: "conflict", 421: "installation_mismatch", 422: "invalid_request",
         426: "protocol_unsupported", 503: "request_failed"}


def refusal(status, **extra):
    return status, dict({"error": REASONS[status], "code": CODES[status], "detail": "stand-in"}, **extra)


def plain_path(path):
    return isinstance(path, str) and path != "" and not path.startswith("/") and "\\" not in path and "\0" not in path \
        and not any(part in ("", ".", "..") for part in path.split("/"))


class Rejected(Exception):
    pass


class FakeDirector(Protocol2):
    def __init__(self, installation="inst-standin"):
        self.installation, self.supported = installation, [1]
        self.down = self.foreign = self.omit_protocol = False
        self.bad_objects, self.envelope_answer, self.accept_status = set(), None, None
        self.nodes, self.jobs, self.objects, self.bundle_objects = {}, {}, {}, {}
        self.acked, self.ledger, self.dispatch, self.runs, self.calls = {}, [], [], {}, []
        self.lose = {}
        self.lock = threading.Lock()
        self.init2()
        director = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def handle_any(self, method):
                raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
                headers = {k.lower(): v for k, v in self.headers.items()}
                path = self.path.split("/api/v1/nodes/me/", 1)[1] if "/api/v1/nodes/me/" in self.path else None
                with director.lock:
                    call = {"method": method, "path": path, "headers": headers, "body": raw}
                    director.calls.append(call)
                    status, body, version = director.answer(method, path, headers, raw)
                while status == "hold":  # GET work held open (§9.2): the lock is free while it waits
                    deadline, held = body
                    time.sleep(0.05)
                    with director.lock:
                        status, body, version = director.work_again(held, deadline)
                with director.lock:
                    for prefix, n in list(director.lose.items()):
                        if n > 0 and path is not None and path.startswith(prefix):
                            director.lose[prefix] = n - 1
                            status, body, version = 503, {"error": "Service Unavailable", "code": "request_failed",
                                                          "detail": "answer lost"}, None
                            break
                call["status"] = status
                data = body if isinstance(body, bytes) else json.dumps(body).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "application/octet-stream" if isinstance(body, bytes) else "application/json")
                self.send_header("Content-Length", str(len(data)))
                if path is not None:
                    self.send_header(H_INSTALLATION, "inst-someone-else" if director.foreign else director.installation)
                    self.send_header(H_SUPPORTED, ",".join(str(v) for v in director.supported))
                    if version is not None and not (director.omit_protocol and status < 300):
                        self.send_header(H_PROTOCOL, str(version))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                self.handle_any("GET")

            def do_POST(self):
                self.handle_any("POST")

            def do_PUT(self):
                self.handle_any("PUT")

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = "http://127.0.0.1:%d" % self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    # ---- administration (not node calls; NODE-PROTOCOL.md §9) ----
    def register(self, name):
        token = secrets.token_urlsafe(36)
        self.nodes[token] = name
        return token, self.installation

    def offer(self, node, files=None, meta=None, job_id=None):
        files = dict(files or JOB_FILES)
        listed = sorted(({"path": p, "sha256": sha(b), "bytes": len(b), "mode": m} for p, (b, m) in files.items()),
                        key=lambda f: f["path"])
        for b, _ in files.values():
            self.objects[sha(b)] = b
        job_id = job_id or "J" + secrets.token_hex(12).upper()
        task = next(p.rsplit("/", 1)[0] for p in files if p.startswith("tasks/") and p.endswith("/goal.md"))
        self.jobs[job_id] = {"job_id": job_id, "spec": "spec.yaml", "task": task, "trials": 1, "bundle_sha256": bundle_sha(listed),
                             "files": listed, "grant": None, "grant_sha256": None, "meta": meta or {},
                             "node": node, "state": "offered"}
        self.bundle_objects.setdefault(node, set()).update(f["sha256"] for f in listed)
        return self.jobs[job_id]

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    # ---- node protocol 1 ----
    def answer(self, method, path, headers, raw):
        """(status, body, protocol version answered in or None)."""
        if self.down:
            return 503, {"error": "Service Unavailable", "code": "request_failed", "detail": "down"}, None
        if path is None:
            return refusal(404) + (None,)
        auth = headers.get("authorization") or ""
        node = self.nodes.get(auth[7:]) if auth.startswith("Bearer ") else None
        if node is None:
            return refusal(401) + (None,)
        sent = headers.get(H_PROTOCOL.lower())
        version = int(sent) if sent and re.fullmatch(r"[1-9][0-9]{0,8}", sent) else None
        if version not in self.supported:
            return refusal(426, protocol={"node": version, "supported": self.supported}) + (None,)
        if headers.get(H_INSTALLATION.lower()) != self.installation:
            return refusal(421, installation=self.installation) + (version,)
        if version == 2:
            routed = self.route2(node, method, path, raw)
            if routed is not None:
                return routed + (version,)
        elif path.split("?")[0] in ("work", "control", "status", "schedules") or path.startswith(("control/", "runs/")):
            return refusal(404) + (version,)
        self.seen_version[node] = version
        status, body = self.route(node, method, path, raw)
        return status, body, version

    def route(self, node, method, path, raw):
        parts = path.split("/")
        if method == "GET" and path == "jobs":
            return 200, {"jobs": [{k: v for k, v in j.items() if k not in ("node", "state")}
                                  for j in self.jobs.values() if j["node"] == node and j["state"] in ("offered", "accepted")]}
        if method == "POST" and len(parts) == 3 and parts[0] == "jobs" and parts[2] == "accept":
            return self.accept(node, parts[1], raw)
        if len(parts) == 2 and parts[0] == "objects" and SHA.fullmatch(parts[1]):
            if method == "GET":
                if parts[1] not in self.bundle_objects.get(node, set()) or parts[1] not in self.objects:
                    return refusal(404)
                data = self.objects[parts[1]]
                return 200, data + b"x" if parts[1] in self.bad_objects else data
            if method == "PUT":
                if sha(raw) != parts[1]:
                    return refusal(422)
                stored = parts[1] not in self.objects
                self.objects.setdefault(parts[1], raw)
                return 200, {"sha256": parts[1], "stored": stored}
        if method == "POST" and path == "envelopes":
            if self.envelope_answer is not None:
                return 200, self.envelope_answer
            try:
                body = json.loads(raw)
            except ValueError:
                body = None
            body = body if isinstance(body, dict) else {}
            return 200, self.receive(node, body.get("envelope"), body.get("row_line"))
        return refusal(404)

    def accept(self, node, job_id, raw):
        job = self.jobs.get(job_id)
        if job is None or job["node"] != node:
            return refusal(404)
        if self.accept_status is not None:
            return refusal(self.accept_status)
        try:
            want = json.loads(raw or b"{}").get("bundle_sha256")
        except (ValueError, AttributeError):
            want = None
        if not isinstance(want, str):
            return refusal(422)
        if want != job["bundle_sha256"]:
            return refusal(409)
        if job["state"] == "offered":
            job["state"] = "accepted"
        return 200, {"job_id": job_id, "state": job["state"]}

    def receive(self, node, env, row_line):
        try:
            kind, subject, files = self.parse(env, row_line)
        except Rejected as error:
            return {"status": "rejected", "error": str(error)}
        event = env["event_id"]
        if event in self.acked:
            owner, ack = self.acked[event]
            if owner != node or ack["envelope_sha256"] != env["envelope_sha256"]:
                return {"status": "conflict", "error": "%s is already acked with envelope_sha256 %s" % (event, ack["envelope_sha256"])}
            return {"status": "acked", "ack": ack}
        try:
            if kind == "run_finalized":
                missing = sorted({f["sha256"] for f in files if f["sha256"] not in self.objects})
                if missing:
                    return {"status": "missing", "missing": missing}
                self.accept_run(subject, files, env, row_line)
            else:
                self.accept_terminal(node, subject, files, env, row_line)
        except Rejected as error:
            return {"status": "rejected", "error": str(error)}
        ack = {"event_id": event, "envelope_sha256": env["envelope_sha256"],
               "verified_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "restored": True}
        self.acked[event] = (node, ack)
        return {"status": "acked", "ack": ack}

    def parse(self, env, row_line):
        if not isinstance(env, dict):
            raise Rejected("envelope must be a JSON object")
        if env.get("envelope_version") != 1 or isinstance(env.get("envelope_version"), bool):
            raise Rejected("envelope_version must be 1")
        match = re.fullmatch(r"(run_finalized|job_terminal):(.+)", env.get("event_id") or "") if isinstance(env.get("event_id"), str) else None
        if not match:
            raise Rejected("event_id must be run_finalized:<run> or job_terminal:<job>")
        kind, subject = match.groups()
        if not isinstance(env.get("envelope_sha256"), str) or not SHA.fullmatch(env["envelope_sha256"]):
            raise Rejected("envelope_sha256 must be a sha256")
        if env.get("row_sha256") is not None and not (isinstance(env["row_sha256"], str) and SHA.fullmatch(env["row_sha256"])):
            raise Rejected("row_sha256 must be null or a sha256")
        if row_line is not None and not isinstance(row_line, str):
            raise Rejected("row_line must be a string or null")
        if kind == "run_finalized":
            if env.get("run_id") != subject or not PLAIN_RUN.fullmatch(subject) or ".." in subject:
                raise Rejected("run_id must equal the event suffix and be a plain name")
            check_usage(env.get("usage"))
        elif "job_id" in env and env["job_id"] != subject:
            raise Rejected("job_id must equal the event suffix")
        files = env.get("files")
        if not isinstance(files, list):
            raise Rejected("files must be a list")
        seen = set()
        for f in files:
            ok = isinstance(f, dict) and plain_path(f.get("path")) and isinstance(f.get("sha256"), str) \
                and SHA.fullmatch(f["sha256"]) and isinstance(f.get("bytes"), int) and not isinstance(f["bytes"], bool) and f["bytes"] >= 0
            if not ok:
                raise Rejected("every file needs a plain relative path, a sha256 and bytes")
            if f["path"] in seen:
                raise Rejected("%s is listed twice" % f["path"])
            seen.add(f["path"])
        return kind, subject, files

    def row(self, env, row_line):
        """The row, once its line hashes to row_sha256; judged only when the event is settled, not on a repeat."""
        if env.get("row_sha256") is None:
            return None
        if row_line is None or sha(row_line) != env["row_sha256"]:
            raise Rejected("row_line is missing or does not hash to row_sha256")
        try:
            row = json.loads(row_line)
        except ValueError:
            row = None
        if not isinstance(row, dict):
            raise Rejected("row_line is not a JSON object")
        return row

    def accept_run(self, run_id, files, env, row_line):
        for f in files:
            data = self.objects[f["sha256"]]
            if sha(data) != f["sha256"] or len(data) != f["bytes"]:
                raise Rejected("object %s for %s does not verify" % (f["sha256"], f["path"]))
        if envelope_sha(files) != env["envelope_sha256"]:
            raise Rejected("envelope_sha256 does not match the listed files")
        row = self.row(env, row_line)
        paths = {f["path"]: f["sha256"] for f in files}
        try:
            terminal = json.loads(self.objects[paths["terminal.json"]]) if "terminal.json" in paths else None
        except ValueError:
            terminal = None
        if not isinstance(terminal, dict) or terminal.get("run_id") != run_id:
            raise Rejected("terminal.json is missing or does not name run %s" % run_id)
        for need in ("transcript.md", "check.json"):
            if need not in paths:
                raise Rejected("%s is missing" % need)
        if any(p.startswith("stimulus/") for p in paths) and "stimulus/manifest.json" not in paths:
            raise Rejected("stimulus/manifest.json is missing")
        if run_id in self.runs and any(self.runs[run_id].get(p) != s for p, s in paths.items()):
            raise Rejected("runs/%s/ already exists and differs from the envelope" % run_id)
        self.runs[run_id] = paths
        if row is not None and not any(r.get("transcript") == row.get("transcript") for r in map(json.loads, self.ledger)):
            self.ledger.append(row_line)

    def accept_terminal(self, node, job_id, files, env, row_line):
        job = self.jobs.get(job_id)
        if job is None or job["node"] != node:
            raise Rejected("job %s is not a job of this node" % job_id)
        if files:
            raise Rejected("a job_terminal envelope carries no files")
        if envelope_sha(files) != env["envelope_sha256"]:
            raise Rejected("envelope_sha256 does not match the listed files")
        row = self.row(env, row_line)
        if row is None:
            raise Rejected("row_sha256 is required")
        duplicate = row_line in self.dispatch
        if not duplicate:
            self.dispatch.append(row_line)
        job["state"] = "done" if row.get("state") == "done" else "error"
        if not duplicate:
            self.settle_by_terminal(job_id)


def check_usage(u):
    def count(v):
        return v is None or (isinstance(v, int) and not isinstance(v, bool) and v >= 0)

    def usd(v):
        return v is None or (isinstance(v, (int, float)) and not isinstance(v, bool) and v >= 0)

    if not isinstance(u, dict):
        raise Rejected("usage must be an object")
    for k in ("input_tokens", "output_tokens"):
        if k not in u or not count(u[k]):
            raise Rejected("usage.%s must be a non-negative integer or null" % k)
    for k in ("est_usd", "charged_usd"):
        if k not in u or not usd(u[k]):
            raise Rejected("usage.%s must be a non-negative number or null" % k)
    if "est_basis" not in u or not (u["est_basis"] is None or isinstance(u["est_basis"], str)):
        raise Rejected("usage.est_basis must be a string or null")
    s = u.get("source")
    if not isinstance(s, dict) or "class" not in s or "model" not in s or s["class"] not in (None, "api", "subscription", "free_local") \
            or not (s["model"] is None or isinstance(s["model"], str)):
        raise Rejected("usage.source must be {class, model}")
