"""Process and configuration: startup refusals with a visible reason,
signals, invalid configuration, missing models.json."""
import signal
import subprocess

from harness.gateway import model_call
from harness.run import case, eq, sub


def refuses(t, name, needle, config=None, **kw):
    code, log = t.gateway(name, config, expect_start=False, **kw).start_fails()
    eq(code != 0, True, name + " exit status nonzero")
    eq(needle in log, True, "%s: log names the reason %r (log tail: %s)" % (name, needle, log[-300:]))


@case("process", gw=None)
def auth_token_required(t):
    """Auth is on by default: no token and no explicit opt-out refuses startup."""
    refuses(t, "no-token", "CSGATEWAY_AUTH_TOKEN is required", env={"CSGATEWAY_AUTH_REQUIRED": None})


@case("process", gw=None)
def auth_disabled_warns(t):
    """CSGATEWAY_AUTH_REQUIRED=false without a token starts, logs a warning, and serves without auth."""
    gw = t.gateway("no-auth")
    eq("WARNING: gateway auth is disabled" in gw.log_text(), True, "warning logged")
    eq(gw.get("/v1/models", auth=False).status, 200, "open")


@case("process", gw=None)
def invalid_deployments(t):
    """Invalid admission configuration refuses startup: endpoint differing from the adapter, duplicate model, share without estate, bad JSON."""
    r = t.run
    refuses(t, "dep-endpoint", "deployment provider vllm endpoint differs from configured adapter",
            r.config(deployments=[dict(r.vllm_deployment(), endpoint="http://127.0.0.1:1/v1")]))
    refuses(t, "dep-dup", "duplicate or empty deployment model: vllm/local-coder",
            r.config(deployments=[r.vllm_deployment("a", models=["local-coder"]), {"id": "b", "provider": "vllm", "models": ["local-coder"], "max_concurrent": 1}]))
    refuses(t, "dep-share", 'deployment vllm-spec has no capacity for estate ""',
            r.config(deployments=[r.vllm_deployment(max_concurrent=4, capacity={"other": 2})]))
    refuses(t, "dep-queue", "invalid deployment limit: vllm-spec", r.config(deployments=[r.vllm_deployment(max_queued=2, max_wait_ms=0)]))
    refuses(t, "dep-unconfigured", "deployment provider nosuch is not configured",
            r.config(deployments=[{"id": "n", "provider": "nosuch", "endpoint": "https://nosuch.invalid/v1", "models": ["x-spec"], "max_concurrent": 1}]))
    gw = t.gateway("bad-json", r.config(), expect_start=False)
    gw.models_path.write_text("{not json")
    code, log = gw.start_fails()
    eq((code != 0, "config: decode deployment config" in log), (True, True), "bad JSON")


@case("process", gw=None)
def keyless_host_paid_deployments(t):
    """With no credential store (CSGATEWAY_CREDENTIALS=off, a node), a deployment for a keyed provider does not stop
    startup: the provider is simply not available, so one models.json serves hosts with and without keys."""
    r = t.run
    gw = t.gateway("keyless", r.config(deployments=[{"id": "o", "provider": "openai", "endpoint": "https://api.openai.com/v1",
                                                     "models": ["gpt-spec"], "max_concurrent": 1}]))
    eq(gw.get("/v1/models", auth=False).status, 200, "starts and serves")


@case("process", gw=None)
def invalid_executables(t):
    """models.json executables must be bare names mapped to absolute clean paths."""
    r = t.run
    refuses(t, "exe-rel", "executables: claude must be an absolute clean path", r.config(executables={"claude": "bin/claude"}))
    refuses(t, "exe-name", 'executables: "a/b" is not a bare name', r.config(executables={"a/b": "/bin/sh"}))


@case("process", gw=None)
def cli_executor_validated(t):
    """cli_executor must be an absolute path to a regular file."""
    for name, value, needle in (("code_root-rel", "fake_cli.py", "CLI executor path must be absolute"),
                                ("code_root-missing", str(t.run.tmp / "missing.py"), "no such file or directory"),
                                ("code_root-dir", str(t.run.tmp), "CLI executor must be a regular file")):
        c = t.run.config()
        c["cli_executor"] = value
        refuses(t, name, needle, c)


@case("process", gw=None)
def missing_models_json(t):
    """No models.json: the gateway starts with only discovered models; named configured models are unknown."""
    gw = t.gateway("no-models", env={"CSAGENT_MODELS_CONFIG": str(t.run.tmp / "absent.json")})
    # CLI rows are excluded: without models.json executables, CLI lookup falls back to the host's search path.
    names = sorted(m["name"] for m in gw.get("/v1/models").json["models"] if m.get("executor") != "cli" and m["provider"] == "vllm")
    eq(names, ["fake-paid", "fake-sub", "local-coder", "vllm-big", "vllm-extra"], "only discovered vLLM models (tier 2) and nothing configured")
    eq([m["name"] for m in gw.get("/v1/models").json["models"] if m["provider"] == "ollama"], ["ollama-small"], "discovered Ollama model")
    sub(gw.call(model_call("mock-echo")).json, {"error": {"code": "gateway.unknown_model"}}, "configured-only model is unknown")


@case("process", gw=None)
def sigterm_clean_exit(t):
    """SIGTERM shuts down cleanly with exit status 0; SIGINT likewise."""
    for sig in (signal.SIGTERM, signal.SIGINT):
        gw = t.gateway("sig-%d" % sig)
        eq(gw.stop(sig), 0, "exit status after %s" % sig.name)


@case("process", gw=None)
def port_in_use(t):
    """A TCP port already bound refuses startup (nonzero exit)."""
    first = t.gateway("port-a")
    second = t.gateway("port-b", expect_start=False)
    second.env["CSGATEWAY_PORT"] = str(first.port)
    code, log = second.start_fails()
    eq((code != 0, "address already in use" in log), (True, True), "port in use")


@case("process", gw=None)
def help_starts_nothing(t):
    """--help and --version print and exit 0 before anything starts: no discovery, no listener, no log line; an
    unknown argument prints the usage and exits 2."""
    gw = t.gateway("help", expect_start=False)
    for argv, code, stream in ((["--help"], 0, "stdout"), (["--version"], 0, "stdout"), (["--nosuch"], 2, "stderr")):
        p = subprocess.run([t.run.binary] + argv, cwd=gw.dir, env=gw.env, capture_output=True, text=True, timeout=10)
        eq(p.returncode, code, "%s exit status" % argv[0])
        eq(("usage:" if argv[0] != "--version" else "csgateway") in getattr(p, stream), True, "%s prints on %s" % (argv[0], stream))
        eq("discovery" in (p.stdout + p.stderr).lower(), False, "%s ran no discovery" % argv[0])
    eq(sorted(str(p.relative_to(gw.dir)) for p in gw.dir.rglob("*") if p.is_socket() or p.name.endswith((".sock", ".log"))),
       [], "no socket or log written")
