"""Hermetic harness + gateway runs for trace acceptance scenarios."""
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from hermetic_env import hermetic_env

HERE = Path(__file__).resolve().parent
CS = HERE.parents[2]
REPO = CS.parent


def wait_for(predicate, procs, seconds=25):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if any(p.poll() is not None for p in procs):
            raise AssertionError("fixture process exited")
        try:
            if predicate():
                return
        except OSError:
            pass
        time.sleep(0.02)
    raise AssertionError("fixture readiness timed out")


def gateway_bin():
    path = Path(os.environ.get("CSGATEWAY_BIN") or REPO / "target/release/cs-gateway")
    if not path.is_file():
        raise SystemExit("trace-e2e: build cs-gateway (cargo build --release -p cs-gateway)")
    return path


def runner_bin():
    sys.path.insert(0, str(CS / "ops/bin"))
    from _runner import built_runner
    return built_runner(CS)


def jsonl(path):
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n")


def box_overlay(upstream):
    return {
        "schema": "csgateway.providers.v1",
        "providers": [{
            "name": "spec-box",
            "protocol": "openai_chat_completions",
            "endpoint": upstream + "/v1",
            "host": "box-1",
            "capabilities": {"images": False, "tools": True, "streaming": False},
        }],
    }


class Stack:
    """One scratch root: provider, gateway, estate."""

    def __init__(self, scratch, scenario, *, box=False, no_gateway=False):
        self.scratch = Path(scratch)
        self.scenario = scenario
        self.box = box
        self.no_gateway = no_gateway
        # Start from the shared fixture policy, then allow only the host's tool search path.
        self.env = {
            "PATH": hermetic_env().get("PATH", os.defpath),
            "HOME": str(self.scratch),
            "TMPDIR": str(self.scratch),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
        self.procs = []
        self.transport = None
        self.gw_home = self.scratch / "gw-home"
        self.estate = self.scratch / "estate"
        self.gw_endpoint = None
        self.cli_state = self.scratch / "cli-state"

    def __enter__(self):
        try:
            self._start()
        except BaseException:
            self.__exit__()
            raise
        return self

    def __exit__(self, *exc):
        if self.transport:
            self.transport.shutdown()
            self.transport.server_close()
        for proc in reversed(self.procs):
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()

    def _start(self):
        self.gw_home.mkdir(parents=True)
        (self.gw_home / "tmp").mkdir()
        ready = self.scratch / "provider-ready"
        if not self.no_gateway:
            provider = subprocess.Popen(
                [sys.executable, str(HERE / "provider.py"), "--ready", str(ready), "--scenario", self.scenario],
                env=self.env,
            )
            self.procs.append(provider)
            wait_for(ready.exists, self.procs)
            upstream = ready.read_text().strip()
        else:
            upstream = "http://127.0.0.1:1"

        cfg = json.loads((HERE / "models.json").read_text())
        cfg["deployments"][0]["endpoint"] = upstream + "/v1"
        if self.box:
            ep = upstream + "/v1"
            cfg["deployments"] = [{
                "id": "trace-box", "provider": "spec-box", "endpoint": ep,
                "models": ["box-model"], "max_concurrent": 2, "max_queued": 0,
                "max_wait_ms": 0, "capacity": {"dev": 2},
            }]
            cfg["models"] = [m for m in cfg["models"] if m["name"] == "box-model"]
        if self.scenario in {"U", "X", "action-cli-no-go"}:
            cfg["cli_executor"] = str(self.estate / "ops/bin/_executor.py")
            cfg["deployments"].append({"id": "trace-cli", "provider": "cli", "models": ["trace-cli"],
                                       "max_concurrent": 1, "capacity": {"dev": 1}})
        models_path = self.scratch / "models.json"
        write_json(models_path, cfg)

        (self.gw_home / ".contextstack/config/gateway").mkdir(parents=True)
        (self.gw_home / ".contextstack/config/gateway/limits.json").write_text('{"api_usd_per_day": 1000}')

        dead = "http://127.0.0.1:1"

        child_env = {
            **self.env,
            "HOME": str(self.gw_home),
            "USER": "fixture",
            "PATH": os.pathsep.join(["/usr/bin", "/bin", str(Path(sys.executable).parent)]),
            "TMPDIR": str(self.gw_home / "tmp"),
            "LANG": "C",
            "CSGATEWAY_CREDENTIALS": "off",
            "CSAGENT_MODELS_CONFIG": str(models_path),
            "CSGATEWAY_AUTH_REQUIRED": "false",
            "CSGATEWAY_DISCOVERY_HEALTH": "off",
            "VLLM_ENDPOINT": upstream,
            "OLLAMA_ENDPOINT": dead,
            "HTTP_PROXY": dead,
            "HTTPS_PROXY": dead,
            "NO_PROXY": "127.0.0.1",
            "http_proxy": dead,
            "https_proxy": dead,
        }
        if self.box:
            overlay_dir = self.scratch / "providers"
            overlay_dir.mkdir()
            write_json(overlay_dir / "box.json", box_overlay(upstream))
            child_env["CSGATEWAY_PROVIDERS_DIR"] = str(overlay_dir)

        if self.no_gateway:
            self.gw_endpoint = "http://127.0.0.1:1/v1/agent/model.call"
            self._build_estate(child_env)
            return

        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        child_env["CSGATEWAY_PORT"] = str(port)
        if self.scenario in {"U", "X", "action-cli-no-go"}:
            self._build_estate(child_env)
            binaries = self.scratch / "bin"
            binaries.mkdir(exist_ok=True)
            sandbox_exec = binaries / "sandbox-exec"
            sandbox_exec.write_text('#!/bin/sh\n[ "$1" = -f ] || exit 2\nshift 2\nexec "$@"\n')
            sandbox_exec.chmod(0o755)
            fixtures = self.estate / "home"
            (fixtures / ".gitconfig").write_text("[user]\nname = Trace Fixture\nemail = trace@example.invalid\n")
            self.cli_state.mkdir()
            child_env.update(HOME=str(fixtures), CS_CONFIG_ROOT=str(self.estate), CS_STATE_ROOT=str(self.cli_state),
                             PATH=str(binaries) + os.pathsep + child_env["PATH"])
        gw_bin = self.scratch / "cs-gateway"
        shutil.copy2(gateway_bin(), gw_bin)
        with (self.scratch / "gateway.log").open("ab") as log:
            server = subprocess.Popen([str(gw_bin)], cwd=self.scratch, env=child_env, stdout=log, stderr=log)
        self.procs.append(server)
        self.gw_endpoint = "http://127.0.0.1:%d/v1/agent/model.call" % port
        wait_for(self._health, self.procs, seconds=15)
        if self.scenario in {"J", "L", "O"}:
            import transport
            self.transport = transport.start(port, self.scenario)
            self.gw_endpoint = f"http://127.0.0.1:{self.transport.server_port}/v1/agent/model.call"
        if self.scenario in {"U", "X", "action-cli-no-go"}:
            self.child_env.update(PATH=child_env["PATH"], CSGATEWAY_ENDPOINT=self.gw_endpoint,
                                  CS_STATE_ROOT=str(self.cli_state),
                                  CSGATEWAY_CLI_ENDPOINT=self.gw_endpoint.replace("model.call", "cli.run"))
        else:
            self._build_estate(child_env)

    def _health(self):
        base = self.gw_endpoint.rsplit("/v1/", 1)[0]
        try:
            with urllib.request.urlopen(base + "/v1/health", timeout=2):
                return True
        except OSError:
            return False

    def _build_estate(self, base_env):
        e = self.estate
        for d in ("ops/bin", "ledger", "specs", "prompts", "tasks/fx", "tools/bin", "home", "tmp"):
            (e / d).mkdir(parents=True, exist_ok=True)
        for src in (CS / "ops/bin").iterdir():
            if src.is_file():
                shutil.copy2(src, e / "ops/bin" / src.name)
        shutil.copy2(HERE / "fixture/fx_tool", e / "tools/bin/fx_tool")
        shutil.copy2(HERE / "fixture/fx_tool.json", e / "tools/bin/fx_tool.json")
        (e / "tools/bin/fx_tool").chmod(0o755)
        (e / "ESTATE").write_text("dev\n")
        (e / "INVARIANTS.md").write_text("trace-e2e fixture\n")
        (e / "prompts/coder-min.md").write_text("Reply briefly.\n")
        (e / "tasks/fx/goal.md").write_text("Respond and finish.\n")
        (e / "tasks/fx/check").write_text("#!/bin/sh\nexit 0\n")
        (e / "tasks/fx/check").chmod(0o755)
        subprocess.run(["git", "init", "-q", str(e)], env=self.env, check=True)
        subprocess.run(["git", "-C", str(e), "-c", "user.name=Fixture", "-c", "user.email=f@example.invalid",
                        "commit", "--allow-empty", "-qm", "fixture"], check=True, env=self.env)
        proxy_keys = {k for k in base_env if k.lower() in {"http_proxy", "https_proxy", "all_proxy"}}
        self.child_env = {
            **{k: v for k, v in base_env.items() if k not in proxy_keys},
            "HOME": str(e / "home"),
            "TMPDIR": str(e / "tmp"),
            "LANG": "en_US.UTF-8",
            **estate_vars(CODE_ROOT=e, CONFIG_ROOT=e, STATE_ROOT=e / "ledger"),
            "CSGATEWAY_ENDPOINT": self.gw_endpoint,
            "CS_SANDBOX_BIN": str(REPO / "target/release/cs-sandbox"),
        }
        from native_runtime import stage_native_runtime
        stage_native_runtime(e / "ops/bin")

    def run_spec(self, spec_yaml, *, job_id=None, argv=()):
        spec_path = self.estate / "specs" / "case.yaml"
        spec_path.write_text(spec_yaml)
        env = dict(self.child_env, CS_RUNNER_BIN=str(runner_bin()))
        if job_id:
            env.update(estate_vars(JOB_ID=job_id))
        runs_dir = self.estate / "ledger/runs"
        before = set(runs_dir.glob("*"))
        cmd = [str(self.estate / "ops/bin/run-api"), *argv, "specs/case.yaml"]
        proc = subprocess.run(cmd, cwd=self.estate, env=env, text=True, capture_output=True, timeout=30)
        runs = set(runs_dir.glob("*")) - before
        if len(runs) != 1:
            raise AssertionError(f"expected one new run: {runs}; {proc.stdout} {proc.stderr}")
        run_dir = runs.pop()
        if not (run_dir / "finalized.json").is_file():
            raise AssertionError(f"finalize-run did not finish: {proc.stdout} {proc.stderr}")
        return run_dir

    def run_cli(self, *, cancel=False, action=None):
        from shlex import quote
        # Replay three native stream lines, including the CLI's own usage receipt.
        source = CS / "tasks/regressions/agent-usage/fixtures/claude-stream.jsonl"
        lines = source.read_text().splitlines()
        binary = self.estate / "tools/bin/claude"
        if action is not None:
            from action_approval import fake_cli_source
            payload = fake_cli_source(action)
            for line in (lines[0], lines[2], lines[-1]):
                payload += f"\nprint({line!r}, flush=True)"
            binary.write_text(f"#!{sys.executable}\n" + payload + "\n")
        elif cancel:
            payload = """
import os, subprocess, sys, time
from pathlib import Path
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(300)"])
Path("agent-pids").write_text(f"{os.getpid()} {child.pid}")
print('{"type":"assistant","message":{"content":[{"type":"text","text":"live before cancel"}]}}', flush=True)
time.sleep(300)
"""
            binary.write_text(f"#!{sys.executable}\n" + payload)
        else:
            binary.write_text("#!/bin/sh\nprintf '%s\\n' "
                              + " ".join(quote(line) for line in (lines[0], lines[2], lines[-1])) + "\n")
        binary.chmod(0o755)
        write_json(self.estate / "ops/bin/executor-templates.json", {"trace-cli": {
            "binary": str(binary), "args": ["--print", "--output-format", "stream-json",
                                            "--verbose", "{{prompt}}"],
            "cost_class": "subscription", "auth": "subscription", "cost_usd": 0,
            "cost_per_1k": 0.01,
        }})
        order = {"executor": "trace-cli", "task": "tasks/fx", "exp": "trace-cli",
                 "trial": 1, "timeout_s": 20}
        runner = runner_bin()
        state = self.cli_state
        job = "trace-cancel" if cancel else "trace-cli"
        env = {**self.child_env, "CS_RUNNER_BIN": str(runner), "CS_JOB_ID": job}
        (state / "dispatch.jsonl").write_text(json.dumps({"id": job, "state": "running",
                                                         "task": order["task"]}) + "\n")
        launcher = str(self.estate / "ops/bin/executor")
        if cancel:
            marker = state / "node/cancelled/trace-before-start"
            marker.parent.mkdir(parents=True)
            marker.touch()
            skipped = subprocess.run([launcher], cwd=self.estate, env={**env, "CS_JOB_ID": marker.name},
                                     input=json.dumps(order), text=True, capture_output=True, timeout=5)
            assert skipped.returncode == 1 and not (state / "runs").exists(), skipped
        with subprocess.Popen([launcher], cwd=self.estate,
                              env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True) as proc:
            proc.stdin.write(json.dumps(order))
            proc.stdin.close()
            proc.stdin = None
            try:
                if cancel:
                    from _job_identity import _pid_identity
                    from _node_events import live_runs, send

                    started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 5))
                    active = [{"job_id": job, "state": "running", "started_at": started}]
                    forwarded = []

                    class Director:
                        protocol = 2

                        def call(self, method, path, body):
                            forwarded.extend(body["events"])
                            return {"confirmed": max(event["seq"] for event in body["events"])}

                    def live():
                        for run, _ in live_runs(state, active).values():
                            send(Director(), state, run, job)
                            if any(e["payload"].get("text") == "live before cancel" for e in forwarded):
                                return run
                        return None

                    wait_for(live, [proc, *self.procs])
                    run = next((state / "runs").iterdir())
                    pids = [int(pid) for pid in (run / "workspace/agent-pids").read_text().split()]
                    identities = {pid: _pid_identity(pid) for pid in pids}
                    assert len(identities) == 2 and all(identities.values()), identities
                    assert not (run / "terminal.json").exists()
                    proc.send_signal(signal.SIGTERM)
                out, err = proc.communicate(timeout=30)
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()
        runs = list((state / "runs").iterdir())
        assert proc.returncode == int(cancel), (proc.returncode, out, err,
                                               [(run / "cli.log").read_text() for run in runs
                                                if (run / "cli.log").is_file()])
        assert len(runs) == 1 and (runs[0] / "finalized.json").is_file(), (runs, out, err)
        run = runs[0]
        check = json.loads((run / "check.json").read_text())
        assert check["verdict"] == ("fail" if cancel else "pass"), check
        if cancel:
            wait_for(lambda: all(_pid_identity(pid) != birth for pid, birth in identities.items()),
                     self.procs, seconds=5)
            log = (self.scratch / "gateway.log").read_text()
            assert f"POST /v1/agent/cli.run/cli-{run.name}/cancel 200" in log, log
            terminal = json.loads((run / "terminal.json").read_text())
            assert terminal["terminal_state"] == "cancelled" and terminal["cancel_reason"] == "SIGTERM"
            assert check["skipped"]
            rows = jsonl(state / "ledger.jsonl")
            assert len(rows) == 1 and rows[0]["stop"] == "cancelled", rows
            assert len(list((state / "outbox").glob("*.json"))) == 1
        return run

    def merge_trace(self, run_dir, *, exchanges=0):
        run_id = run_dir.name
        harness = jsonl(run_dir / "trace.jsonl")
        gw_file = self.gw_home / ".contextstack/gateway/traces" / (run_id + ".jsonl")
        if exchanges:
            def exchanges_written():
                return sum(s["kind"] == "gateway_exchange" for s in jsonl(gw_file)) >= exchanges

            try:
                wait_for(exchanges_written, self.procs, seconds=6)
            except AssertionError as exc:
                raise AssertionError(f"expected {exchanges} gateway exchanges for {run_id}; "
                                     f"wrote {len(jsonl(gw_file))} gateway spans") from exc
        gateway = jsonl(gw_file)
        if self.scenario == "O":
            # Deliver the late send 2 records before the send 1 records to the checker.
            gateway.reverse()
        return harness + gateway, run_id


def estate_vars(**values):
    """Pin both estate environment spellings to the same fixture values."""
    return {f"{prefix}_{key}": str(value)
            for prefix in ("CS", "LAB") for key, value in values.items()}


def drop_branch(spans, span_id):
    removed = {span_id}
    while True:
        children = {s["span_id"] for s in spans if s["parent_id"] in removed}
        if children <= removed:
            return [s for s in spans if s["span_id"] not in removed]
        removed.update(children)
