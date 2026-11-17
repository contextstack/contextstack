"""A stand-in for the director's node API, so scripts/check needs no PHP.

Speaks the contract of director/app/Http/Controllers/Api/Node*Controller.php (Phase6.EstateSplit §3):
GET jobs, POST jobs/<id>/accept, GET/PUT objects/<sha256>, POST envelopes -> 200 {status: acked |
missing | rejected | conflict}. It verifies like the real receiver: every object present and
re-hashed, envelope_sha256 recomputed, row_line hashed, the run restored into <root>/runs/<run>
(terminal.json naming the run, transcript.md, check.json) before an ack; the row appended once.
`down = True` answers 503 to everything (a disconnect); `bad_objects` serves wrong bytes for those shas.
Node protocol 1 (tests/director-spec/NODE-PROTOCOL.md): every answer names `installation` and the protocol; a request
without ContextStack-Protocol 1 is 426, one naming another installation 421. The full stand-in is
tests/director-spec/harness/fake_director.py.
"""
import hashlib
import http.server
import json
import shutil
import threading
import time
from pathlib import Path


def sha(data):
    return hashlib.sha256(data).hexdigest()


def bundle(files):
    """files: {path: (bytes, mode)} -> the job's file list and bundle_sha256, as JobBundler builds them."""
    listed = sorted(({"path": p, "sha256": sha(b), "bytes": len(b), "mode": m} for p, (b, m) in files.items()),
                    key=lambda f: f["path"])
    lines = "".join("%s:%s:%o\n" % (f["path"], f["sha256"], f["mode"]) for f in listed)
    return listed, sha(lines.encode("utf-8"))


