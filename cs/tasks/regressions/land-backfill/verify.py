#!/usr/bin/env python3
"""land-backfill: git landing subjects, job lookup before commit time, idempotent repost."""
import http.server
import json
import os
import re
import subprocess
import sys
import threading
import urllib.parse
from pathlib import Path

sys.path.insert(0, str(next(p / "tasks/regressions/_lib" for p in Path(__file__).resolve().parents
                            if (p / "tasks/regressions/_lib").is_dir())))
from hermetic_env import hermetic_env
from scratch import scratch_dir

sys.dont_write_bytecode = True
KIT = Path(__file__).resolve().parents[3]
REPO = KIT.parent
sys.path.insert(0, str(REPO / "tests/director-spec"))
from harness.fake_director import FakeDirector  # noqa: E402

problems = []
NODE_TOKEN = "node-token-land-backfill"
OWNER_TOKEN = "owner-token-land-backfill"
RFC3339 = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")


def expect(cond, msg, detail=""):
    print(("ok   " if cond else "FAIL ") + msg + ("" if cond else "  " + str(detail)[:800]))
    if not cond:
        problems.append(msg)


def git(repo, *args, env_extra=None):
    env = hermetic_env()
    for key in [k for k in env if k.startswith("GIT_")]:
        del env[key]
    env.update(GIT_TERMINAL_PROMPT="0", GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1",
               GIT_AUTHOR_NAME="Backfill Test", GIT_AUTHOR_EMAIL="bf@test",
               GIT_COMMITTER_NAME="Backfill Test", GIT_COMMITTER_EMAIL="bf@test",
               HOME=os.environ.get("HOME", ""))
    if env_extra:
        env.update(env_extra)
    done = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, env=env)
    if done.returncode:
        raise SystemExit("git %s in %s: %s" % (args, repo, done.stderr))
    return done.stdout.strip()


def commit_at(repo, message, when):
    env = {"GIT_AUTHOR_DATE": when, "GIT_COMMITTER_DATE": when}
    if repo.is_dir():
        (repo / ("touch-%s.txt" % when.replace(":", "-"))).write_text(message + "\n")
        git(repo, "add", ".")
    git(repo, "commit", "-q", "-m", message, env_extra=env)


class BackfillDirector(FakeDirector):
    """POST runs/{id}/land with per-run exclusivity and idempotent replay."""

    def __init__(self):
        super().__init__(installation="inst-land-bf")
        self.supported = [1, 2]
        self.nodes[NODE_TOKEN] = "n1"
        self.node_runs = {}
        self.idem = {}
        self.lands = {}
        self._land_headers = {}
        self._land_replay = False

    def answer(self, method, path, headers, raw):
        self._land_headers = headers
        self._land_replay = False
        return super().answer(method, path, headers, raw)

    def route(self, node, method, path, raw):
        match = re.fullmatch(r"runs/([^/]+)/land", path.split("?", 1)[0])
        if method == "POST" and match:
            return self.take_land(node, urllib.parse.unquote(match.group(1)), raw)
        return super().route(node, method, path, raw)

    def take_land(self, node, run_id, raw):
        key = (self._land_headers or {}).get("idempotency-key") or ""
        if not re.fullmatch(r"[A-Za-z0-9._:-]{1,128}", key):
            return 428, {"error": "Precondition Required", "code": "idempotency_key_required",
                         "detail": "missing or malformed Idempotency-Key"}
        if self.node_runs.get(run_id) != node:
            return 404, {"error": "Not Found", "code": "not_found", "detail": "run is not on this node"}
        try:
            body = json.loads(raw or b"null")
        except ValueError:
            body = None
        if not isinstance(body, dict):
            return 422, {"error": "Unprocessable Content", "code": "invalid_request", "detail": "body must be an object"}
        why = _bad_land(body)
        if why:
            return 422, {"error": "Unprocessable Content", "code": "invalid_request", "detail": why}
        prev = self.idem.get(key)
        if prev:
            if prev["body"] == body and prev["run_id"] == run_id:
                self._land_replay = True
                return 200, prev["record"]
            return 409, {"error": "Conflict", "code": "conflict", "detail": "idempotency conflict"}
        if run_id in self.lands:
            return 409, {"error": "Conflict", "code": "already_landed", "land": self.lands[run_id],
                         "detail": "this run already has a land record"}
        record = {"land_id": "land_%s" % key.replace("-", "")[:16], "run_id": run_id, "state": body["state"],
                  "commit": body.get("commit")}
        self.idem[key] = {"body": body, "run_id": run_id, "record": record}
        self.lands[run_id] = record
        return 201, record


