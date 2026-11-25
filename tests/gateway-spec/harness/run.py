"""Suite run: one set of mocks and one egress proxy, shared gateways per
configuration, the case registry, dispositions and the result table."""
import json
import shutil
import sys
import tempfile
import time
import traceback
from pathlib import Path

from . import upstreams
from .gateway import SPEC, FIXTURES, Gateway, python_exe

INTERCEPT = ["api.anthropic.com", "api.openai.com"]

# Discovery lists the mocks serve by default (cases may change them).
VLLM_MODELS = ["local-coder", "vllm-big", "fake-paid", "fake-sub", "vllm-extra"]
OLLAMA_MODELS = ["ollama-small"]
ANTHROPIC_MODELS = ["claude-spec"]
OPENAI_MODELS = ["gpt-spec"]


# CLI agents the gateway knows only by their inventory entry (not auto-discovered): the binary picks the dialect.
CURSOR_CLIS = ["cursor-agent", "cursor-sol", "cursor-codex", "cursor-composer", "cursor-grok", "cursor-sonnet"]
AGENT_CLIS = [{"name": name, "provider": "cursor", "tier": 3, "capability": "coding", "executor": "cli",
               "cli_binary": "cursor-agent"} for name in CURSOR_CLIS] + [
              {"name": "antigravity", "provider": "gemini", "tier": 3, "capability": "coding", "executor": "cli",
               "cli_binary": "agy"},
              {"name": "antigravity-hydrogen", "provider": "gemini", "tier": 3, "capability": "coding", "executor": "cli",
               "cli_binary": "agy"}]


