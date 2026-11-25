#!/usr/bin/env python3
"""cs land reports each landing to the Director. Temp repos, a fake node Director, the Client API."""
import sys
from pathlib import Path

sys.path.insert(0, str(next(p / "tasks/regressions/_lib" for p in Path(__file__).resolve().parents
                            if (p / "tasks/regressions/_lib").is_dir())))
from hermetic_env import hermetic_env
from scratch import scratch_dir

import http.server
import json
import os
import re
import subprocess
import threading
import urllib.parse

sys.dont_write_bytecode = True
KIT = Path(__file__).resolve().parents[3]
REPO = KIT.parent
sys.path.insert(0, str(REPO / "tests/director-spec"))
from harness.fake_director import FakeDirector  # noqa: E402

problems = []
NODE_TOKEN = "node-token-land-record"
NODE_FILE = True
OWNER_TOKEN = "owner-token-land-record"
RFC3339 = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")
UUID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
KEYS = set()


def expect(cond, msg, detail=""):
    print(("ok   " if cond else "FAIL ") + msg + ("" if cond else "  " + str(detail)[:800]))
    if not cond:
        problems.append(msg)


def git(repo, *args):
    env = hermetic_env()
    for key in [k for k in env if k.startswith("GIT_")]:
        del env[key]
    env.update(GIT_TERMINAL_PROMPT="0", GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1",
               GIT_AUTHOR_NAME="Land Test", GIT_AUTHOR_EMAIL="land@test",
               GIT_COMMITTER_NAME="Land Test", GIT_COMMITTER_EMAIL="land@test",
               HOME=os.environ.get("HOME", ""))
    done = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, env=env)
    if done.returncode:
        raise SystemExit("git %s in %s: %s" % (args, repo, done.stderr))
    return done.stdout.strip()


class LandDirector(FakeDirector):
    """Node API stand-in plus POST runs/{id}/land (the route this machine reports to)."""

    def __init__(self):
        super().__init__(installation="inst-land")
        self.supported = [1, 2]
        self.nodes[NODE_TOKEN] = "n1"
        self.node_runs = {}
        self.idem = {}
        self._land_headers = {}

    def answer(self, method, path, headers, raw):
        self._land_headers = headers
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
                return 200, prev["record"]
            return 409, {"error": "Conflict", "code": "conflict", "detail": "idempotency conflict"}
        record = {"land_id": "land_%s" % key.replace("-", "")[:16], "run_id": run_id, "state": body["state"]}
        self.idem[key] = {"body": body, "run_id": run_id, "record": record}
        return 201, record


def _bad_land(body):
    state = body.get("state")
    if state not in ("landed", "failed", "refused"):
        return "state must be landed, failed or refused"
    for key in ("repo", "lane", "main", "landed_by", "landed_at"):
        if not isinstance(body.get(key), str):
            return "%s must be a string" % key
    if not RFC3339.fullmatch(body["landed_at"]):
        return "landed_at must be RFC3339"
    if state == "landed":
        if not isinstance(body.get("commit"), str) or not body["commit"] or body.get("reason") is not None:
            return "landed requires commit and a null reason"
    elif body.get("commit") is not None or not isinstance(body.get("reason"), str) or not body["reason"]:
        return "failed and refused require a null commit and a reason"
    return None


class JobAPI:
    """Client API stand-in: GET jobs and GET jobs/{id}, owner credential."""

    def __init__(self, owner):
        self.owner, self.jobs, self.calls = owner, [], []
        self.lock = threading.Lock()
        api = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_GET(self):
                headers = {k.lower(): v for k, v in self.headers.items()}
                path = urllib.parse.unquote(urllib.parse.urlsplit(self.path).path)
                rel = path.split("/api/v1/client/", 1)[1] if "/api/v1/client/" in path else None
                with api.lock:
                    api.calls.append({"method": "GET", "path": rel, "headers": headers})
                    if headers.get("authorization") != "Bearer " + api.owner:
                        status, body = 401, {"error": "Unauthorized", "code": "unauthenticated", "detail": "stand-in"}
                    elif rel == "jobs":
                        status, body = 200, {"jobs": list(api.jobs)}
                    elif isinstance(rel, str) and rel.startswith("jobs/"):
                        found = next((job for job in api.jobs if job["job_id"] == rel.split("/", 1)[1]), None)
                        status, body = (200, found) if found else (
                            404, {"error": "Not Found", "code": "not_found", "detail": "no such job"})
                    else:
                        status, body = 404, {"error": "Not Found", "code": "not_found", "detail": "no such route"}
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(data)

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.url = "http://127.0.0.1:%d" % self.httpd.server_address[1]
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


