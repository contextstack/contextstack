"""cs init: one command from an installed release to a finished first job on this machine (install stage 0).

Loaded by scripts/cs (`cs init`), on the install's interpreter. Stdlib only. Steps, each reported:
  1. disk encryption: FileVault (macOS) or LUKS (Linux) off is a loud warning (the credential store is plain 0600 files)
  2. agents: which subscription CLIs are installed and signed in, without spending a call (`claude auth status`,
     `codex login status`, `cursor-agent status`, `opencode auth list`; gemini from its settings file); their token
     files are never read
  3. services: the Rust Director on loopback, the gateway and the node, through node-release activate (never a fork
     of it). This machine's node registers with its own Director (`director-rs node register`): the register output
     waits in config/.cs-init-node-token (0600) until the activation has taken it, then is deleted; it is never printed
  4. first job: a one-line task through the first signed-in CLI a gateway template runs (claude, codex, gemini,
     cursor-agent), with fallback, waited for in the Director's database (read-only), its reply printed; defaults saved
  5. API keys: explicit --api-key only, after the first job; the gateway reads each key hidden into the file store
Idempotent after success; a failed first job can be retried on rerun. --first-job runs another job after success.
"""
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import threading
from contextlib import contextmanager
from pathlib import Path

_SCRIPTS = Path(__file__).resolve().parent
if str(_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS))
sys.path.insert(0, str(_SCRIPTS.parent / "ops/bin"))
from _agent_discovery import AGENTS, discover_agents, probe_agent
import _install_setup
from _node_services import release_bin

FIRST_JOB = "cs-init-first-job"
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,99}")  # director-rs node names
PROVIDERS = ("anthropic", "openai", "groq", "together", "lambda", "openrouter", "gemini")  # csgateway credentials
LABELS = {"claude": "Claude Code", "codex": "Codex", "gemini": "Gemini", "cursor-agent": "Cursor",
          "opencode": "OpenCode", "pi": "Pi", "goose": "Goose", "grok": "Grok"}
LOGINS = {"claude": [], "codex": ["login"], "cursor-agent": ["login"], "gemini": []}
REPLY = "hello from ContextStack"


def sign_in_command(name):
    return {"claude": "claude, then /login", "gemini": "gemini, then sign in"}.get(name, name + " login")