class Run:
    def __init__(self, binary, impl, profile, keep=False):
        self.binary, self.impl, self.profile, self.keep = binary, impl, profile, keep
        import ssl
        if not getattr(ssl, "HAS_TLSv1_3", False):
            # The intercept's TLS server needs a harness Python on OpenSSL >= 1.1.1 (macOS's
            # /usr/bin/python3 is LibreSSL 2.8 and fails the handshake with Go 1.27): block, don't fail.
            caps = dict(profile.get("capabilities", {}), tls_intercept=False)
            self.profile = profile = dict(profile, capabilities=caps)
            print("note: %s has no TLS 1.3 (%s); tls_intercept cases are blocked" % (sys.executable, ssl.OPENSSL_VERSION))
        self.tmp = Path(tempfile.mkdtemp(prefix="modelgateway-spec-"))
        self.tls = upstreams.TLSMaterial(self.tmp / "tls", INTERCEPT)
        self.vllm = upstreams.openai_compat("vllm")
        self.ollama = upstreams.openai_compat("ollama")
        self.anthropic = upstreams.anthropic()
        self.openai = upstreams.openai_compat("openai")
        self.listeners = [upstreams.Listener(self.vllm), upstreams.Listener(self.ollama)]
        self.vllm_url, self.ollama_url = self.listeners[0].url, self.listeners[1].url
        self.proxy = upstreams.EgressProxy(self.tls, {"api.anthropic.com": self.anthropic, "api.openai.com": self.openai})
        self.fake_cli = FIXTURES / "fake_cli.py"
        self.fake_claude = self.tmp / "bin" / "claude"
        self.fake_claude.parent.mkdir()
        self.fake_claude.write_text("#!/bin/sh\necho fake claude\n")
        self.fake_claude.chmod(0o755)
        self.shared = {}
        self.dropped_disables = []
        self.reset()
        self.probe_disables()

    def probe_disables(self):
        """The disable list guards compiled-in endpoints a mock cannot
        stand in for (vllm-spark2 at 127.0.0.1:8001). A binary that no
        longer compiles one in refuses to disable it by name; then there
        is nothing to guard, so the name is dropped and recorded."""
        while self.profile.get("disable_endpoints"):
            gw = Gateway(self, "probe", self.config(), expect_start=False)
            gw.spawn()
            try:
                gw.wait(lambda: gw.proc.poll() is not None or gw.get("/v1/health").status == 200, timeout=15, what="probe start")
                log = gw.log_text()
            finally:
                gw.cleanup()
            names = [n for n in self.profile["disable_endpoints"] if 'unknown endpoint "%s"' % n in log]
            if not names:
                return
            self.dropped_disables += names
            self.profile = dict(self.profile, disable_endpoints=[n for n in self.profile["disable_endpoints"] if n not in names])

    # -- environment the implementation runs in
    def base_env(self, gw):
        tmp = gw.dir / "tmp"
        tmp.mkdir(exist_ok=True)
        env = {
            "HOME": str(gw.home), "USER": "spec", "LOGNAME": "spec", "LANG": "C",
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "TMPDIR": str(tmp),
            "CSGATEWAY_AUTH_REQUIRED": "false",
            "CSGATEWAY_CREDENTIALS": "off",          # never the host Keychain or store
            "CSGATEWAY_DISCOVERY_HEALTH": "off",
            "HTTP_PROXY": self.proxy.url, "HTTPS_PROXY": self.proxy.url,
            "http_proxy": self.proxy.url, "https_proxy": self.proxy.url,
            "NO_PROXY": "", "no_proxy": "",
        }
        for var in self.profile.get("ca_env", []):
            env[var] = str(self.tls.ca)
        env.update(self.profile.get("env", {}))
        up = self.profile["upstream_env"]
        env[up["ollama"]] = self.ollama_url
        env[up["vllm"]] = self.vllm_url
        disabled = self.profile.get("disable_endpoints") or []
        if disabled:
            env["CSGATEWAY_DISCOVERY_DISABLED"] = ",".join(disabled)
        return env

    def config(self, deployments=None, cli_executor=False, models=None, extra_models=(), **top):
        """The suite's models.json. Model names are the routing fixtures every
        area shares; deployments and the CLI executor are opt-in because any
        deployment turns admission on for every provider."""
        c = {
            "models": models if models is not None else [
                {"name": "local-coder", "provider": "vllm", "tier": 1, "capability": "coding", "preferred": True},
                {"name": "vllm-big", "provider": "vllm", "tier": 2, "capability": "coding"},
                {"name": "fake-paid", "provider": "vllm", "tier": 3, "capability": "general", "cost_per_1k": 0.01, "cost_class": "api"},
                {"name": "fake-sub", "provider": "vllm", "tier": 3, "capability": "general", "cost_per_1k": 0.02, "cost_class": "subscription"},
                {"name": "vllm-absent", "provider": "vllm", "tier": 2, "capability": "coding"},
                {"name": "ollama-small", "provider": "ollama", "tier": 2, "capability": "general"},
                {"name": "gpt-spec", "provider": "openai", "tier": 1, "capability": "coding", "cost_per_1k": 0.015},
                {"name": "claude-spec", "provider": "anthropic", "tier": 1, "capability": "coding", "cost_per_1k": 0.015},
                {"name": "claude-code", "provider": "anthropic", "tier": 1, "capability": "coding", "executor": "cli", "cli_binary": "claude",
                 "picker": {"provider_label": "Anthropic", "label": "Claude Code, uses its configured model",
                            "efforts": ["low", "medium", "high"]}},
                {"name": "mock-echo", "provider": "mock", "tier": 3, "capability": "general"},
            ] + list(extra_models),
            # Pin every executable the gateway may look up: nothing from the host.
            "executables": {"claude": str(self.fake_claude), "codex": str(self.tmp / "absent" / "codex"),
                            "gemini": str(self.tmp / "absent" / "gemini"), "cursor-agent": str(self.tmp / "absent" / "cursor-agent"),
                            "agy": str(self.tmp / "absent" / "agy"), "python3": python_exe()},
            "deployments": deployments or [],
        }
        if cli_executor:
            c["cli_executor"] = str(self.fake_cli)
        c.update(top)
        return c

    def vllm_deployment(self, id="vllm-spec", models=("local-coder", "vllm-big", "fake-paid", "fake-sub", "vllm-absent"), **kw):
        d = {"id": id, "provider": "vllm", "endpoint": self.vllm_url + "/v1", "models": list(models),
             "max_concurrent": 1, "max_queued": 0, "max_wait_ms": 0}
        d.update(kw)
        return d

    def gateway(self, name, config=None, **kw):
        return Gateway(self, name, config if config is not None else self.config(), **kw)

    # -- shared gateways, one per configuration, started lazily
    SHARED = {
        "base": lambda r: r.gateway("base"),
        "auth": lambda r: r.gateway("auth", auth_token="spec-token"),
        "admit": lambda r: r.gateway("admit", r.config(
            deployments=[r.vllm_deployment(max_concurrent=1, max_queued=1, max_wait_ms=5000),
                         {"id": "cli-spec", "provider": "cli", "models": ["fake-cli"], "max_concurrent": 1, "max_queued": 0, "max_wait_ms": 0}],
            cli_executor=True, estate="spec")),
        # Coding-agent work orders (live output, cancel): the fake executor stands in for each agent CLI.
        "agents": lambda r: r.gateway("agents", r.config(
            deployments=[{"id": "cli-agents", "provider": "cli",
                          "models": ["claude-code", "codex", "gemini-cli", *CURSOR_CLIS, "antigravity",
                                     "antigravity-hydrogen", "fake-cli"],
                          "max_concurrent": 4, "max_queued": 0, "max_wait_ms": 0}],
            cli_executor=True, extra_models=AGENT_CLIS), auth_token="spec-token"),
        "cloud": lambda r: r.gateway("cloud", env={"CSGATEWAY_CREDENTIALS": str(r.credential_store("cloud", anthropic="sk-ant-spec-FAKE", openai="sk-spec-FAKE"))}),
    }

    def shared_gateway(self, name):
        if name not in self.shared:
            self.shared[name] = self.SHARED[name](self)
        return self.shared[name]

    def credential_store(self, name, dir_mode=0o700, file_mode=0o600, **keys):
        d = self.tmp / "credentials" / name
        d.mkdir(parents=True, exist_ok=True)
        for provider, key in keys.items():
            p = d / provider
            p.write_text(key + "\n")
            p.chmod(file_mode)
        d.chmod(dir_mode)
        return d

    def reset(self):
        for m, models in ((self.vllm, VLLM_MODELS), (self.ollama, OLLAMA_MODELS), (self.openai, OPENAI_MODELS)):
            m.chat.clear()
            m.default_chat = {"json": upstreams.chat_completion()}
            m.discovery = None
            m.models = list(models) if m is not self.ollama else []
            m.tags = list(models) if m is self.ollama else []
        self.anthropic.models = list(ANTHROPIC_MODELS)
        self.anthropic.discovery = None
        self.anthropic.messages = {"json": upstreams.anthropic_message([{"type": "text", "text": "hello from anthropic"}])}
        for m in (self.vllm, self.ollama, self.anthropic, self.openai):
            for g in m.gates.values():
                g.set()
            m.gates.clear()
            m.requests.clear()
            m.events.clear()
        self.proxy.seen.clear()

    def calls(self, mock, path_suffix="/chat/completions"):
        return [r for r in mock.requests.items if r["path"].endswith(path_suffix) and r["method"] == "POST"]

    def close(self):
        for gw in self.shared.values():
            gw.cleanup()
        for m in (self.vllm, self.ollama, self.anthropic, self.openai):
            for g in m.gates.values():
                g.set()
        for l in self.listeners:
            l.close()
        self.proxy.close()
        if not self.keep:
            shutil.rmtree(self.tmp, ignore_errors=True)