def job(job_id, run_id, repo, lane, state, finished_at):
    return {"job_id": job_id, "run_id": run_id, "state": state, "finished_at": finished_at,
            "created_at": finished_at, "run_ids": [run_id] if run_id else [],
            "meta": {"repo": repo, "lane": lane, "source": "cs-mcp"}}


def add_job(director, api, row):
    api.jobs.append(row)
    if row.get("run_id"):
        director.node_runs[row["run_id"]] = "n1"


def write_machine(home, client_url, node_url):
    root = Path(home) / ".contextstack"
    config = root / "config"
    (config / "policy").mkdir(parents=True)
    (root / "state/director").mkdir(parents=True)
    (config / "install.json").write_text(json.dumps({"director": client_url, "node": "n1"}) + "\n")
    (config / "policy/spool.json").write_text(json.dumps({"receiver": {
        "kind": "director", "url": node_url, "node": "n1", "token_env": "CONTEXTSTACK_NODE_TOKEN",
        "installation": "inst-land", "protocol": 2}}) + "\n")
    global NODE_FILE
    try:
        (config / "node.env").write_text("CONTEXTSTACK_NODE_TOKEN=%s\n" % NODE_TOKEN)
    except PermissionError:
        # This sandbox refuses to create a file named *.env. The recorder then
        # reads token_env from the environment, which cs_land sets.
        NODE_FILE = False
    cred = root / "state/director/local-credential"
    cred.write_text(OWNER_TOKEN + "\n")
    cred.chmod(0o600)


def cs_land(home, repo, lane, *extra):
    env = hermetic_env()
    for key in [k for k in env if k.startswith("GIT_")]:
        del env[key]
    env.update(HOME=str(home), CS_REPOS_FILE=os.environ["CS_REPOS_FILE"],
               PATH=os.environ.get("PATH", "/usr/bin:/bin"), GIT_TERMINAL_PROMPT="0")
    if not NODE_FILE:
        env["CONTEXTSTACK_NODE_TOKEN"] = NODE_TOKEN
    done = subprocess.run([sys.executable, "-I", str(KIT / "scripts/cs"), "land", repo, lane, "--json", *extra],
                          capture_output=True, text=True, env=env, timeout=90)
    try:
        out = json.loads(done.stdout)
    except ValueError:
        out = {"stdout": done.stdout, "stderr": done.stderr}
    return done.returncode, out, done.stderr


def posts_since(director, mark):
    return [call for call in director.calls[mark:] if call["method"] == "POST" and call["path"].endswith("/land")]




def absolute_path_strings(value):
    """String fields in a land post must not carry filesystem paths."""
    found = []

    def walk(obj):
        if isinstance(obj, str) and obj.startswith("/"):
            found.append(obj)
        elif isinstance(obj, dict):
            for item in obj.values():
                walk(item)
        elif isinstance(obj, list):
            for item in obj:
                walk(item)

    walk(value)
    return found