def _bad_land(body):
    state = body.get("state")
    if state != "landed":
        return "state must be landed"
    for key in ("repo", "lane", "main", "landed_by", "landed_at"):
        if not isinstance(body.get(key), str):
            return "%s must be a string" % key
    if not RFC3339.fullmatch(body["landed_at"]):
        return "landed_at must be RFC3339 Z"
    if not isinstance(body.get("commit"), str) or not body["commit"] or body.get("reason") is not None:
        return "landed requires commit and a null reason"
    return None


class JobAPI:
    def __init__(self, owner):
        self.owner, self.jobs = owner, []
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self.httpd.daemon_threads = True
        self.url = "http://127.0.0.1:%d" % self.httpd.server_address[1]
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def _handler(self):
        api = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_GET(self):
                headers = {k.lower(): v for k, v in self.headers.items()}
                path = urllib.parse.urlsplit(self.path).path
                rel = path.split("/api/v1/client/", 1)[1] if "/api/v1/client/" in path else None
                if headers.get("authorization") != "Bearer " + api.owner:
                    status, body = 401, {"error": "Unauthorized", "code": "unauthenticated"}
                elif rel == "jobs":
                    status, body = 200, {"jobs": list(api.jobs)}
                else:
                    status, body = 404, {"error": "Not Found", "code": "not_found"}
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        return Handler

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


def job(job_id, run_id, repo, lane, state, finished_at):
    return {"job_id": job_id, "run_id": run_id, "state": state, "finished_at": finished_at,
            "created_at": finished_at, "run_ids": [run_id], "meta": {"repo": repo, "lane": lane, "source": "cs-mcp"}}


def add_job(director, api, row):
    api.jobs.append(row)
    director.node_runs[row["run_id"]] = "n1"


def write_machine(home, client_url, node_url):
    root = Path(home) / ".contextstack"
    config = root / "config"
    (config / "policy").mkdir(parents=True)
    (root / "state/director").mkdir(parents=True)
    (config / "install.json").write_text(json.dumps({"director": client_url, "node": "n1"}) + "\n")
    (config / "policy/spool.json").write_text(json.dumps({"receiver": {
        "kind": "director", "url": node_url, "node": "n1", "token_env": "CONTEXTSTACK_NODE_TOKEN",
        "installation": "inst-land-bf", "protocol": 2}}) + "\n")
    try:
        (config / "node.env").write_text("CONTEXTSTACK_NODE_TOKEN=%s\n" % NODE_TOKEN)
    except PermissionError:
        pass
    cred = root / "state/director/local-credential"
    cred.write_text(OWNER_TOKEN + "\n")
    cred.chmod(0o600)


def run_backfill(home, repo_name, dry=False):
    env = hermetic_env()
    env.update(HOME=str(home), CS_REPOS_FILE=os.environ["CS_REPOS_FILE"], PATH=os.environ.get("PATH", "/usr/bin:/bin"),
               CONTEXTSTACK_NODE_TOKEN=NODE_TOKEN)
    cmd = [sys.executable, "-I", str(KIT / "ops/bin/land-backfill"), repo_name]
    if dry:
        cmd.append("--dry-run")
    done = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=60)
    try:
        out = json.loads(done.stdout)
    except ValueError:
        out = {"stdout": done.stdout, "stderr": done.stderr, "code": done.returncode}
    return done.returncode, out, done.stderr


def land_posts(director):
    return [c for c in director.calls if c.get("method") == "POST" and c.get("path", "").endswith("/land")]