# --- cases

CASES = []


class Case:
    def __init__(self, fn, area, gw, fast, needs):
        self.fn, self.area, self.gw, self.fast, self.needs = fn, area, gw, fast, tuple(needs)
        self.id = fn.__name__.replace("_", "-")
        self.doc = (fn.__doc__ or "").strip().splitlines()[0] if fn.__doc__ else ""


def case(area, gw="base", fast=True, needs=()):
    """Register a case. `gw` names a shared gateway (None: the case starts
    its own); `fast` puts it in the offline check subset; `needs` lists
    profile capabilities without which the case is blocked, not run."""
    def wrap(fn):
        CASES.append(Case(fn, area, gw, fast, needs))
        return fn
    return wrap


class T:
    """What a case sees: the shared gateway (if any), the run's mocks, and
    gateways it starts itself (stopped and removed afterwards)."""

    def __init__(self, run, gw):
        self.run, self.gw, self.owned = run, gw, []
        self.vllm, self.ollama, self.anthropic, self.openai, self.proxy = run.vllm, run.ollama, run.anthropic, run.openai, run.proxy

    def gateway(self, name, config=None, **kw):
        gw = Gateway(self.run, name, config if config is not None else self.run.config(), **kw)
        self.owned.append(gw)
        return gw

    def mock(self, name):
        """An extra OpenAI-compatible loopback upstream for this case only
        (a second machine behind an overlay endpoint); returns (mock, url)."""
        m = upstreams.openai_compat(name)
        listener = upstreams.Listener(m)
        self.owned_listeners = getattr(self, "owned_listeners", []) + [(m, listener)]
        return m, listener.url

    def done(self):
        for gw in self.owned:
            gw.cleanup()
        for m, listener in getattr(self, "owned_listeners", []):
            for g in m.gates.values():
                g.set()
            listener.close()
        for d in getattr(self, "owned_dirs", []):
            shutil.rmtree(d, ignore_errors=True)