def check_post(call, out, run_id, state, target_branch="main"):
    body = json.loads(call["body"] or b"null")
    expect(call["path"] == "runs/%s/land" % run_id, "posts runs/%s/land" % run_id, call["path"])
    expect(call["headers"].get("authorization") == "Bearer " + NODE_TOKEN, "land post uses the node token")
    expect(OWNER_TOKEN not in (call["headers"].get("authorization") or ""), "land post does not use the owner credential")
    expect(call["headers"].get("contextstack-protocol") == "2", "land post speaks node protocol 2",
           call["headers"].get("contextstack-protocol"))
    expect(call["headers"].get("contextstack-installation") == "inst-land", "land post names the installation")
    key = call["headers"].get("idempotency-key") or ""
    expect(UUID.fullmatch(key) is not None, "one idempotency key per landing", key)
    KEYS.add(key)
    paths = absolute_path_strings(body)
    expect(not paths, "land post body has no absolute path", paths)
    expect(body.get("state") == state and body.get("repo") == out.get("repo") and body.get("lane") == out.get("lane")
           and body.get("main") == target_branch and body.get("landed_by") == "n1"
           and RFC3339.fullmatch(body.get("landed_at") or "") is not None,
           "body for %s" % state, body)
    if state == "landed":
        expect(body.get("commit") == out.get("commit") and body.get("reason") is None,
               "landed names the new commit and no reason", body)
    else:
        expect(body.get("commit") is None and isinstance(body.get("reason"), str) and body["reason"],
               "%s has a reason and no commit" % state, body)
    expect(call.get("status") in (200, 201) and out.get("recorded") is True and "record_error" not in out
           and out.get("land_id") and out["land_id"] == call_land_id(call),
           "cs land --json records the land_id", (call.get("status"), out.get("recorded"), out.get("record_error"),
                                                  out.get("land_id")))
    return body


def call_land_id(call):
    # The stand-in derives land_id from the key, so the JSON result can be checked without a second read.
    key = call["headers"].get("idempotency-key") or ""
    return "land_%s" % key.replace("-", "")[:16]


def check_mcp():
    sys.path.insert(0, str(KIT / "scripts"))
    import cs_mcp
    from unittest.mock import patch
    schema = next(tool for tool in cs_mcp.TOOLS if tool[0] == "land_task")
    expect("job_id" in schema[2] and "job_id" not in schema[3], "MCP land_task job_id is optional", schema[3])
    captured = {}

    class FakeLand:  # land_task runs `cs land` with Popen (so a timeout can stop it gracefully); record its argv
        def __init__(self, argv, **_kwargs):
            captured["argv"] = list(argv)
            self.pid, self.returncode = 0, 2

        def communicate(self, timeout=None):
            return '{"status": "refused", "reason": "stub"}\n', ""

    tools = cs_mcp.Tools(None, "n1")
    with patch.object(cs_mcp.subprocess, "Popen", FakeLand), patch.object(cs_mcp.agents, "kit_dir", return_value=KIT):
        out = tools.land_task("demo", "fix", job_id="job_abc")
    argv = captured.get("argv") or []
    expect(out.get("exit") == 2 and "--job" in argv and argv[argv.index("--job") + 1] == "job_abc",
           "land_task passes --job JOB_ID", argv)
    with patch.object(cs_mcp.subprocess, "Popen", FakeLand), patch.object(cs_mcp.agents, "kit_dir", return_value=KIT):
        tools.land_task("demo", "fix")
    expect("--job" not in (captured.get("argv") or []), "land_task omits --job when no job is given", captured.get("argv"))
    try:
        tools.land_task("demo", "fix", job_id="not a job")
        expect(False, "land_task refuses a malformed job_id")
    except cs_mcp.Refused:
        expect(True, "land_task refuses a malformed job_id")