class Init:
    def __init__(self, cs, args):
        self.cs, self.args = cs, args
        self.root, self.config = cs.ROOT, cs.CONFIG
        self.nr = cs.node_release()
        self.data = self.nr.DIRECTOR_RS_DATA
        self.marker = self.config / "cs-init.json"
        self.pending = self.config / ".cs-init-node-token"
        self.listen = args.director_listen or (self.nr.load_record().get("director_rs_listen") or [self.nr.DIRECTOR_RS_LISTEN])[0]
        self.url = "http://" + self.listen

    def say(self, line=""):
        print(line, flush=True)

    def detail(self, line):
        if getattr(self.args, "verbose", False):
            self.say(line)

    @contextmanager
    def stage(self, name):
        started, stopped = time.monotonic(), threading.Event()
        tty = sys.stderr.isatty()
        self.say("%s… (0s)" % name)

        def progress():
            tick = 0
            while not stopped.wait(0.2 if tty else 5):
                elapsed = int(time.monotonic() - started)
                if tty:
                    print("\r%s %s (%ds)" % ("|/-\\"[tick % 4], name, elapsed), end="", file=sys.stderr, flush=True)
                else:
                    self.say("%s… (%ds)" % (name, elapsed))
                tick += 1

        worker = threading.Thread(target=progress, daemon=True)
        worker.start()
        try:
            yield
        finally:
            stopped.set()
            worker.join()
            if tty:
                print("\r\033[2K", end="", file=sys.stderr, flush=True)
            self.say("%s (%ds)" % (name, int(time.monotonic() - started)))

    def retry(self):
        if not self.interactive():
            return False
        try:
            return input("Try again? [Y/n] ").strip().lower() in ("", "y", "yes")
        except EOFError:
            return False

    def cancel_job(self, job):
        import urllib.request
        import uuid
        try:
            token = (self.data / "local-credential").read_text().strip()
        except OSError:
            raise self.cs.Stop("cs init: cannot read the owner credential to cancel job %s; check cs doctor" % job)
        request = urllib.request.Request(self.url + "/api/v1/client/jobs/" + job + "/cancel",
            data=json.dumps({"reason": "cs init first job timed out"}).encode(),
            headers={"Authorization": "Bearer " + token, "Content-Type": "application/json",
                     "Idempotency-Key": str(uuid.uuid4())}, method="POST")
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                response.read()
        except OSError:
            raise self.cs.Stop("cs init: could not cancel timed-out job %s; retry after checking cs jobs" % job)

    def ask(self, question):
        if self.args.yes or not sys.stdin.isatty():
            return False
        return input("%s [y/N] " % question).strip().lower() in ("y", "yes")

    def run(self, argv, timeout=30, **kw):
        try:
            return subprocess.run(argv, capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL, **kw)
        except (OSError, subprocess.TimeoutExpired) as error:
            return subprocess.CompletedProcess(argv, 127, "", str(error))

    # ---- state ----------------------------------------------------------------------------------------------------
    def receiver(self):
        return self.cs.receiver() or {}

    def has_token(self, var):
        return var in self.cs.node_env()  # names only; the values join cs's redactor

    def initialised(self):
        rec, r = self.nr.load_record(), self.receiver()
        return bool(rec.get("serve_director_rs")) and r.get("url") == self.url and self.has_token(r.get("token_env") or "")

    def marker_data(self):
        return self.cs.read_json(self.marker, {}) or {}

    def write_marker(self, data):
        self.nr.write_atomic(self.marker, (json.dumps(data, indent=2, sort_keys=True) + "\n").encode(), 0o600)

    # ---- 1. disk encryption ---------------------------------------------------------------------------------------
    def disk(self):
        if self.cs.PLATFORM == "macos":
            r = self.run(["fdesetup", "status"])  # by PATH: tests shim it
            return (r.returncode == 0 and "FileVault is On" in r.stdout), "FileVault", (r.stdout or r.stderr).strip()
        r = self.run(["lsblk", "-rno", "TYPE"])
        return (r.returncode == 0 and "crypt" in r.stdout.split()), "LUKS", "lsblk: %s" % ("a crypt volume" if "crypt" in r.stdout else "none")

    def step_disk(self):
        on, what, detail = self.disk()
        if on:
            return
        self.say("Warning: %s is off." % what)
        self.say("Turn on FileVault: System Settings › Privacy & Security › FileVault" if what == "FileVault"
                 else "Turn on disk encryption: encrypt the volume holding your home directory (LUKS)")
        self.detail("%s (%s): provider API keys and this node's Director token are plain 0600 files under %s. "
                    "Anyone who gets this disk can read them." % (what, detail or "unknown", self.root))

    # ---- 2. agents ------------------------------------------------------------------------------------------------
    def service_path(self):
        """PATH the node service runs with (not the login shell). CS_SERVICE_PATH overrides (tests)."""
        if os.environ.get("CS_SERVICE_PATH") is not None:
            return os.environ["CS_SERVICE_PATH"]
        venv = self.cs.CURRENT / "code" / ".venv/bin"
        return "%s:%s" % (venv, self.nr.SYSTEM_PATH)

    def step_toolchains(self):
        sys.path.insert(0, str(_SCRIPTS.parent / "ops/bin"))
        from _toolchains import login_toolchain_additions

        destination = self.config / "toolchains.json"
        try:
            existing, added = login_toolchain_additions(destination, self.service_path())
            if added:
                self.config.mkdir(parents=True, exist_ok=True)
                self.nr.write_atomic(destination, (json.dumps(existing + added, indent=2) + "\n").encode(), 0o644)
        except (OSError, ValueError) as error:
            self.say("toolchains: %s" % error)
            return
        for entry in added:
            self.say("%s: %s" % (entry["name"], entry["bin"]))

    def agent_path(self):
        """The PATH the gateway resolves CLIs on (its plist), else what it would be; CS_AGENT_PATH overrides (tests)."""
        if os.environ.get("CS_AGENT_PATH") is not None:
            return os.environ["CS_AGENT_PATH"]
        _, gw = self.cs.load_plist(self.cs.GATEWAY)
        path = ((gw or {}).get("EnvironmentVariables") or {}).get("PATH")
        return path or "%s:%s" % (self.cs.HOME / ".local/bin", self.nr.SYSTEM_PATH)

    def probe(self, name, path):
        return probe_agent(name, path, self.cs.HOME, self.run, lambda path, default: self.cs.read_json(path, default))

    def interactive(self):
        return not self.args.yes and sys.stdin.isatty()

    def offer_login(self, agent):
        if not self.interactive():
            return False
        try:
            return input("Sign in to %s now? [Y/n] " % LABELS[agent["name"]]).strip().lower() in ("", "y", "yes")
        except EOFError:
            return False

    def login(self, agent):
        name = agent["name"]
        if name == "claude":
            self.say("In Claude Code, type /login. Exit Claude Code when you have finished.")
        elif name == "gemini":
            self.say("Sign in in Gemini, then exit when you have finished.")
        env = {k: v for k, v in os.environ.items() if k not in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY")}
        env["PATH"] = self.agent_path()
        try:
            # Inherit the terminal: only the official CLI handles credentials.
            subprocess.run([agent["path"], *LOGINS[name]], env=env)
        except OSError:
            self.say("Could not start %s. Try its login command below." % LABELS[name])

    def report_agent(self, agent):
        label = LABELS[agent["name"]]
        if not agent["path"]:
            self.say("Not installed: %s" % label)
        else:
            status = {True: "signed in", False: "not signed in", None: "sign-in status unknown"}[agent["signed_in"]]
            self.say("Found %s — %s" % (label, status))

    def write_agent_capabilities(self, found):
        self.config.mkdir(parents=True, exist_ok=True)
        capabilities = [{k: agent[k] for k in ("name", "path", "signed_in")} for agent in found]
        self.nr.write_atomic(self.config / "agents.json",
                             (json.dumps(capabilities, sort_keys=True) + "\n").encode(), 0o600)

    def step_agents(self):
        path = self.agent_path()
        self.say("Checking installed agents and sign-ins…")
        found = discover_agents(path, self.probe)
        self.write_agent_capabilities(found)
        for agent in found:
            self.report_agent(agent)
        if any(a["template"] and a["signed_in"] is True for a in found):
            return found
        for i, agent in enumerate(found):
            name = agent["name"]
            if not agent["path"] or name not in LOGINS or agent["signed_in"] is True:
                continue
            if self.offer_login(agent):
                self.login(agent)
                found[i] = self.probe(name, path)
                self.report_agent(found[i])
                if found[i]["signed_in"] is True:
                    break
            command = sign_in_command(name)
            self.say("To sign in later, run: %s" % command)
        if not any(a["template"] and a["path"] and a["signed_in"] is True for a in found):
            self.say("No signed-in agent? You can also use a paid API: cs init --api-key PROVIDER")
        self.write_agent_capabilities(found)
        return found


    def step_cli_pins(self):
        kit = _SCRIPTS.parent / "ops/bin"
        if not (kit / "_cli_pins.py").is_file():
            return
        sys.path.insert(0, str(kit))
        from _cli_pins import ensure_pin, package_sha256, profiled_templates, resolve_binary
        env = {"CS_CONFIG_ROOT": str(self.config)}
        path_probe = self.agent_path()
        probe_env = {k: v for k, v in os.environ.items()
                     if k not in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "XAI_API_KEY", "GROK_API_KEY")}
        probe_env["PATH"] = path_probe
        pinned = []
        for name, profile in profiled_templates(kit):
            try:
                binary = resolve_binary(profile["binary"], self.cs.HOME)
            except ValueError:
                continue
            r = self.run([str(binary), "--version"], 15, env=probe_env)
            if r.returncode != 0:
                continue
            version = (r.stdout or r.stderr).strip().splitlines()[0]
            pkg = package_sha256(profile["package"]) if profile.get("package") else None
            ensure_pin(name, binary, version, env=env, repin=False, package_sha=pkg)
            pinned.append(name)
        if pinned:
            self.detail("cli pins recorded: " + ", ".join(pinned))

    # ---- 3. services ----------------------------------------------------------------------------------------------
    def default_name(self):
        r = self.run(["scutil", "--get", "LocalHostName"], 5) if self.cs.PLATFORM == "macos" else None
        host = (r.stdout.strip() if r is not None and r.returncode == 0 else "") or socket.gethostname().split(".")[0]
        name = re.sub(r"-+", "-", re.sub(r"[^a-z0-9.-]", "-", host.lower())).strip("-.")[:63]
        return name or "this-mac"

    def node_name(self):
        estate = self.config / "ESTATE"
        have = estate.read_text(encoding="utf-8").strip() if estate.is_file() else None
        name = self.args.name or self.receiver().get("node") or have or self.default_name()
        if not NAME.fullmatch(name):
            raise self.cs.Stop("cs init: node name %r: want letters, digits, '.', '_' or '-' (--name NAME)" % name, 2)
        if have and have != name:
            raise self.cs.Stop("cs init: this install's machine name is %r; --name %r differs (use that name, or reinstall to change it)" % (have, name), 2)
        return name

    def register(self, name):
        """The register output in self.pending (0600), kept from an earlier run that stopped before activating."""
        if self.pending.is_file():
            self.detail("services: using the registration an earlier cs init left in %s" % self.pending)
            return
        if self.data.exists() and (not self.data.is_dir() or self.data.stat().st_mode & 0o077):
            raise self.cs.Stop("cs init: %s is open to other users (the Director refuses it): chmod 700 it" % self.data)
        self.data.mkdir(mode=0o700, parents=True, exist_ok=True)
        r = self.run([str(release_bin(self.cs.CURRENT, "cs-director")), "node", "register", name, "--data-dir", str(self.data)])
        if r.returncode != 0:
            why = (r.stderr or r.stdout).strip().splitlines()[-1:] or ["exit %d" % r.returncode]
            hint = " (this Director knows that name, and this machine has no token for it: pick another with --name)" \
                if "already exists" in why[0] else ""
            raise self.cs.Stop("cs init: director-rs node register %s: %s%s" % (name, why[0], hint))
        fd = os.open(self.pending, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(r.stdout.strip().splitlines()[-1] + "\n")

    def activate(self, name):
        rec = self.nr.load_record()
        argv = [sys.executable, "-I", str(self.cs.KIT / "scripts/node-release"), "activate", os.readlink(self.cs.CURRENT).rsplit("/", 1)[-1], "--no-smoke",
                "--skip-path-setup", "--node", name, "--director", self.url, "--serve-director-rs", "--director-rs-listen", self.listen,
                "--director-token-from", str(self.pending)]
        if not rec.get("credentials"):
            argv += ["--credentials", "file"]  # the decided default: the file store, never the Keychain
        r = self.run(argv, None)  # activate drains for up to 30 min: never kill it mid-switch
        out = self.cs.read_body(r.stdout)
        if r.returncode != 0 or not out.get("ok"):
            raise self.cs.Stop("cs init: node-release activate refused: %s (the registration is kept in %s; rerun cs init)"
                               % (out.get("refused") or (r.stderr or r.stdout).strip()[-400:], self.pending))
        self.pending.unlink()
        if getattr(self.args, "verbose", False):
            self.say(json.dumps(out, indent=2))

    def wait_up(self, seconds=60):
        """True when the Director answers (401: it wants a credential) and the gateway reports healthy."""
        end, director, gateway = time.monotonic() + seconds, False, False
        while time.monotonic() < end and not (director and gateway):
            director = director or self.cs.curl(self.url + "/api/v1/client/jobs")[0] in (200, 401)
            gateway = gateway or self.cs.curl("http://localhost/v1/health", unix=str(self.root / "gateway/csgateway.sock"))[0] == 200
            if not (director and gateway):
                time.sleep(0.5)
        return director, gateway

    def require_services(self):
        with self.stage("Waiting for services"):
            director, gateway = self.wait_up()
        if not (director and gateway):
            raise self.cs.Stop("cs init: %s does not answer yet (cs status; cs logs %s)"
                               % ("the Director" if not director else "the gateway", "director" if not director else "gateway"))

    def step_services(self):
        r = self.receiver()
        if r.get("url") and r["url"] != self.url:
            raise self.cs.Stop("cs init: this machine is a node of the Director at %s; cs init sets up a machine that is "
                               "its own Director (stage 0). `cs status` shows it." % r["url"])
        if self.initialised():
            self.detail("services: already set up (Director %s, node %s); nothing changed" % (self.url, r.get("node")))
            return r.get("node")
        name = self.node_name()
        self.detail("services: Director on %s, gateway, node %r (user agents)" % (self.url, name))
        self.register(name)
        self.activate(name)
        return name

    # ---- 4. API keys ----------------------------------------------------------------------------------------------
    def step_keys(self):
        wanted = list(self.args.api_key or [])
        if not wanted:
            return
        _, gw = self.cs.load_plist(self.cs.GATEWAY)
        store = ((gw or {}).get("EnvironmentVariables") or {}).get("CSGATEWAY_CREDENTIALS")
        if store == "off":
            raise self.cs.Stop("cs init: the gateway has no credential store (CSGATEWAY_CREDENTIALS=off); "
                               "run `cs doctor` for how to enable the file credential store")
        store = store or ""  # unset or empty is the gateway's default: the file store ~/.contextstack/credentials
        for provider in wanted:
            if provider not in PROVIDERS:
                raise self.cs.Stop("cs init: --api-key %r: want one of %s" % (provider, " ".join(PROVIDERS)), 2)
            # stdin is the person's terminal (or a pipe): csgateway-rs hides the input and never prints the value
            r = subprocess.run([str(release_bin(self.cs.CURRENT, "cs-gateway")), "credentials", "set", provider],
                               env=dict(os.environ, CSGATEWAY_CREDENTIALS=store), stdout=subprocess.PIPE, text=True)
            if r.returncode != 0:  # its prompt and its error went to the terminal (stderr)
                raise self.cs.Stop("cs init: credentials set %s failed (exit %d)" % (provider, r.returncode))
            self.detail("api keys: %s stored in %s" % (provider, store or "~/.contextstack/credentials"))
        self.nr.launchctl("kickstart", "-k", "gui/%d/%s" % (os.getuid(), self.cs.GATEWAY))  # re-reads the store

    # ---- 5. the first job -----------------------------------------------------------------------------------------
    def bundle(self, where, template, reply=REPLY, spec_id=FIRST_JOB):
        spec = ["id: %s" % spec_id, "model: %s" % template, "context: []", "tools: []", "mcp: []",
                "tasks: [tasks/hello]", "trials: 1"]
        if template.startswith("claude-code"):  # the executor requires a declared budget for claude, and only for it
            spec.insert(2, "budget: {turns: 2, tool_calls: 0, usd: 0, seconds: 180}")
        (where / "tasks/hello").mkdir(parents=True)
        (where / "spec.yaml").write_text("\n".join(spec) + "\n")
        (where / "tasks/hello/goal.md").write_text("Reply with exactly this line and nothing else: %s\n" % reply)
        check = where / "tasks/hello/check"
        check.write_text("#!/bin/sh\n# cs init's first job: the run finishing is the result\nexit 0\n")
        check.chmod(0o755)

    def job_row(self, job):
        import sqlite3
        import urllib.parse
        db = self.data / "director.sqlite"
        try:
            conn = sqlite3.connect("file:%s?mode=ro" % urllib.parse.quote(str(db)), uri=True, timeout=2)
            try:
                row = conn.execute("SELECT state, run_id, terminal_row FROM jobs WHERE job_id = ?", (job,)).fetchone()
            finally:
                conn.close()
        except sqlite3.Error:
            return None
        return row

    def reply(self, run_id):
        """The agent's last words from the node's run dir (cli.log's stream-json result, else the transcript)."""
        run = self.cs.state_root() / "runs" / str(run_id)
        for line in reversed((self.cs.read_text(run / "cli.log") or "").splitlines()):
            try:
                doc = json.loads(line)
            except ValueError:
                continue
            if isinstance(doc, dict) and doc.get("type") == "result" and isinstance(doc.get("result"), str):
                return doc["result"].strip(), run
        text = (self.cs.read_text(run / "transcript.md") or "").strip()
        return (text.splitlines()[-1] if text else ""), run

    def step_first_job(self, name, agents):
        self.needs_agent = False
        self.first_job_rc = 0
        done = self.marker_data().get("first_job") or {}
        if self.args.no_first_job:
            self.say("first job: skipped (--no-first-job)")
            self.first_job_rc = 0
            return 0
        if done.get("state") == "done" and not self.args.first_job:
            self.say("first job: done earlier (%s through %s); --first-job runs another job"
                     % (done.get("job"), done.get("agent")))
            self.first_job_rc = 0
            return 0
        ready = [a for a in agents if a["template"] and a["path"] and a["signed_in"] is True]
        if self.args.agent:
            ready = [a for a in ready if a["name"] == self.args.agent]
        if not ready:
            self.needs_agent = True
            return 0
        if done.get("state") in ("offered", "waiting", "accepted", "running", "unknown"):
            self.cancel_job(done["job"])
        for index, agent in enumerate(ready):
            while True:
                with self.stage("Running first job"):
                    rc = self.try_first_job(name, agent)
                if rc == 0:
                    self.first_job_rc = 0
                    return 0
                if not self.retry():
                    break
            self.say("%s could not answer.%s" % (LABELS[agent["name"]],
                     " Trying the next signed-in agent…" if index + 1 < len(ready) else ""))
        self.say("No signed-in agent answered. Check cs logs node and cs logs gateway, then run cs init --first-job.")
        self.first_job_rc = 1
        return 1

    def try_first_job(self, name, agent):
        self.detail("first job: asking %s for one line, through the Director and the node..." % agent["name"])
        started = time.monotonic()
        with tempfile.TemporaryDirectory(prefix="cs-init-") as tmp:
            self.bundle(Path(tmp), agent["template"])
            r = self.run([str(release_bin(self.cs.CURRENT, "cs-director")), "job", "create", "--node", name, "--bundle", tmp,
                          "--data-dir", str(self.data)])
        created = self.cs.read_body(r.stdout)
        if r.returncode != 0 or not created.get("job_id"):
            self.say("first job: could not create the job for %s" % LABELS[agent["name"]])
            return 1
        job, row, end = created["job_id"], None, time.monotonic() + self.args.timeout
        while time.monotonic() < end:
            row = self.job_row(job)
            if row and row[0] not in ("offered", "waiting", "accepted", "running"):
                break
            time.sleep(1)
        state = row[0] if row else "unknown"
        if state in ("offered", "waiting", "accepted", "running", "unknown"):
            self.cancel_job(job)
            state = "timed_out"
        record = {"job": job, "agent": agent["name"], "model": agent["template"], "state": state, "seconds": round(time.monotonic() - started, 1),
                  "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        self.write_marker({**self.marker_data(), "node": name, "director": self.url, "first_job": record})
        if state != "done":
            self.say("first job: %s is %s after %ds (cs jobs; cs logs node; cs logs gateway)" % (job, state, record["seconds"]))
            return 1
        text, run = self.reply(row[1])
        if not text.strip() or REPLY not in text:
            record["state"] = "invalid_reply"
            self.write_marker({**self.marker_data(), "node": name, "director": self.url, "first_job": record})
            self.say("first job: %s did not return the requested greeting" % LABELS[agent["name"]])
            return 1
        install = {**self.nr.load_record(), "default_agent": agent["name"], "default_model": agent["template"]}
        self.nr.write_atomic(self.config / "install.json", (json.dumps(install, indent=2, sort_keys=True) + "\n").encode(), 0o600)
        self.say("%s answered: %s ✓" % (LABELS[agent["name"]], text[:300]))
        self.detail("first job: done in %.0fs. %s replied: %s" % (record["seconds"], agent["name"], text[:300] or "(no text)"))
        self.detail("  job %s, run %s" % (job, row[1]))
        self.detail("  run files: %s" % run)
        return 0

    def first_job_failure_sentence(self, record):
        state = record.get("state") or "failed"
        job = record.get("job")
        agent = record.get("agent")
        label = LABELS.get(agent, agent or "the agent")
        if state == "timed_out":
            return "the first job timed out before %s answered" % label
        if state == "invalid_reply":
            return "%s answered but not with the expected greeting" % label
        if state in ("offered", "waiting", "accepted", "running"):
            return "the first job is still %s" % state
        if job:
            return "the first job (%s) ended as %s" % (job, state)
        return "the first job did not finish (%s)" % state

    def next_steps(self):
        if getattr(self, "needs_agent", False):
            from cs_smoke import smoke_template

            templates = self.cs.read_json(self.cs.KIT / "ops/bin/executor-templates.json", {})
            self.say("Next: install a coding agent.")
            for name, url in (
                ("claude", "https://claude.com/claude-code"),
                ("codex", "https://developers.openai.com/codex"),
                ("cursor-agent", "https://cursor.com/cli"),
                ("gemini", "https://github.com/google-gemini/gemini-cli"),
            ):
                template = smoke_template(templates, AGENTS, name)
                if isinstance(templates.get(template, {}).get("unavailable_reason"), str):
                    continue
                self.say("%s: %s · sign in: %s" % (LABELS[name], url, sign_in_command(name)))
            self.say("Then run: cs init")
            return
        if not getattr(self.args, "no_first_job", False) and getattr(self, "first_job_rc", 0) != 0:
            marker = self.marker_data()
            first = marker.get("first_job") or {}
            if first.get("state") not in (None, "done"):
                sentence = self.first_job_failure_sentence(first)
                self.say("Almost ready: %s." % (sentence[:1].upper() + sentence[1:]))
                if first.get("state") == "invalid_reply" or first.get("job"):
                    self.say("Next: run cs doctor, then cs init --first-job.")
                else:
                    self.say("Next: sign in to a coding agent, then run cs init --first-job.")
            else:
                self.say("Almost ready: the first job did not finish.")
                self.say("Next: run cs doctor, then cs init --first-job.")
            self.detail("next: cs status · cs jobs · cs doctor")
            return
        import cs_ui
        from urllib.parse import quote
        url = self.url.rstrip("/") + "/ui/"
        try:
            self.prepare_welcome()
        except Exception as error:
            self.say("Welcome: %s. Run cs init again to retry." % error)
        try:
            url = cs_ui.issue_url(self.cs)
            welcome = self.marker_data().get("welcome") or {}
            if welcome.get("conversation"):
                url += "&conversation=" + quote(welcome["conversation"], safe="")
            opened = False
            if not (self.args.yes or self.args.no_open or not sys.stdin.isatty()):
                opened = cs_ui.open_browser(url, self.cs.PLATFORM)
        except Exception as error:
            self.say("Open ContextStack: %s (%s)" % (self.url.rstrip("/") + "/ui/", error))
            return
        rec = self.nr.load_record()
        if rec.get("serve_client_web"):
            self.say("Also available: client-web at http://" + rec["serve_client_web"])
        self.detail("next: cs status · cs jobs · cs doctor")
        self.detail("Director API: " + self.url)
        self.detail("Owner credential: " + str(self.data / "local-credential"))
        self.say("ContextStack is ready.")
        self.say("Opened your first conversation in the browser." if opened else
                 "Open your first conversation: %s (works once, expires in 5 minutes)" % url)
        self.say("Later: cs ui opens it again · cs lead create sets up the hosted lead · cs doctor checks for problems")

    def prepare_welcome(self):
        import urllib.request
        import uuid
        marker = self.marker_data()
        first = marker.get("first_job") or {}
        if first.get("state") != "done" or self.args.no_first_job:
            return
        welcome = marker.get("welcome") or {"key": str(uuid.uuid4())}
        if not marker.get("welcome"):
            self.write_marker({**marker, "welcome": welcome})
        credential = (self.data / "local-credential").read_text().strip()

        def post(path, data, key):
            req = urllib.request.Request(self.url + "/api/v1/client/" + path,
                data=json.dumps(data).encode(), headers={"Authorization": "Bearer " + credential,
                "Content-Type": "application/json", "Idempotency-Key": key}, method="POST")
            with urllib.request.urlopen(req, timeout=30) as response:
                return json.load(response)

        if not welcome.get("conversation"):
            model = first.get("model") or AGENTS[first["agent"]]
            created = post("conversations", {"node": marker["node"], "model": model,
                "title": "Welcome to ContextStack"}, welcome["key"])
            welcome["conversation"] = created["conversation"]["conversation_id"]
            self.write_marker({**self.marker_data(), "welcome": welcome})
        if welcome.get("turn"):
            return
        path = "conversations/" + welcome["conversation"]
        if not welcome.get("turn_id"):
            attempt = welcome.get("attempt", 0)
            key = welcome["key"] + "-turn" + ("-%d" % attempt if attempt else "")
            created = post(path + "/turns",
                 {"message": "Welcome me to ContextStack briefly, then ask what I would like to do."}, key)
            welcome["turn_id"] = created["turn"]["turn_id"]
            self.write_marker({**self.marker_data(), "welcome": welcome})
        end = time.monotonic() + self.args.timeout
        while True:
            req = urllib.request.Request(self.url + "/api/v1/client/" + path + "/thread",
                headers={"Authorization": "Bearer " + credential})
            with urllib.request.urlopen(req, timeout=30) as response:
                thread = json.load(response)
            turn = next((t for t in thread["turns"] if t["turn_id"] == welcome["turn_id"]), {})
            state = turn.get("state")
            if state == "done" and (turn.get("reply") or "").strip():
                welcome["turn"] = True
                self.write_marker({**self.marker_data(), "welcome": welcome})
                return
            if state in ("done", "error", "stopped"):
                welcome.pop("turn_id")
                welcome["attempt"] = welcome.get("attempt", 0) + 1
                self.write_marker({**self.marker_data(), "welcome": welcome})
                raise RuntimeError("welcome reply ended %s%s" % (state,
                    ": " + turn["detail"] if turn.get("detail") else " without a reply"))
            if time.monotonic() >= end:
                raise RuntimeError("welcome reply timed out while %s" % (state or "waiting"))
            time.sleep(1)


def main(cs, args):
    cs.need_install()
    refusal = _install_setup.init_setup_refusal()
    if refusal:
        summary, alternative = refusal
        raise cs.Stop("cs init: %s %s" % (summary, alternative), 2)
    cs.need_launchd()  # the services are launchd agents; Linux (systemd --user) is install stage 1
    init = Init(cs, args)
    init.detail("cs init: %s, release %s" % (cs.ROOT, os.readlink(cs.CURRENT).rsplit("/", 1)[-1]))
    with init.stage("Checking disk encryption"):
        init.step_disk()
    with init.stage("Checking agents"):
        agents = init.step_agents()
    init.step_toolchains()
    init.step_cli_pins()
    with init.stage("Starting services"):
        name = init.step_services()
    init.require_services()
    init.say("Services: Director at %s, gateway, node %s" % (init.url, name))
    rc = init.step_first_job(name, agents)
    init.step_keys()
    init.next_steps()
    repos = Path(os.environ.get("CS_REPOS_FILE") or cs.CONFIG / "repos.json")
    if not repos.exists() or not json.loads(repos.read_text(encoding="utf-8")):
        print("Add a repository: cs repo add ~/path/to/project")
    return 0 if args.install_setup else rc