class FakeDirector:
    def __init__(self, root, token):
        self.root, self.token = Path(root), token
        (self.root / "objects").mkdir(parents=True, exist_ok=True)
        self.jobs, self.acks, self.down, self.bad_objects, self.calls = {}, {}, False, set(), []
        self.installation = "inst-fx"
        self.lock = threading.Lock()
        director = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def reply(self, status, body, ctype="application/json"):
                data = body if isinstance(body, bytes) else json.dumps(body).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", ctype)
                self.send_header("ContextStack-Installation", director.installation)
                self.send_header("ContextStack-Protocol-Supported", "1")
                if status < 400:
                    self.send_header("ContextStack-Protocol", "1")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def route(self, method):
                raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
                path = self.path.split("/api/v1/nodes/me/", 1)[-1]
                with director.lock:
                    director.calls.append((method, path))
                    if director.down:
                        return self.reply(503, {"error": "down"})
                    if self.headers.get("Authorization") != "Bearer " + director.token:
                        return self.reply(401, {"error": "unauthenticated"})
                    if self.headers.get("ContextStack-Protocol") != "1":
                        return self.reply(426, {"code": "protocol_unsupported"})
                    if self.headers.get("ContextStack-Installation") != director.installation:
                        return self.reply(421, {"code": "installation_mismatch"})
                    status, body, ctype = director.handle(method, path, raw)
                self.reply(status, body, ctype)

            def do_GET(self):
                self.route("GET")

            def do_POST(self):
                self.route("POST")

            def do_PUT(self):
                self.route("PUT")

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = "http://127.0.0.1:%d" % self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def obj(self, digest):
        return self.root / "objects" / digest

    def offer(self, job_id, files, grant=None, meta=None):
        """grant: (id, yaml bytes) carried at grants/<id>.yaml, as director:node-dispatch --grant bundles it."""
        files = dict(files)
        if grant:
            files["grants/%s.yaml" % grant[0]] = (grant[1], 0o644)
        listed, digest = bundle(files)
        for path, (data, _) in files.items():
            self.obj(sha(data)).write_bytes(data)
        task = next(p.rsplit("/", 1)[0] for p in files if p.startswith("tasks/") and p.endswith("/goal.md"))
        self.jobs[job_id] = {"job_id": job_id, "spec": "spec.yaml", "task": task, "trials": 1,
                             "bundle_sha256": digest, "files": listed, "state": "offered", "terminal": None,
                             "grant": grant[0] if grant else None, "grant_sha256": sha(grant[1]) if grant else None,
                             "meta": meta or {}}
        return self.jobs[job_id]

    def handle(self, method, path, raw):
        parts = path.split("/")
        if method == "GET" and path == "jobs":
            return 200, {"jobs": [{k: v for k, v in j.items() if k not in ("state", "terminal")}
                                  for j in self.jobs.values() if j["state"] in ("offered", "accepted")]}, "application/json"
        if method == "POST" and parts[0] == "jobs" and len(parts) == 3 and parts[2] == "accept":
            job = self.jobs.get(parts[1])
            if not job:
                return 404, {}, "application/json"
            if json.loads(raw or b"{}").get("bundle_sha256") != job["bundle_sha256"]:
                return 409, {}, "application/json"
            if job["state"] == "offered":
                job["state"] = "accepted"
            return 200, {"job_id": job["job_id"], "state": job["state"]}, "application/json"
        if parts[0] == "objects" and len(parts) == 2:
            if method == "GET":
                if not self.obj(parts[1]).is_file():
                    return 404, {}, "application/json"
                data = self.obj(parts[1]).read_bytes()
                return 200, (data + b"x" if parts[1] in self.bad_objects else data), "application/octet-stream"
            if sha(raw) != parts[1]:
                return 422, {}, "application/json"
            stored = not self.obj(parts[1]).is_file()
            self.obj(parts[1]).write_bytes(raw)
            return 200, {"sha256": parts[1], "stored": stored}, "application/json"
        if method == "POST" and path == "envelopes":
            body = json.loads(raw)
            return 200, self.receive(body["envelope"], body.get("row_line")), "application/json"
        return 404, {}, "application/json"

    def append_once(self, shelf, line, same):
        path = self.root / shelf
        rows = path.read_text(encoding="utf-8").splitlines() if path.is_file() else []
        if not any(same(r) for r in rows):
            with open(path, "a", encoding="utf-8") as out:
                out.write(line + "\n")

    def receive(self, env, row_line):
        event, digest = env["event_id"], env["envelope_sha256"]
        if event in self.acks:
            ack = self.acks[event]
            return {"status": "acked", "ack": ack} if ack["envelope_sha256"] == digest else \
                {"status": "conflict", "error": "event acked with another envelope"}
        files = env.get("files") or []
        missing = [f["sha256"] for f in files if not self.obj(f["sha256"]).is_file()]
        if missing:
            return {"status": "missing", "missing": missing}
        for f in files:
            if sha(self.obj(f["sha256"]).read_bytes()) != f["sha256"]:
                return {"status": "rejected", "error": "object %s" % f["sha256"]}
        lines = "".join("%s:%s\n" % (f["path"], f["sha256"]) for f in sorted(files, key=lambda f: f["path"]))
        if sha(lines.encode("utf-8")) != digest:
            return {"status": "rejected", "error": "envelope_sha256"}
        if env.get("row_sha256") and (row_line is None or sha(row_line.encode("utf-8")) != env["row_sha256"]):
            return {"status": "rejected", "error": "row_sha256"}
        if event.startswith("run_finalized:"):
            run = env["run_id"]
            stage = self.root / "runs" / (run + ".restoring")
            shutil.rmtree(stage, ignore_errors=True)
            for f in files:
                (stage / f["path"]).parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(self.obj(f["sha256"]), stage / f["path"])
            ok = (stage / "transcript.md").is_file() and (stage / "check.json").is_file() and \
                (stage / "terminal.json").is_file() and json.loads((stage / "terminal.json").read_text()).get("run_id") == run
            if not ok:
                shutil.rmtree(stage)
                return {"status": "rejected", "error": "not restorable"}
            target = self.root / "runs" / run
            shutil.rmtree(target, ignore_errors=True)
            stage.rename(target)
            transcript = json.loads(row_line).get("transcript")
            self.append_once("ledger.jsonl", row_line, lambda r: json.loads(r).get("transcript") == transcript)
        elif event.startswith("job_terminal:"):
            job = self.jobs[env["job_id"]]
            self.append_once("dispatch.jsonl", row_line, lambda r: r == row_line)
            job["terminal"] = json.loads(row_line)
            job["state"] = "done" if job["terminal"].get("state") == "done" else "error"
        else:
            return {"status": "rejected", "error": "unknown event"}
        self.acks[event] = {"event_id": event, "envelope_sha256": digest, "restored": True,
                            "verified_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        return {"status": "acked", "ack": self.acks[event]}

    def close(self):
        self.server.shutdown()
        self.server.server_close()
