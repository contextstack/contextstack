"""Discovery and per-endpoint discovery health (V2.2 §4) against
mock endpoints: modes, freshness, stale conditions, recovery, persistence,
failure classes, auto-added models, refresh triggers."""
import json
import signal
import time

from harness.run import case, eq, sub

SCHEMA = "csgateway.discovery_health.v1"
STATIC_NO_KEY = "no credential in the gateway process; models come from models.json"


def endpoints(gw):
    return {e["endpoint_id"]: e for e in gw.get("/v1/discovery/health").json["endpoints"]}


@case("discovery")
def discovery_health_modes(t):
    """Every catalog endpoint is reported: refresh endpoints fresh with counts; keyless cloud static; mock static; executables listed. vllm-spark2 is not in the embedded catalog (phase 5: DGX by host overlay)."""
    r = t.gw.get("/v1/discovery/health").json
    eq(sorted(r), ["conditions", "endpoints", "evaluated_at", "executables", "schema", "stale_after_s"], "keys")
    sub(r, {"schema": SCHEMA, "stale_after_s": 900, "conditions": []}, "report")
    e = endpoints(t.gw)
    eq(sorted(e), ["anthropic", "gemini", "groq", "lambda", "mock", "ollama", "openai", "openrouter", "together", "typesafe", "vllm"], "endpoint ids")
    eq({k: (v["mode"], v["state"]) for k, v in e.items()}, {
        "anthropic": ("static", "static"), "gemini": ("static", "static"), "groq": ("static", "static"), "lambda": ("static", "static"),
        "mock": ("static", "static"), "ollama": ("refresh", "fresh"), "openai": ("static", "static"), "openrouter": ("static", "static"),
        "together": ("static", "static"), "typesafe": ("static", "static"), "vllm": ("refresh", "fresh")}, "modes and states")
    eq((e["vllm"]["model_count"], e["ollama"]["model_count"], e["vllm"]["consecutive_failures"]), (5, 1, 0), "counts")
    eq((e["openai"]["reason"], e["mock"]["reason"]), (STATIC_NO_KEY, "no discovery protocol"), "reasons")
    v = e["vllm"]
    eq(v["last_success"] == v["last_attempt"] and v["expected_since"] <= v["last_success"], True, "success timestamps (expected_since kept across refreshes)")
    eq({x["name"]: (x["source"], "error" in x) for x in r["executables"]},
       {"claude": ("configured", False), "codex": ("configured", True), "gemini": ("configured", True)}, "executables")


@case("discovery", gw=None)
def discovery_failure_isolated(t):
    """One endpoint failing (vLLM 500) does not touch another: vllm unknown with a status-class failure, ollama fresh, inventory still served."""
    t.vllm.discovery = {"json": {}, "status": 500}
    gw = t.gateway("disc-fail")
    e = endpoints(gw)
    sub(e["vllm"], {"mode": "refresh", "state": "unknown", "consecutive_failures": 1, "last_error_class": "status",
                    "last_error": "vllm /v1/models returned status 500", "model_count": 0}, "vllm")
    eq("last_success" in e["vllm"], False, "a failure never manufactures last_success")
    sub(e["ollama"], {"state": "fresh", "model_count": 1}, "ollama")
    rows = {m["name"]: m["available"] for m in gw.get("/v1/models?available_only=false").json["models"]}
    eq((rows["local-coder"], rows["ollama-small"]), (False, True), "availability follows discovery")
    eq(gw.get("/v1/health").json["discovery_stale"], [], "not stale before stale_after")


@case("discovery", gw=None)
def discovery_error_classes(t):
    """Failure classes: undecodable body is decode; connection refused is request; a valid empty list is a success with model_count 0."""
    t.vllm.discovery = {"raw": b"not json", "type": "application/json"}
    t.ollama.discovery = {"json": {"models": []}}
    gw = t.gateway("disc-classes")
    e = endpoints(gw)
    sub(e["vllm"], {"state": "unknown", "last_error_class": "decode"}, "vllm decode")
    sub(e["ollama"], {"state": "fresh", "model_count": 0, "consecutive_failures": 0}, "ollama empty success")
    gw2 = t.gateway("disc-refused", env={"VLLM_ENDPOINT": "http://127.0.0.1:1"})
    sub(endpoints(gw2)["vllm"], {"state": "unknown", "last_error_class": "request"}, "vllm refused")


@case("discovery", gw=None, fast=False)
def discovery_stale_and_recovery(t):
    """A failing endpoint goes stale after stale_after: condition discovery-stale-vllm with a fixed first_observed and growing age, listed in /v1/health; only vLLM's own success clears it."""
    gw = t.gateway("disc-stale", env={"CSAGENT_INVENTORY_REFRESH_INTERVAL": "1s", "CSGATEWAY_DISCOVERY_STALE_AFTER": "2s"})
    t.vllm.discovery = {"json": {}, "status": 503}
    stale = gw.wait(lambda: [c for c in gw.get("/v1/discovery/health").json["conditions"]], timeout=8, what="stale condition")
    c1 = stale[0]
    sub(c1, {"kind": "discovery_stale", "id": "discovery-stale-vllm", "endpoint_id": "vllm", "last_error_class": "status",
             "recovery": "bring vllm back (it clears on its next successful discovery), or set CSGATEWAY_DISCOVERY_DISABLED=vllm"}, "condition")
    eq(gw.get("/v1/health").json["discovery_stale"], ["discovery-stale-vllm"], "/v1/health")
    eq(endpoints(gw)["ollama"]["state"], "fresh", "ollama unaffected")
    time.sleep(2.1)
    c2 = gw.get("/v1/discovery/health").json["conditions"][0]
    eq(c2["first_observed"], c1["first_observed"], "first_observed stable across failed retries")
    eq(c2["age_s"] > c1["age_s"] and c2["consecutive_failures"] > c1["consecutive_failures"], True, "age and failures grow")
    t.vllm.discovery = None
    gw.wait(lambda: gw.get("/v1/health").json["discovery_stale"] == [], timeout=6, what="recovery")
    sub(endpoints(gw)["vllm"], {"state": "fresh", "consecutive_failures": 0}, "vllm recovered")


