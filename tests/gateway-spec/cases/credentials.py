"""Credential store modes with temp file stores and fake keys:
off, a private file store, a refused (too open) store, provider narrowing,
*_API_KEY never honoured from the environment, the operator CLI. Cloud
traffic is served by mocks through the TLS intercept; nothing leaves."""
import os
import signal
import stat
import subprocess
import sys

from harness.gateway import model_call
from harness.run import case, eq, no_egress, sub

STORE_PROVIDERS = ["anthropic", "openai", "groq", "together", "lambda", "openrouter", "gemini"]


def endpoints(gw):
    return {e["endpoint_id"]: e for e in gw.get("/v1/discovery/health").json["endpoints"]}


@case("credentials", gw="cloud", needs=("tls_intercept",))
def store_keys_used(t):
    """A file store with fake keys: cloud discovery runs for those providers (fresh), the rest are unconfigured; calls carry the stored key."""
    e = endpoints(t.gw)
    eq({k: v["mode"] for k, v in e.items() if k in STORE_PROVIDERS}, {
        "anthropic": "refresh", "openai": "refresh", "gemini": "unconfigured", "groq": "unconfigured", "lambda": "unconfigured",
        "openrouter": "unconfigured", "together": "unconfigured"}, "modes")
    eq(e["groq"]["reason"], "no credential in the store; run: csgateway credentials set groq", "unconfigured reason")
    b = t.gw.call(model_call("gpt-spec")).json
    sub(b, {"status": "ok", "provider": "openai", "model": "gpt-spec", "cost": {"class": "api", "amount": "0.000075"}}, "gpt-spec")
    eq(t.run.calls(t.openai)[-1]["headers"]["authorization"], "Bearer sk-spec-FAKE", "openai Authorization")
    eq(t.gw.get("/v1/health").json["discovery_stale"], [], "unconfigured never alerts")
    no_egress(t)


@case("credentials", gw=None, needs=("tls_intercept",))
def discovery_list_decides_availability(t):
    """A configured provider's live model list decides which of its models.json entries are available."""
    t.openai.models = ["other-model"]
    store = t.run.credential_store("list", openai="sk-spec-FAKE")
    gw = t.gateway("cred-list", env={"CSGATEWAY_CREDENTIALS": str(store)})
    rows = {m["name"]: m["available"] for m in gw.get("/v1/models?available_only=false").json["models"]}
    eq(rows["gpt-spec"], False, "gpt-spec absent from the provider's list")
    t.openai.models = ["gpt-spec"]
    gw.post("/v1/inventory/refresh")
    rows = {m["name"]: m["available"] for m in gw.get("/v1/models?available_only=false").json["models"]}
    eq(rows["gpt-spec"], True, "available after the list includes it")


@case("credentials", gw=None, needs=("tls_intercept",))
def store_refused_fails_closed(t):
    """A group-readable store is refused: discovery fails with class credential (not unconfigured), and a call is provider_auth with no egress."""
    store = t.run.credential_store("open", dir_mode=0o750, openai="sk-spec-FAKE")
    gw = t.gateway("cred-open", env={"CSGATEWAY_CREDENTIALS": str(store)})
    e = endpoints(gw)
    sub(e["openai"], {"mode": "refresh", "state": "unknown", "last_error_class": "credential"}, "openai")
    eq("group/world-accessible" in e["openai"]["last_error"], True, "error names the permission problem")
    eq("sk-spec-FAKE" in gw.get("/v1/discovery/health").text, False, "no key in the report")
    b = gw.call(model_call("gpt-spec")).json
    sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "fatal_session", "code": "gateway.provider_auth"}}, "call")
    eq(t.proxy.seen.items, [], "no connection attempted to a provider")


@case("credentials", gw=None, needs=("tls_intercept",))
def store_file_too_open(t):
    """A 0644 credential file in a private directory is refused the same way."""
    store = t.run.credential_store("filemode", file_mode=0o644, openai="sk-spec-FAKE")
    gw = t.gateway("cred-filemode", env={"CSGATEWAY_CREDENTIALS": str(store)})
    sub(endpoints(gw)["openai"], {"last_error_class": "credential"}, "openai")


