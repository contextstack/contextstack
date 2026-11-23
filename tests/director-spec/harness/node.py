"""The node side: the node client's ops (job-pull, spool-send, node-listen) run as subprocesses against the stand-in
Director. node-listen (protocol 2) runs in the background, never starting tick or schedule-tick ("dispatch": false).

The client is a directory holding the two ops and their helper modules (today cs/ops/bin). Each case gets its
own roots (CS_CONFIG_ROOT, CS_STATE_ROOT, CS_WORKSPACES_ROOT) under the suite's temp dir, a fixture token in
a 0600 node.env file, a temp HOME and nothing else from the caller's environment.
"""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

from harness.fake_director import FakeDirector
from harness.wire import run_fixture

TOKEN_ENV = "DIRECTOR_SPEC_NODE_TOKEN"


class NodeRig:
    def __init__(self, client, tmp):
        self.client, self.tmp = Path(client), Path(tmp)
        for d in ("config/policy", "state", "workspaces", "home"):
            (self.tmp / d).mkdir(parents=True, exist_ok=True)
        (self.tmp / "config/ESTATE").write_text("node-dspec\n")
        self.director = FakeDirector()
        self.token, self.installation = self.director.register("node-dspec")
        token_file = self.tmp / "config/node.env"
        token_file.write_text("export %s=%s\n" % (TOKEN_ENV, self.token))
        token_file.chmod(0o600)
        self.configure()

    @property
    def state(self):
        return self.tmp / "state"

    def configure(self, **receiver):
        """policy/spool.json naming the stand-in; receiver keys override (installation=None drops it)."""
        rec = {"kind": "director", "url": self.director.url, "node": "node-dspec", "token_env": TOKEN_ENV,
               "installation": self.installation}
        rec.update(receiver)
        rec = {k: v for k, v in rec.items() if v is not None}
        (self.tmp / "config/policy/spool.json").write_text(json.dumps({"version": 1, "min_free_bytes": 0, "receiver": rec}))

    def env(self, **more):
        return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(self.tmp / "home"),
                "CS_CONFIG_ROOT": str(self.tmp / "config"), "CS_STATE_ROOT": str(self.state),
                "CS_WORKSPACES_ROOT": str(self.tmp / "workspaces"), **more}

    def op(self, name, payload=None):
        r = subprocess.run([sys.executable, str(self.client / name)], input=json.dumps(payload or {}), text=True,
                           capture_output=True, cwd=str(self.tmp), env=self.env(), timeout=60)
        try:
            out = json.loads(r.stdout.strip().splitlines()[-1])
        except (ValueError, IndexError):
            out = {}
        return r.returncode, out, r.stderr

    def spool_run(self, run_id, **kw):
        """A finalized run as finalize-run leaves it: run dir, ledger row, outbox envelope (usage included)."""
        run = run_fixture(run_id, **kw)
        run_dir = self.state / "runs" / run_id
        for path, data in run["files"].items():
            (run_dir / path).parent.mkdir(parents=True, exist_ok=True)
            (run_dir / path).write_bytes(data)
        with open(self.state / "ledger.jsonl", "a", encoding="utf-8") as ledger:
            ledger.write(run["row_line"] + "\n")
        outbox = self.state / "outbox"
        outbox.mkdir(exist_ok=True)
        (outbox / (run_id + ".json")).write_text(json.dumps(run["envelope"], sort_keys=True) + "\n")
        return run

    def calls(self, method=None, path=None, probes=False):
        """The requests the stand-in judged; a protocol-2 try answered 426 before the fall back to 1 is a probe,
        left out unless probes=True."""
        return [c for c in self.director.calls if (method is None or c["method"] == method)
                and (path is None or (c["path"] or "").startswith(path)) and (probes or c.get("status") != 426)]

    def listen(self, seconds=120, dispatch=False, client=None, **env):
        """node-listen in the background (protocol 2); its stdout lines land in listen.log. `dispatch` lets it start
        tick: only with `client`, a copy of the node client whose tick is a stand-in."""
        client = Path(client or self.client)
        if not (client / "node-listen").is_file():
            raise AssertionError("the node client has no node-listen")
        if dispatch and client == self.client:
            raise AssertionError("dispatch needs a client copy with a stand-in tick")
        self.listen_log = open(self.tmp / "listen.log", "ab")
        self.listener = subprocess.Popen([sys.executable, str(client / "node-listen")], stdin=subprocess.PIPE,
                                         stdout=self.listen_log, stderr=subprocess.STDOUT, cwd=str(self.tmp),
                                         env=self.env(CS_CANCEL_GRACE_S="5", **env))
        self.listener.stdin.write(json.dumps({"seconds": seconds, "dispatch": dispatch}).encode())
        self.listener.stdin.close()
        return self.listener

    def stop_listening(self):
        proc = getattr(self, "listener", None)
        if proc is not None and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        if getattr(self, "listen_log", None):
            self.listen_log.close()
        self.listener = None

    def log(self):
        try:
            return [json.loads(line) for line in (self.tmp / "listen.log").read_text().splitlines() if line.startswith("{")]
        except OSError:
            return []

    def until(self, check, timeout=15, what="condition"):
        """Poll check() until it is truthy; returns its value, or raises naming what never happened."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            value = check()
            if value:
                return value
            time.sleep(0.1)
        raise AssertionError("%s did not happen within %ss; listener log: %s" % (what, timeout, self.log()[-5:]))

    def dispatch_rows(self):
        path = self.state / "dispatch.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.is_file() else []

    def close(self):
        self.stop_listening()
        self.director.close()