@case("discovery", gw=None)
def discovery_stopped_loop_goes_stale(t):
    """Freshness is evaluated on read: a persisted last_success older than stale_after is stale at once, aged from last_success + stale_after."""
    path = t.run.tmp / "seed-health.json"
    old = "2026-01-01T00:00:00Z"
    path.write_text(json.dumps({"schema": SCHEMA, "stale_after_s": 60, "written_at": old, "endpoints": [
        {"endpoint_id": "vllm", "mode": "refresh", "expected_since": old, "last_attempt": old, "last_success": old, "consecutive_failures": 0, "model_count": 5}],
        "executables": []}))
    t.vllm.discovery = {"json": {}, "status": 500}
    gw = t.gateway("disc-seed", env={"CSGATEWAY_DISCOVERY_HEALTH": str(path), "CSGATEWAY_DISCOVERY_STALE_AFTER": "60m"})
    c = gw.get("/v1/discovery/health").json["conditions"]
    eq([(x["id"], x["first_observed"], x["last_success"], x["consecutive_failures"]) for x in c],
       [("discovery-stale-vllm", "2026-01-01T01:00:00Z", old, 1)], "condition from the persisted record")


@case("discovery", gw=None)
def discovery_health_persisted(t):
    """The health file is written on assembly and on clean stop (stopped_at); off disables it."""
    path = t.run.tmp / "persist" / "health.json"
    gw = t.gateway("disc-persist", env={"CSGATEWAY_DISCOVERY_HEALTH": str(path)})
    f = json.loads(path.read_text())
    eq(sorted(f), ["endpoints", "executables", "schema", "stale_after_s", "written_at"], "file keys while running")
    eq(f["schema"], SCHEMA, "schema")
    gw.stop()
    eq("stopped_at" in json.loads(path.read_text()), True, "stopped_at after SIGTERM")
    code, log = t.gateway("disc-relative", env={"CSGATEWAY_DISCOVERY_HEALTH": "relative.json"}, expect_start=False).start_fails()
    eq((code != 0, 'CSGATEWAY_DISCOVERY_HEALTH must be an absolute path or "off"' in log), (True, True), "relative path refused")


@case("discovery", gw=None)
def discovery_disabled(t):
    """CSGATEWAY_DISCOVERY_DISABLED: no discovery request, mode disabled, never stale; an unknown id refuses startup."""
    disabled = ",".join(["vllm"] + (t.run.profile.get("disable_endpoints") or []))
    gw = t.gateway("disc-off", env={"CSGATEWAY_DISCOVERY_DISABLED": disabled, "CSGATEWAY_DISCOVERY_STALE_AFTER": "2s",
                                    "CSAGENT_INVENTORY_REFRESH_INTERVAL": "1s"})
    eq([r for r in t.vllm.requests.items if r["path"] == "/v1/models"], [], "no vLLM discovery request")
    sub(endpoints(gw)["vllm"], {"mode": "disabled", "state": "disabled"}, "vllm")
    eq([m["name"] for m in gw.get("/v1/models").json["models"] if m["provider"] == "vllm"], [], "no vLLM model available without discovery")
    code, log = t.gateway("disc-unknown", env={"CSGATEWAY_DISCOVERY_DISABLED": "nosuch"}, expect_start=False).start_fails()
    eq((code != 0, 'CSGATEWAY_DISCOVERY_DISABLED: unknown endpoint "nosuch"' in log), (True, True), "unknown id")


@case("discovery", gw=None)
def discovery_stale_after_bound(t):
    """stale_after must exceed the refresh interval, else startup refuses."""
    code, log = t.gateway("disc-bound", env={"CSGATEWAY_DISCOVERY_STALE_AFTER": "30s"}, expect_start=False).start_fails()
    eq((code != 0, "CSGATEWAY_DISCOVERY_STALE_AFTER (30s) must exceed the refresh interval (1m0s)" in log), (True, True), "refused")


@case("discovery", gw=None)
def discovery_sighup_refresh(t):
    """SIGHUP re-runs discovery: a model newly served by vLLM becomes available without a restart."""
    gw = t.gateway("disc-hup")
    eq("vllm-later" in [m["name"] for m in gw.get("/v1/models").json["models"]], False, "absent at start")
    t.vllm.models = t.vllm.models + ["vllm-later"]
    gw.send(signal.SIGHUP)
    gw.wait(lambda: "vllm-later" in [m["name"] for m in gw.get("/v1/models").json["models"]], timeout=5, what="vllm-later after SIGHUP")


@case("discovery", gw=None)
def discovery_ollama_auto_add(t):
    """A model Ollama serves that models.json does not name is added as tier 2 general free_local with the shadow price."""
    t.ollama.tags = ["ollama-small", "qwen-new:7b"]
    gw = t.gateway("disc-ollama")
    rows = {m["name"]: m for m in gw.get("/v1/models").json["models"]}
    eq(rows["qwen-new:7b"], {"name": "qwen-new:7b", "provider": "ollama", "tier": 2, "capability": "general", "available": True,
                             "preferred": False, "streaming": True, "cost_class": "free_local"}, "auto-added row")