def main():
    check_mcp()
    with scratch_dir(prefix="land-record-") as raw:
        tmp = Path(raw)
        repos = tmp / "repos.json"
        os.environ["CS_REPOS_FILE"] = str(repos)
        home = tmp / "home"
        home.mkdir()
        os.environ["HOME"] = str(home)  # fixture git must not read the operator's ~/.gitconfig
        main_repo = tmp / "main"
        main_repo.mkdir()
        git(main_repo, "init", "-q", "-b", "main")
        git(main_repo, "config", "user.name", "Land Test")
        git(main_repo, "config", "user.email", "land@test")
        (main_repo / "README").write_text("seed\n")
        git(main_repo, "add", "README")
        git(main_repo, "commit", "-q", "-m", "seed")
        repos.write_text(json.dumps({"demo": {"path": str(main_repo), "gate":
                                              "test -f green.txt || { echo 'FAIL rule-green: no green.txt'; exit 1; }"}}))
        sys.path.insert(0, str(KIT / "ops/bin"))
        import _lanes
        director = LandDirector()
        api = JobAPI(OWNER_TOKEN)
        write_machine(home, api.url, director.url)
        dead = tmp / "dead-home"
        dead.mkdir()
        write_machine(dead, "http://127.0.0.1:1", "http://127.0.0.1:1")

        def make_lane(name, files):
            info = _lanes.ensure_lane(_lanes.repo_config("demo"), name)
            path = Path(info["path"])
            for rel, text in files.items():
                dest = path / rel
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_text(text)
            commit = _lanes.commit_lane(path, "lane %s" % name, main=info["main"], lane=name)
            expect(bool(commit), "lane %s committed" % name, commit)
            return path

        def remember(repo_name, lane, run, state, when):
            add_job(director, api, job("job_" + lane + "_" + run, run, repo_name, lane, state, when))

        try:
            remember("demo", "dirty", "run_dirty", "done", "2026-11-10T00:00:00Z")
            remember("demo", "red", "run_red", "done", "2026-11-10T00:00:00Z")
            remember("demo", "ok", "run_ok", "done", "2026-11-10T00:00:00Z")
            # Unfinished, other lane, and other repo are newer; the old match is listed first and last.
            add_job(director, api, job("job_pick_open", "run_pick_open", "demo", "pick", "accepted", "2026-11-20T00:00:00Z"))
            remember("demo", "pick", "run_pick_old", "done", "2026-11-01T00:00:00Z")
            remember("demo", "pick", "run_pick_new", "done", "2026-11-12T00:00:00Z")
            remember("demo", "pick", "run_pick_mid", "done", "2026-11-02T00:00:00Z")
            remember("demo", "elsewhere", "run_elsewhere", "done", "2026-11-30T00:00:00Z")
            add_job(director, api, job("job_other_repo", "run_other_repo", "other", "pick", "done", "2026-11-30T00:00:00Z"))
            remember("demo", "forced", "run_force_new", "done", "2026-11-12T00:00:00Z")
            remember("demo", "forced", "run_force_old", "done", "2026-11-01T00:00:00Z")

            before_head = git(main_repo, "rev-parse", "HEAD")
            (main_repo / "stray.txt").write_text("dirty\n")
            mark, reads = len(director.calls), len(api.calls)
            code, out, err = cs_land(home, "demo", "dirty")
            posted = posts_since(director, mark)
            expect(len(posted) == 1 and len(api.calls) == reads + 1 and api.calls[-1]["path"] == "jobs"
                   and api.calls[-1]["headers"].get("authorization") == "Bearer " + OWNER_TOKEN,
                   "refused land looks up jobs with the owner credential",
                   (len(posted), [c["path"] for c in api.calls[reads:]]))
            body = check_post(posted[0], out, "run_dirty", "refused") if posted else {}
            expect(code == 2 and out.get("status") == "refused" and "uncommitted" in out.get("reason", "")
                   and body.get("reason") == out.get("reason") and git(main_repo, "rev-parse", "HEAD") == before_head,
                   "a dirty main is refused, reported with that text, and unchanged", (code, out, err))
            (main_repo / "stray.txt").unlink()

            # A busy refusal (another landing holds the lock) says nothing about the lane: never posted
            import fcntl
            lock_path = Path(git(main_repo, "rev-parse", "--path-format=absolute", "--git-common-dir")) / "cs-land.lock"
            mark = len(director.calls)
            with open(lock_path, "a+") as held:
                fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
                code, out, err = cs_land(home, "demo", "dirty")
            expect(code == 2 and "another landing" in out.get("reason", "") and not posts_since(director, mark)
                   and "couldn't be recorded" not in err,
                   "a lock/busy refusal is not posted as a refused land record", (code, out, err))

            make_lane("red", {"red.txt": "r\n"})
            mark = len(director.calls)
            code, out, err = cs_land(home, "demo", "red")
            posted = posts_since(director, mark)
            body = check_post(posted[0], out, "run_red", "failed") if posted else {}
            expect(code == 1 and out.get("status") == "red" and body.get("reason") == (out.get("failed") or [None])[0]
                   and body.get("reason") == "FAIL rule-green: no green.txt"
                   and git(main_repo, "rev-parse", "HEAD") == before_head,
                   "a red check is failed, with its first FAIL line, and main stays", (code, out, body, err))

            make_lane("ok", {"green.txt": "g\n"})
            mark = len(director.calls)
            code, out, err = cs_land(home, "demo", "ok")
            posted = posts_since(director, mark)
            head = git(main_repo, "rev-parse", "HEAD")
            body = check_post(posted[0], out, "run_ok", "landed") if posted else {}
            expect(code == 0 and out.get("status") == "landed" and out.get("commit") == head
                   and body.get("commit") == head and "couldn't be recorded" not in err
                   and not git(main_repo, "branch", "--list", "lane/ok"),
                   "a green land reports the new commit", (code, out, body, err))

            make_lane("pick", {"pick.txt": "p\n"})
            mark, reads = len(director.calls), len(api.calls)
            code, out, err = cs_land(home, "demo", "pick")
            posted = posts_since(director, mark)
            body = check_post(posted[0], out, "run_pick_new", "landed") if posted else {}
            expect(code == 0 and [c["path"] for c in api.calls[reads:]] == ["jobs"]
                   and body.get("state") == "landed",
                   "lookup posts the newest finished matching job, not an older or unfinished one",
                   (code, [c["path"] for c in api.calls[reads:]], posted[0]["path"] if posted else None, out, err))

            make_lane("forced", {"forced.txt": "f\n"})
            mark, reads = len(director.calls), len(api.calls)
            code, out, err = cs_land(home, "demo", "forced", "--job", "job_forced_run_force_old")
            posted = posts_since(director, mark)
            body = check_post(posted[0], out, "run_force_old", "landed") if posted else {}
            expect(code == 0 and [c["path"] for c in api.calls[reads:]] == ["jobs/job_forced_run_force_old"]
                   and (posted and posted[0]["path"] == "runs/run_force_old/land"),
                   "--job wins over the newer finished job",
                   (code, [c["path"] for c in api.calls[reads:]], posted[0]["path"] if posted else None, out))

            make_lane("solo", {"solo.txt": "s\n"})
            mark, reads = len(director.calls), len(api.calls)
            solo_head = git(main_repo, "rev-parse", "HEAD")
            code, out, err = cs_land(home, "demo", "solo")
            expect(code == 0 and out.get("status") == "landed" and out.get("recorded") is False
                   and "no finished job" in (out.get("record_error") or "") and "couldn't be recorded" in err
                   and not posts_since(director, mark) and git(main_repo, "rev-parse", "HEAD") != solo_head,
                   "no matching job still lands, with a warning that it couldn't be recorded", (code, out, err))

            make_lane("down", {"down.txt": "d\n"})
            node_calls, client_calls = len(director.calls), len(api.calls)
            code, out, err = cs_land(dead, "demo", "down")
            head = git(main_repo, "rev-parse", "HEAD")
            expect(code == 0 and out.get("status") == "landed" and out.get("commit") == head
                   and out.get("ok") is True and out.get("recorded") is False and out.get("record_error")
                   and "couldn't be recorded" in err and NODE_TOKEN not in (done_text(out, err))
                   and len(director.calls) == node_calls and len(api.calls) == client_calls
                   and (main_repo / "down.txt").is_file(),
                   "an unreachable Director does not change the landing", (code, out, err))
            expect(len(KEYS) == 5, "each landing attempt uses its own idempotency key", KEYS)
        finally:
            director.close()
            api.close()
    print("land-record: %s" % ("FAIL (%d)" % len(problems) if problems else "ok"))
    return 1 if problems else 0


def done_text(out, err):
    return json.dumps(out) + err


if __name__ == "__main__":
    sys.exit(main())