@case("credentials", gw=None, needs=("tls_intercept",))
def keychain_providers_narrow(t):
    """CSGATEWAY_KEYCHAIN_PROVIDERS narrows which providers use the store; others behave as keyless; an unknown name refuses startup."""
    store = t.run.credential_store("narrow", anthropic="sk-ant-spec-FAKE", openai="sk-spec-FAKE")
    gw = t.gateway("cred-narrow", env={"CSGATEWAY_CREDENTIALS": str(store), "CSGATEWAY_KEYCHAIN_PROVIDERS": "openai"})
    e = endpoints(gw)
    eq((e["openai"]["mode"], e["anthropic"]["mode"], e["anthropic"].get("reason")), ("refresh", "static", "no credential in the gateway process; models come from models.json"), "modes")
    sub(gw.call(model_call("claude-spec")).json, {"error": {"code": "gateway.no_provider_credential"}}, "anthropic not in the narrowed set")
    code, log = t.gateway("cred-badname", env={"CSGATEWAY_CREDENTIALS": str(store), "CSGATEWAY_KEYCHAIN_PROVIDERS": "openai,nosuch"}, expect_start=False).start_fails()
    eq((code != 0, 'unknown keychain provider "nosuch"' in log), (True, True), "unknown provider")


@case("credentials", gw=None)
def env_keys_ignored(t):
    """*_API_KEY in the gateway's environment is never used (stripped at start): with the store off the provider stays uncredentialed and nothing egresses."""
    gw = t.gateway("cred-env", env={"OPENAI_API_KEY": "sk-env-FAKE", "ANTHROPIC_API_KEY": "sk-ant-env-FAKE"})
    sub(endpoints(gw)["openai"], {"mode": "static", "reason": "no credential in the gateway process; models come from models.json"}, "openai")
    sub(gw.call(model_call("gpt-spec")).json, {"error": {"code": "gateway.no_provider_credential"}}, "call")
    eq(t.proxy.seen.items, [], "egress attempts")


@case("credentials", gw=None)
def env_keys_not_inherited_by_children(t):
    """Stripped keys never reach work-order children."""
    import json as _json
    cfg = t.run.config(deployments=[{"id": "cli-spec", "provider": "cli", "models": ["fake-cli"], "max_concurrent": 1, "max_queued": 0, "max_wait_ms": 0}], lab_executor=True)
    gw = t.gateway("cred-child", cfg, env={"OPENAI_API_KEY": "sk-env-FAKE"})
    rep = _json.loads(gw.post("/v1/ai/lab-execute", {"work_order": {"executor": "fake-cli", "timeout_s": 5, "fake": {}}}).json["stdout"])
    eq([n for n in rep["env_names"] if n.endswith("_API_KEY")], [], "child *_API_KEY names")


@case("credentials", gw=None)
def credentials_invalid_spec(t):
    """CSGATEWAY_CREDENTIALS must be empty, keychain, off or absolute: a relative value refuses startup."""
    code, log = t.gateway("cred-rel", env={"CSGATEWAY_CREDENTIALS": "relative/dir"}, expect_start=False).start_fails()
    eq((code != 0, 'CSGATEWAY_CREDENTIALS must be empty, "keychain", "off" or an absolute path, got "relative/dir"' in log), (True, True), "refused")


# The Keychain is opt-in: the default store must never exec `security`. On
# macOS the gateway runs under a sandbox that denies that one exec, so a
# regression shows as a credential failure and never reaches the real Keychain.
NO_SECURITY = ["/usr/bin/sandbox-exec", "-p", '(version 1)(allow default)(deny process-exec (literal "/usr/bin/security"))']