def main():
    with scratch_dir(prefix="land-backfill-") as raw:
        tmp = Path(raw)
        repos = tmp / "repos.json"
        os.environ["CS_REPOS_FILE"] = str(repos)
        home = tmp / "home"
        home.mkdir()
        os.environ["HOME"] = str(home)
        main_repo = tmp / "main"
        main_repo.mkdir()
        git(main_repo, "init", "-q", "-b", "main")
        git(main_repo, "config", "user.name", "Backfill Test")
        git(main_repo, "config", "user.email", "bf@test")
        (main_repo / "README").write_text("seed\n")
        git(main_repo, "add", "README")
        git(main_repo, "commit", "-q", "-m", "seed", env_extra={"GIT_AUTHOR_DATE": "2026-11-01T00:00:00Z",
                                                                 "GIT_COMMITTER_DATE": "2026-11-01T00:00:00Z"})
        commit_at(main_repo, "Land lane/squash-lane", "2026-11-10T12:00:00Z")
        commit_at(main_repo, "Merge branch 'lane/merge-lane'", "2026-11-11T12:00:00Z")
        commit_at(main_repo, "lane pick-lane: legacy landing subject", "2026-11-15T12:00:00Z")
        repos.write_text(json.dumps({"demo": {"path": str(main_repo)}}))
        director = BackfillDirector()
        # Patch HTTP handler to emit Idempotent-Replayed on land replays
        handler_cls = director.server.RequestHandlerClass

        class ReplayHandler(handler_cls):
            def send_response(self, code, message=None):
                super().send_response(code, message)
                if getattr(director, "_land_replay", False) and code == 200:
                    self.send_header("Idempotent-Replayed", "true")

        director.server.RequestHandlerClass = ReplayHandler

        api = JobAPI(OWNER_TOKEN)
        write_machine(home, api.url, director.url)
        add_job(director, api, job("j_squash", "run_squash", "demo", "squash-lane", "done", "2026-11-09T00:00:00Z"))
        add_job(director, api, job("j_merge", "run_merge", "demo", "merge-lane", "done", "2026-11-10T00:00:00Z"))
        add_job(director, api, job("j_pick_old", "run_pick_old", "demo", "pick-lane", "done", "2026-11-01T00:00:00Z"))
        add_job(director, api, job("j_pick_mid", "run_pick_mid", "demo", "pick-lane", "done", "2026-11-02T00:00:00Z"))
        add_job(director, api, job("j_pick_new", "run_pick_new", "demo", "pick-lane", "done", "2026-11-12T00:00:00Z"))
        add_job(director, api, job("j_pick_late", "run_pick_late", "demo", "pick-lane", "done", "2026-11-20T00:00:00Z"))

        mark = len(director.calls)
        code, summary, err = run_backfill(home, "demo")
        expect(code == 0, "land-backfill exits 0", (code, err, summary))
        expect(len(summary.get("recorded", [])) == 3, "three landings recorded", summary)
        expect(not summary.get("errors"), "no errors on first run", summary.get("errors"))
        posts = land_posts(director)[mark:]
        expect(len(posts) == 3, "three land posts", len(posts))
        pick = next((r for r in summary["recorded"] if r.get("lane") == "pick-lane"), None)
        expect(pick and pick.get("run_id") == "run_pick_new",
               "pick-lane uses the newest finished job before the commit", pick)

        mark = len(director.calls)
        code, summary2, err = run_backfill(home, "demo")
        expect(code == 0, "re-run exits 0", (code, err))
        expect(not summary2.get("recorded"), "re-run records nothing new", summary2.get("recorded"))
        expect(len(summary2.get("already_present", [])) == 3, "re-run treats posts as already present", summary2)
        posts2 = land_posts(director)[mark:]
        expect(all(c.get("status") == 200 for c in posts2), "re-run only idempotent replays", [c.get("status") for c in posts2])

        mark = len(director.calls)
        before_lands = len(director.lands)
        code, dry, err = run_backfill(home, "demo", dry=True)
        expect(code == 0, "--dry-run exits 0", err)
        expect(len(dry.get("dry_run", [])) == 3, "--dry-run lists three would-be posts", dry)
        expect(len(land_posts(director)[mark:]) == 0, "--dry-run posts nothing", len(land_posts(director)[mark:]))
        expect(len(director.lands) == before_lands, "--dry-run leaves director state unchanged", len(director.lands))

        api.close()
        director.close()

    print("land-backfill: %s" % ("FAIL (%d)" % len(problems) if problems else "ok"))
    if problems:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