def norm(body):
    """Documented normalisation: latency_ms is wall-clock (and omitted when 0)."""
    body = dict(body)
    body.pop("latency_ms", None)
    return body


def eq(actual, expected, what):
    if actual != expected:
        raise AssertionError("%s: expected %s, got %s" % (what, json.dumps(expected, sort_keys=True, default=str),
                                                          json.dumps(actual, sort_keys=True, default=str)))


def sub(actual, expected, what):
    """Every key in expected equals actual's (recursively for dicts)."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            raise AssertionError("%s: expected an object, got %r" % (what, actual))
        for k, v in expected.items():
            if k not in actual:
                raise AssertionError("%s.%s: missing (have %s)" % (what, k, sorted(actual)))
            sub(actual[k], v, "%s.%s" % (what, k))
    else:
        eq(actual, expected, what)


def absent(obj, keys, what):
    for k in keys:
        if k in obj:
            raise AssertionError("%s.%s: expected absent, got %r" % (what, k, obj[k]))


def no_egress(t, what="egress"):
    eq(t.proxy.refused(), [], what + " (destinations refused by the egress proxy)")


def run_cases(run, cases, dispositions, verbose=False):
    results = []
    for c in cases:
        missing = [n for n in c.needs if not run.profile.get("capabilities", {}).get(n)]
        disp = dispositions.get(c.id, {}).get(run.impl)
        started = time.monotonic()
        if missing:
            results.append({"id": c.id, "area": c.area, "result": "blocked", "detail": "profile lacks " + ", ".join(missing), "s": 0})
            continue
        run.reset()
        t = None
        try:
            gw = run.shared_gateway(c.gw) if c.gw else None
            t = T(run, gw)
            c.fn(t)
            result, detail = "pass", ""
        except Exception as e:  # noqa: BLE001 - every failure is a result
            result = "fail"
            detail = str(e) if isinstance(e, AssertionError) else traceback.format_exc(limit=3)
        finally:
            if t:
                t.done()
        if result == "fail" and disp:
            result = "dispositioned"
        elif result == "pass" and disp:
            result = "pass-stale-disposition"
        r = {"id": c.id, "area": c.area, "result": result, "detail": detail, "s": round(time.monotonic() - started, 2)}
        if disp:
            r["disposition"] = disp
        results.append(r)
        if verbose or result not in ("pass",):
            print("%-24s %-26s %-14s %5.2fs %s" % (c.area, c.id, result, r["s"], detail.splitlines()[0] if detail else ""), flush=True)
    return results


def load_dispositions():
    p = SPEC / "dispositions.json"
    if not p.is_file():
        return {}
    return {k: v for k, v in json.loads(p.read_text()).items() if not k.startswith("_")}