@case("credentials", gw=None, needs=("tls_intercept",))
def default_store_is_the_file_store(t):
    """CSGATEWAY_CREDENTIALS unset: the store is ~/.contextstack/credentials only, never the Keychain (no `security` exec); its key is used, a missing provider is unconfigured."""
    wrap = NO_SECURITY if sys.platform == "darwin" else ()
    gw = t.gateway("cred-default", env={"CSGATEWAY_CREDENTIALS": None}, expect_start=False, wrap=wrap)
    d = gw.home / ".contextstack" / "credentials"
    d.mkdir(parents=True)
    (d / "openai").write_text("sk-spec-FAKE\n")
    (d / "openai").chmod(0o600)
    d.chmod(0o700)
    gw.start()
    e = endpoints(gw)
    sub(e["openai"], {"mode": "refresh", "state": "fresh"}, "openai from the file store")
    sub(e["anthropic"], {"mode": "unconfigured", "reason": "no credential in the store; run: csgateway credentials set anthropic"}, "anthropic")
    eq([k for k, v in e.items() if v.get("last_error_class") == "credential"], [], "no credential failures (a denied security exec would be one)")
    sub(gw.call(model_call("gpt-spec")).json, {"status": "ok", "model": "gpt-spec"}, "gpt-spec")
    eq(t.run.calls(t.openai)[-1]["headers"]["authorization"], "Bearer sk-spec-FAKE", "openai Authorization")
    code, out, _ = cli(t, ["list"], {"HOME": str(gw.home)}, wrap=wrap)
    eq((code, out.splitlines()[0]), (0, "store: file " + str(d)), "list names the file store only")
    no_egress(t)


def cli(t, args, env=None, stdin="", wrap=()):
    e = {"HOME": str(t.run.tmp), "PATH": "/usr/bin:/bin", "LANG": "C"}
    e.update(env or {})
    p = subprocess.run([*wrap, t.run.binary, "credentials", *args], input=stdin, capture_output=True, text=True, env=e, timeout=20)
    return p.returncode, p.stdout, p.stderr


@case("credentials", gw=None)
def credentials_cli(t):
    """The operator CLI: list never prints values; set reads stdin into a 0600 file in a 0700 dir; delete; import-env prints only names; store off refuses writes."""
    d = str(t.run.tmp / "cli-store")
    env = {"CSGATEWAY_CREDENTIALS": d}
    code, out, err = cli(t, ["set", "openai"], env, "sk-cli-FAKE\n")
    eq((code, out, err), (0, "stored openai in file; a running gateway picks it up on SIGHUP or restart\n", ""), "set")
    eq((stat.S_IMODE(os.stat(d).st_mode), stat.S_IMODE(os.stat(os.path.join(d, "openai")).st_mode)), (0o700, 0o600), "modes")
    eq(open(os.path.join(d, "openai")).read(), "sk-cli-FAKE\n", "stored value")
    code, out, err = cli(t, ["list"], env)
    eq(code, 0, "list exit")
    eq(out.splitlines(), ["store: file " + d, "anthropic   not configured", "openai      configured (file)", "groq        not configured", "together    not configured",
                          "lambda      not configured", "openrouter  not configured", "gemini      not configured", "typesafe    not configured"], "list")
    code, out, err = cli(t, ["import-env"], dict(env, GROQ_API_KEY="gsk-FAKE"))
    eq(code, 0, "import-env exit")
    eq("imported groq from GROQ_API_KEY into file" in out and "gsk-FAKE" not in out + err, True, "import-env output names only")
    eq(cli(t, ["delete", "openai"], env)[:2], (0, "deleted openai\n"), "delete")
    eq(os.path.exists(os.path.join(d, "openai")), False, "file removed")
    code, out, err = cli(t, ["set", "openai"], {"CSGATEWAY_CREDENTIALS": "off"}, "x\n")
    eq((code, err), (1, "credentials: the store is off (CSGATEWAY_CREDENTIALS=off)\n"), "store off")
    eq(cli(t, ["set", "nosuch"], env, "x\n")[0], 2, "unknown provider is a usage error")
    eq(cli(t, [], env)[0], 2, "no command is a usage error")
    os.chmod(d, 0o755)
    code, out, err = cli(t, ["list"], env)
    eq("refused: credential path" in out and "group/world-accessible" in out, True, "list reports a refused store per provider")


