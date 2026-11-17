"""Routes and schemas: the route inventory after V2.2 retirement, auth,
status and headers, schema discriminators, alias, malformed and unknown input."""
from harness.gateway import V1, V2, model_call
from harness.run import absent, case, eq, norm, sub

TS = "RFC3339 timestamp"


def is_rfc3339(s):
    import re
    return isinstance(s, str) and re.match(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?(Z|[+-]\d\d:\d\d)$", s) is not None


@case("routes")
def health_shape(t):
    """GET /v1/health: healthy, public, implementation, inventory counts, wire_cutover, discovery_stale, catalog_revision (phase 5)."""
    r = t.gw.get("/v1/health", auth=False)
    eq(r.status, 200, "status")
    eq(r.headers.get("content-type"), "application/json", "content-type")
    b = r.json
    eq(sorted(b), sorted(["catalog_revision", "discovery_stale", "implementation", "last_inventory_refresh", "models_available", "ollama_reachable",
                          "providers_active", "role_aware_dispatch", "status", "uptime_seconds", "version", "wire_cutover"]), "keys")
    sub(b, {"status": "healthy", "version": "v0.1.0", "models_available": 8, "ollama_reachable": True,
            "role_aware_dispatch": False, "discovery_stale": []}, "health")
    eq(sorted(b["providers_active"]), ["anthropic", "mock", "ollama", "vllm"], "providers_active (anthropic via the claude-code CLI entry)")
    eq(is_rfc3339(b["last_inventory_refresh"]), True, "last_inventory_refresh " + TS)
    eq(isinstance(b["uptime_seconds"], int), True, "uptime_seconds integer")
    sub(b["wire_cutover"], {"threshold": 500, "ready": False}, "wire_cutover")


@case("routes")
def health_implementation(t):
    """GET /v1/health names the implementation serving it, so a run can record which gateway served its calls."""
    eq(t.gw.get("/v1/health", auth=False).json.get("implementation"), t.run.impl, "implementation")


@case("routes")
def health_ollama_unreachable(t):
    """ollama_reachable reflects a live probe of the Ollama endpoint at request time."""
    gw = t.gateway("ollama-down", env={"OLLAMA_ENDPOINT": "http://127.0.0.1:1"})
    eq(gw.get("/v1/health").json["ollama_reachable"], False, "ollama_reachable")


@case("routes")
def capabilities_shape(t):
    """GET /v1/capabilities: public; API models available, each with its streaming capability; CLI executors separate."""
    r = t.gw.get("/v1/capabilities", auth=False)
    eq(r.status, 200, "status")
    b = r.json
    sub(b, {"gateway_status": "online", "supports_local_only": True, "models_available": 8, "degraded_reasons": []}, "capabilities")
    eq(sorted(m["name"] for m in b["api_models"]),
       ["fake-paid", "fake-sub", "local-coder", "mock-echo", "ollama-small", "vllm-big", "vllm-extra"], "api_models")
    eq({m["streaming"] for m in b["api_models"]}, {True}, "api_models streaming")
    eq(b["cli_executors"], [{"name": "claude-code", "kind": "cli", "binary": "claude", "tier": 1, "capability": "coding", "available": True}], "cli_executors")
    eq(is_rfc3339(b["last_refresh_at"]), True, "last_refresh_at " + TS)


@case("routes")
def models_listing(t):
    """GET /v1/models: available only by default; cost classes; filters; estate and capacity."""
    b = t.gw.get("/v1/models").json
    rows = {m["name"]: m for m in b["models"]}
    eq(sorted(rows), ["claude-code", "fake-paid", "fake-sub", "local-coder", "mock-echo", "ollama-small", "vllm-big", "vllm-extra"], "available models")
    eq(rows["local-coder"], {"name": "local-coder", "provider": "vllm", "tier": 1, "capability": "coding", "available": True,
                             "preferred": True, "streaming": True, "cost_class": "free_local"}, "local-coder row")
    eq(rows["claude-code"], {"name": "claude-code", "provider": "anthropic", "tier": 1, "capability": "coding", "available": True,
                             "preferred": False, "streaming": False, "executor": "cli", "cli_binary": "claude", "cost_class": "subscription"}, "claude-code row")
    eq({n: r["streaming"] for n, r in rows.items()}, {"claude-code": False, "fake-paid": True, "fake-sub": True, "local-coder": True,
       "mock-echo": True, "ollama-small": True, "vllm-big": True, "vllm-extra": True}, "streaming capability (a CLI entry never streams)")
    eq({n: r["cost_class"] for n, r in rows.items()}, {"claude-code": "subscription", "fake-paid": "api", "fake-sub": "subscription",
       "local-coder": "free_local", "mock-echo": "api", "ollama-small": "free_local", "vllm-big": "free_local", "vllm-extra": "free_local"}, "cost classes")
    eq(rows["vllm-extra"], {"name": "vllm-extra", "provider": "vllm", "tier": 2, "capability": "coding", "available": True,
                            "preferred": False, "streaming": True, "cost_class": "free_local"}, "discovered-only vLLM model auto-added")
    sub(b, {"estate": "", "deployment_capacity": {}}, "admission fields")
    eq(sorted(b["providers_active"]), ["anthropic", "mock", "ollama", "vllm"], "providers_active")
    every = {m["name"]: m["available"] for m in t.gw.get("/v1/models?available_only=false").json["models"]}
    eq({k: v for k, v in every.items() if not v}, {"claude-spec": False, "gpt-spec": False, "vllm-absent": False}, "unavailable rows with available_only=false")
    eq(sorted(m["name"] for m in t.gw.get("/v1/models?tier=2").json["models"]), ["ollama-small", "vllm-big", "vllm-extra"], "tier=2")
    eq(sorted(m["name"] for m in t.gw.get("/v1/models?capability=general").json["models"]), ["fake-paid", "fake-sub", "mock-echo", "ollama-small"], "capability=general")
    eq(sorted(m["name"] for m in t.gw.get("/v1/models?tier=abc").json["models"]), sorted(rows), "unparseable tier ignored")


@case("routes")
def models_cli(t):
    """GET /v1/models/cli lists available CLI executors only."""
    eq(t.gw.get("/v1/models/cli").json, {"executors": [{"name": "claude-code", "binary": "claude", "tier": 1, "capability": "coding", "available": True}]}, "executors")
    eq(t.gw.get("/v1/models/cli?tier=2").json, {"executors": []}, "tier filter")


@case("routes")
def retired_routes_404(t):
    """Retired (V2.2 §5): /v1/llm/call, /v1/ai/execute and its DELETE answer 404, never redirect, never call upstream."""
    for method, path in (("POST", "/v1/llm/call"), ("POST", "/v1/ai/execute"), ("DELETE", "/v1/ai/execute/req-1")):
        r = t.gw.request(method, path, {"prompt": "hi", "model_override": "local-coder"})
        eq(r.status, 404, method + " " + path)
        absent(r.headers, ["location"], method + " " + path + " headers")
    eq(t.run.calls(t.vllm) + t.run.calls(t.ollama), [], "upstream requests")


@case("routes")
def unknown_route_and_method(t):
    """Unknown path is 404; a known path with the wrong method is 405 with Allow."""
    eq(t.gw.get("/v1/nope").status, 404, "unknown path")
    retired = "u" + "ah"
    eq(t.gw.post("/v1/" + retired + "/model.call", model_call("local-coder")).status, 404, "retired product route")
    envelope_error(t, model_call("local-coder", schema=retired + ".gateway.model_call.v1"), "gateway.schema_mismatch")
    r = t.gw.get("/v1/agent/model.call")
    eq(r.status, 405, "GET on model.call")
    eq(r.headers.get("allow"), "POST", "Allow")
    eq(t.gw.post("/v1/models", {}).status, 405, "POST on /v1/models")


@case("routes", gw="auth")
def auth_bearer(t):
    """Bearer auth: health and capabilities public; everything else 401 without the exact token."""
    eq(t.gw.get("/v1/health", auth=False).status, 200, "health without token")
    eq(t.gw.get("/v1/capabilities", auth=False).status, 200, "capabilities without token")
    for path in ("/v1/models", "/v1/models/cli", "/v1/discovery/health", "/v1/admission"):
        r = t.gw.get(path, auth=False)
        eq((r.status, r.json), (401, {"error": "unauthorized", "code": "auth_required"}), path + " without token")
    r = t.gw.call(model_call("local-coder"), auth=False)
    eq((r.status, r.json), (401, {"error": "unauthorized", "code": "auth_required"}), "model.call without token")
    eq(t.gw.get("/v1/models", auth=False, headers={"Authorization": "Bearer wrong"}).status, 401, "wrong token")
    eq(t.gw.get("/v1/models", auth=False, headers={"Authorization": "bearer spec-token"}).status, 401, "scheme is case-sensitive")
    eq(t.gw.get("/v1/models").status, 200, "right token")
    eq(t.gw.call(model_call("local-coder")).json["status"], "ok", "model.call with token")
    eq(len(t.run.calls(t.vllm)), 1, "only the authorised call reached upstream")


@case("routes", gw="auth")
def auth_retired_route_order(t):
    """Auth runs before routing: a retired route without a token is 401, not 404."""
    eq(t.gw.post("/v1/llm/call", {}, auth=False).status, 401, "retired route, no token")
    eq(t.gw.post("/v1/llm/call", {}).status, 404, "retired route, token")


@case("routes")
def schema_v2_and_v1_echo(t):
    """Both accepted discriminators echo the request's schema_version; request_id echoed."""
    for schema in (V2, V1):
        b = t.gw.call(model_call("local-coder", schema=schema, request_id="rid-" + schema)).json
        sub(b, {"schema_version": schema, "request_id": "rid-" + schema, "status": "ok", "model": "local-coder"}, schema)


@case("routes")
def alias_route(t):
    """/v1/cs/model.call is the same handler as /v1/agent/model.call."""
    a = norm(t.gw.call(model_call("local-coder")).json)
    b = norm(t.gw.call(model_call("local-coder"), path="/v1/cs/model.call").json)
    eq(b, a, "alias response")
    eq(len(t.run.calls(t.vllm)), 2, "both reached upstream")


@case("routes")
def model_call_body_limit(t):
    """A model call over 32 MiB is 413 and is not dispatched."""
    raw = b"x" * (32 * 1024 * 1024 + 1)
    r = t.gw.post("/v1/agent/model.call", raw=raw)
    eq(r.status, 413, "oversized model call")
    eq(t.run.calls(t.vllm), [], "not dispatched")


ENVELOPE_ERROR_KEYS = ["cost", "error", "request_id", "schema_version", "status", "usage"]


def envelope_error(t, body, code, schema=V2, raw=None, request_id=None):
    r = t.gw.post("/v1/agent/model.call", body, raw=raw)
    eq(r.status, 200, code + " status (envelope errors are HTTP 200)")
    eq(r.headers.get("content-type"), "application/json", code + " content-type")
    b = r.json
    eq(sorted(b), ENVELOPE_ERROR_KEYS if request_id is not None else sorted(ENVELOPE_ERROR_KEYS), code + " keys")
    sub(b, {"schema_version": schema, "status": "error", "usage": {"input_tokens": 0, "output_tokens": 0},
            "cost": {"currency": "USD", "amount": "0.000000"},
            "error": {"class": "fatal_session", "code": code, "boundary": "gateway", "recoverable": False}}, code)
    absent(b["cost"], ["class", "est_usd", "charged_usd", "charged"], code + " cost")
    eq(t.run.calls(t.vllm), [], code + " upstream requests")
    return b


@case("routes")
def schema_mismatch(t):
    """Unknown schema_version: gateway.schema_mismatch under the v2 family, request_id echoed."""
    b = envelope_error(t, model_call("local-coder", schema="agent.gateway.model_call.v9", request_id="rid-x"), "gateway.schema_mismatch", request_id="rid-x")
    eq(b["request_id"], "rid-x", "request_id")
    eq(envelope_error(t, {"messages": []}, "gateway.schema_mismatch")["request_id"], "", "missing schema_version")


@case("routes")
def malformed_json(t):
    """Undecodable body: gateway.bad_request under the v2 family, request_id empty."""
    b = envelope_error(t, None, "gateway.bad_request", raw=b'{"schema_version": ')
    eq(b["request_id"], "", "request_id")
    envelope_error(t, None, "gateway.bad_request", raw=b'{"schema_version": "agent.gateway.model_call.v2", "messages": "not-a-list"}')


@case("routes")
def empty_messages(t):
    """No messages: gateway.empty_messages, echoing the accepted schema (v1 stays v1)."""
    envelope_error(t, model_call("local-coder", messages=[]), "gateway.empty_messages")
    envelope_error(t, model_call("local-coder", messages=[], schema=V1), "gateway.empty_messages", schema=V1)


@case("routes")
def unknown_fields_ignored(t):
    """Unknown top-level and nested fields are ignored (tolerant decoder), not refused."""
    body = model_call("local-coder", surprise={"x": 1})
    body["capability"]["future_hint"] = "y"
    body["messages"][0]["extra"] = True
    eq(t.gw.call(body).json["status"], "ok", "status")


@case("routes", gw=None)
def wire_cutover_counts(t):
    """wire_cutover counts accepted requests by schema and caller; v1 resets consecutive_v2; refused schemas do not count."""
    gw = t.gateway("wire")
    gw.call(model_call("local-coder", schema=V2), headers={"X-ContextStack-Caller": " runner "})
    gw.call(model_call("local-coder", schema=V1))
    gw.call(model_call("nope", schema=V2), headers={"X-ContextStack-Caller": "runner"})
    gw.call(model_call("local-coder", schema=V2, messages=[], role={"role_pack_name": "pack-a"}))
    gw.call(model_call("local-coder", schema="bogus"))
    gw.post("/v1/agent/model.call", raw=b"{")
    eq(gw.get("/v1/health").json["wire_cutover"], {
        "consecutive_v2": 2, "ready": False, "threshold": 500,
        "totals_by_schema": {V1: 1, V2: 3},
        "totals_by_caller_schema": {"runner": {V2: 2}, "unknown": {V1: 1}, "pack-a": {V2: 1}}}, "wire_cutover")
    rows = gw.log_rows("wire_request")
    eq([(r["schema_version"], r["caller"], r["path"]) for r in rows],
       [(V2, "runner", "/v1/agent/model.call"), (V1, "unknown", "/v1/agent/model.call"),
        (V2, "runner", "/v1/agent/model.call"), (V2, "pack-a", "/v1/agent/model.call")], "wire_request rows")


@case("routes")
def admission_report_unconfigured(t):
    """GET /v1/admission with no deployments: empty capacity, config hash, echoed configuration."""
    b = t.gw.get("/v1/admission").json
    eq(sorted(b), ["capacity", "config_sha", "configuration", "deployments", "estate"], "keys")
    sub(b, {"capacity": {}, "deployments": {}, "estate": ""}, "admission")
    eq(len(b["config_sha"]), 64, "config_sha is a sha256")
    eq(b["configuration"]["deployments"], [], "configuration.deployments")


@case("routes")
def inventory_refresh_route(t):
    """POST /v1/inventory/refresh re-runs discovery and reports per-model changes."""
    t.vllm.models = ["local-coder", "vllm-big", "fake-paid", "fake-sub", "vllm-new"]
    b = t.gw.post("/v1/inventory/refresh").json
    try:
        eq(sorted(b), ["changes", "models_after", "models_before", "refreshed_at"], "keys")
        eq((b["models_before"], b["models_after"]), (11, 11), "counts (10 configured + 1 discovered-only)")
        eq(sorted((c["action"], c["model"], c["provider"]) for c in b["changes"]),
           [("added", "vllm-new", "vllm"), ("removed", "vllm-extra", "vllm")], "changes")
        eq(is_rfc3339(b["refreshed_at"]), True, "refreshed_at " + TS)
    finally:
        t.vllm.models = ["local-coder", "vllm-big", "fake-paid", "fake-sub", "vllm-extra"]
        t.gw.post("/v1/inventory/refresh")
