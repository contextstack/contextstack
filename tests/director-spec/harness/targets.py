"""Directors under test. Each target starts a Director and offers the two administrative acts the cases need,
which are not node calls (NODE-PROTOCOL.md §9): register a node, and offer it the fixture job (wire.JOB_FILES).

  target.url                       base URL
  target.register(name) -> (token, installation)
  target.offer(node_name) -> job_id
  target.close()

Targets: `rust` (the Director, director/; the default and the reference) and `fake` (the suite's stand-in). The
Laravel app's node API, once the protocol-1 reference, was retired with Director.Plan step 8: nodes talk only to
director-rs. A new Director adds a target here: start it on a temp database and a free port, and map register/offer
onto its own admin commands.
"""
import json
import os
import secrets
import socket
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

from harness.fake_director import FakeDirector
from harness.fake_gateway import FakeGateway
from harness.wire import JOB_FILES, SPEC


class FakeTarget:
    """The suite's own stand-in, so the Director cases also judge it (it is what the node side runs against)."""

    def __init__(self, tmp):
        self.director = FakeDirector()
        self.url = self.director.url
        self.names = {}

    def register(self, name):
        token, installation = self.director.register(name)
        return token, installation

    def offer(self, name):
        return self.director.offer(name)["job_id"]

    def close(self):
        self.director.close()


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class RustTarget:
    """The standalone Rust Director (director/, Director.Plan step 3): its binary serving a temp data dir on
    a free localhost port. Registration and job creation are its own admin subcommands, run against the same
    data dir. The environment is PATH, a temp HOME and nothing else."""

    def __init__(self, tmp, binary):
        self.binary = Path(binary).resolve()
        if not self.binary.is_file():
            raise SystemExit("director-spec: no Rust Director at %s (cargo build --release -p cs-director, "
                             "or pass --director-bin / $DIRECTOR_BIN)" % self.binary)
        self.tmp = Path(tmp) / "rust"
        self.data = self.tmp / "data"
        self.bundle = self.tmp / "bundle"
        (self.tmp / "home").mkdir(parents=True, exist_ok=True)
        for path, (data, mode) in JOB_FILES.items():
            dest = self.bundle / path
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(data)
            dest.chmod(mode)
        self.env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(self.tmp / "home"), "TMPDIR": str(self.tmp)}
        code = self.tmp / "code"
        (code / "cs").mkdir(parents=True, exist_ok=True)
        (code / "cs/agents").symlink_to(Path(__file__).resolve().parents[3] / "cs/agents")
        ops = code / "cs/ops/bin"
        ops.mkdir(parents=True, exist_ok=True)
        (ops / "executor-templates.json").symlink_to(Path(__file__).resolve().parents[3] / "cs/ops/bin/executor-templates.json")
        self.env["CS_CODE_ROOT"] = str(code)
        self.env["CS_LEAD_BUNDLE_DIR"] = str(Path(__file__).resolve().parents[3] / "cs/lead")
        # A stand-in gateway for source budgets, and its owner token in the temp HOME, where the real one would be.
        token_file = self.tmp / "home/.contextstack/config/gateway/owner-token"
        token_file.parent.mkdir(parents=True, exist_ok=True)
        token_file.parent.chmod(0o700)
        token = "dspec-owner-" + secrets.token_hex(8)
        token_file.write_text(token + "\n")
        token_file.chmod(0o600)
        self.gateway = FakeGateway(token)
        self.port = free_port()
        self.url = "http://127.0.0.1:%d" % self.port
        self.log = open(self.tmp / "server.log", "ab")
        self.extra = ()
        self.start()

    def start(self):
        self.proc = subprocess.Popen([str(self.binary), "serve", "--data-dir", str(self.data), "--listen", "127.0.0.1:%d" % self.port,
                                      # A tiny model window, so the conversation cases compact a short thread.
                                      "--context-window", "dspec-tiny=600", "--gateway", self.gateway.url, *self.extra],
                                     env=self.env, stdout=self.log, stderr=subprocess.STDOUT)
        deadline = time.time() + 20
        while True:
            try:  # every route needs a credential, so a bare probe answering 401 means it is up
                urllib.request.urlopen(self.url + "/api/v1/nodes/me/jobs", timeout=2).read()
            except urllib.error.HTTPError as answer:
                if answer.code == 401:
                    break
            except OSError:
                pass
            if time.time() > deadline or self.proc.poll() is not None:
                raise SystemExit("director-spec: the Rust Director did not start (see %s)" % (self.tmp / "server.log"))
            time.sleep(0.1)

    def admin(self, *args):
        result = subprocess.run([str(self.binary), *args, "--data-dir", str(self.data)], env=self.env, capture_output=True,
                                text=True, timeout=60)
        if result.returncode != 0:
            raise RuntimeError("director-rs %s: %s%s" % (" ".join(args), result.stdout, result.stderr))
        return json.loads(result.stdout.strip().splitlines()[-1])

    def register(self, name):
        printed = self.admin("node", "register", name)
        return printed["token"], printed["installation"]

    def offer(self, name):
        return self.admin("job", "create", "--node", name, "--bundle", str(self.bundle))["job_id"]

    # The client side's two administrative acts (CLIENT-API.md §2): the owner's local credential, and a
    # pairing code issued on this machine, exchanged over HTTP as a device would.
    def local_token(self):
        return (self.data / "local-credential").read_text().strip()

    def pair(self, name):
        from harness.wire import request
        code = self.admin("pair", name)["code"]
        answer = request(self.url, "POST", "/api/v1/pair", body={"code": code})
        if answer.status != 200:
            raise RuntimeError("pairing %s: %r" % (name, answer))
        return answer.json["token"], answer.json["credential_id"]

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=10)

    def restart(self, extra=()):
        """Stop the Director (SIGTERM) and start it again on the same data dir and port, with `extra` serve flags
        (none: as it was first started)."""
        self.stop()
        self.extra = tuple(extra)
        self.start()

    def close(self):
        self.stop()
        self.gateway.close()
        self.log.close()


def make(impl, tmp, director_bin=None):
    if impl == "fake":
        return FakeTarget(tmp)
    if impl == "rust":
        return RustTarget(tmp, director_bin)
    raise SystemExit("director-spec: unknown --impl %r (rust | fake)" % impl)