def listings(mock):
    return [r for r in mock.requests.items if r["method"] == "GET" and r["path"].startswith("/v1/models")]


@case("credentials", gw=None, fast=False, needs=("tls_intercept",))
def credentials_resolved_once(t):
    """Each provider's credential is resolved once per start (found, missing or refused): store edits after start reach no discovery pass and no call; SIGHUP is the explicit reload."""
    fast = {"CSAGENT_INVENTORY_REFRESH_INTERVAL": "1s", "CSGATEWAY_DISCOVERY_STALE_AFTER": "3s"}
    store = t.run.credential_store("once", openai="sk-spec-FAKE")
    gw = t.gateway("cred-once", env=dict(fast, CSGATEWAY_CREDENTIALS=str(store)))
    gw.wait(lambda: listings(t.openai), timeout=5, what="first openai listing")
    eq(endpoints(gw)["anthropic"]["mode"], "unconfigured", "anthropic missing at start")
    (store / "openai").write_text("sk-spec-ROTATED-FAKE\n")
    (store / "anthropic").write_text("sk-ant-spec-FAKE\n")
    (store / "anthropic").chmod(0o600)
    seen = len(listings(t.openai))
    gw.wait(lambda: len(listings(t.openai)) >= seen + 2, timeout=8, what="two more discovery passes")
    eq(sorted({r["headers"]["authorization"] for r in listings(t.openai)}), ["Bearer sk-spec-FAKE"], "every pass used the start-time key")
    sub(endpoints(gw)["anthropic"], {"mode": "unconfigured", "reason": "no credential in the store; run: csgateway credentials set anthropic"}, "missing stays missing")
    for _ in range(3):
        sub(gw.call(model_call("gpt-spec")).json, {"status": "ok", "model": "gpt-spec"}, "gpt-spec")
        sub(gw.call(model_call("claude-spec")).json, {"status": "error", "error": {"code": "gateway.provider_auth", "message": "no credential configured for anthropic (csgateway credentials set anthropic)"}}, "claude-spec")
    eq([c["headers"]["authorization"] for c in t.run.calls(t.openai)], ["Bearer sk-spec-FAKE"] * 3, "every call used the start-time key")
    eq(t.anthropic.requests.items, [], "nothing sent to anthropic")
    gw.send(signal.SIGHUP)
    gw.wait(lambda: endpoints(gw)["anthropic"]["mode"] == "refresh", timeout=5, what="anthropic configured after SIGHUP")
    gw.wait(lambda: listings(t.openai)[-1]["headers"]["authorization"] == "Bearer sk-spec-ROTATED-FAKE", timeout=5, what="rotated key after SIGHUP")
    gw.stop()
    eq("FAKE" in gw.log_text(), False, "no key value in the log")
    t.proxy.seen.clear()

    refused = t.run.credential_store("once-refused", dir_mode=0o750, openai="sk-spec-FAKE")
    gw2 = t.gateway("cred-once-refused", env=dict(fast, CSGATEWAY_CREDENTIALS=str(refused)))
    first = gw2.wait(lambda: endpoints(gw2)["openai"].get("consecutive_failures"), timeout=5, what="refused at start")
    refused.chmod(0o700)
    gw2.wait(lambda: endpoints(gw2)["openai"]["consecutive_failures"] >= first + 2, timeout=8, what="two more discovery passes")
    sub(endpoints(gw2)["openai"], {"mode": "refresh", "last_error_class": "credential"}, "refusal stands until restart")
    for _ in range(3):
        sub(gw2.call(model_call("gpt-spec")).json, {"status": "error", "error": {"code": "gateway.provider_auth"}}, "call refused")
    eq(t.proxy.seen.items, [], "no connection attempted to a provider")
