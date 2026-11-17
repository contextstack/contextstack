"""Routing: unknown model refuses, empty hint is free-local, resolved model
echoed, no implicit paid fallback, CLI and model-call routes stay separate."""
from harness.gateway import model_call
from harness.run import case, eq, no_egress, sub
from harness import upstreams


def refused(t, b, code, cls="fatal_session"):
    sub(b, {"status": "error", "cost": {"currency": "USD", "amount": "0.000000"},
            "error": {"class": cls, "code": code}}, code)
    eq(t.run.calls(t.vllm) + t.run.calls(t.ollama), [], code + ": upstream requests")
    no_egress(t)


@case("routing")
def unknown_model_refuses(t):
    """WIRE clause 1: an unknown model is gateway.unknown_model, $0, no provider called, never a fallback or mock."""
    b = t.gw.call(model_call("no-such-model")).json
    refused(t, b, "gateway.unknown_model")
    eq(b["error"]["message"], "unknown model_tier_hint no-such-model — not in inventory; refuse, never fall through", "message")
    eq("model" in b, False, "no model echoed on refusal")


@case("routing")
def empty_hint_local_coder(t):
    """an empty hint selects local-coder, free_local, $0."""
    b = t.gw.call(model_call("")).json
    sub(b, {"status": "ok", "model": "local-coder", "provider": "vllm",
            "cost": {"amount": "0.000000", "class": "free_local", "est_usd": 0, "charged_usd": 0}}, "empty hint")
    eq([c["json"]["model"] for c in t.run.calls(t.vllm)], ["local-coder"], "upstream model")


@case("routing", gw=None)
def empty_hint_other_free_local(t):
    """Without local-coder, the empty hint takes the lowest-tier free_local model (preferred, then name)."""
    models = [m for m in t.run.config()["models"] if m["name"] != "local-coder"]
    t.vllm.models = [m for m in t.vllm.models if m != "local-coder"]  # else discovery re-adds it
    gw = t.gateway("no-local-coder", t.run.config(models=models))
    b = gw.call(model_call("")).json
    sub(b, {"status": "ok", "model": "ollama-small", "cost": {"class": "free_local"}}, "empty hint")
    eq(len(t.run.calls(t.ollama)), 1, "ollama upstream")


@case("routing", gw=None)
def empty_hint_no_free_local(t):
    """No free_local model: gateway.api_explicit_only; API is never chosen without a name."""
    models = [m for m in t.run.config()["models"] if m["provider"] not in ("vllm", "ollama")]
    gw = t.gateway("api-only", t.run.config(models=models), env={"VLLM_ENDPOINT": "http://127.0.0.1:1", "OLLAMA_ENDPOINT": "http://127.0.0.1:1"})
    b = gw.call(model_call("")).json
    refused(t, b, "gateway.api_explicit_only")


@case("routing")
def resolved_model_echoed(t):
    """The resolved name is echoed and sent upstream; provider named."""
    for name, mock, provider in (("vllm-big", t.vllm, "vllm"), ("ollama-small", t.ollama, "ollama"), ("vllm-extra", t.vllm, "vllm")):
        b = t.gw.call(model_call(name)).json
        sub(b, {"status": "ok", "model": name, "provider": provider}, name)
        eq(t.run.calls(mock)[-1]["json"]["model"], name, name + " upstream model")


@case("routing")
def named_unavailable_dispatches(t):
    """Named dispatch: inventory membership is the gate, not discovery availability (vllm-absent is configured but not served)."""
    b = t.gw.call(model_call("vllm-absent")).json
    sub(b, {"status": "ok", "model": "vllm-absent"}, "vllm-absent")
    eq(t.run.calls(t.vllm)[-1]["json"]["model"], "vllm-absent", "upstream model")


@case("routing")
def no_fallback_on_failure(t):
    """A failing named model returns an error; no other model or provider is tried."""
    t.vllm.chat["vllm-big"] = {"json": {"error": "boom"}, "status": 500}
    b = t.gw.call(model_call("vllm-big")).json
    sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "retryable_gateway", "code": "gateway.provider_transient"}}, "500")
    eq([c["json"]["model"] for c in t.run.calls(t.vllm)], ["vllm-big"] * 3, "vllm requests (gateway retries, no model fallback)")
    eq(t.run.calls(t.ollama), [], "ollama requests")
    no_egress(t)


@case("routing")
def cli_entry_refused(t):
    """A CLI inventory entry on model.call is gateway.provider_unsupported; no child, no upstream."""
    refused(t, t.gw.call(model_call("claude-code")).json, "gateway.provider_unsupported")


@case("routing")
def api_without_credential(t):
    """An API model with no credential (store off): gateway.no_provider_credential, no egress at all."""
    for name in ("gpt-spec", "claude-spec"):
        refused(t, t.gw.call(model_call(name)).json, "gateway.no_provider_credential")
    eq(t.proxy.seen.items, [], "egress attempts")


@case("routing")
def mock_provider(t):
    """The compiled-in mock provider answers a configured mock model with its fixed text and token counts."""
    b = t.gw.call(model_call("mock-echo")).json
    sub(b, {"status": "ok", "provider": "mock", "model": "mock-echo",
            "message": {"role": "assistant", "content": "mock provider response", "stop_reason": "end_turn"},
            "usage": {"input_tokens": 20, "output_tokens": 30},
            "cost": {"amount": "0.000000", "class": "api", "est_usd": 0, "charged_usd": 0}}, "mock")


@case("routing")
def system_messages_hoisted(t):
    """Every system message is hoisted, joined by a blank line, into one leading system message upstream."""
    msgs = [{"role": "system", "content": "rule one"}, {"role": "user", "content": "q"},
            {"role": "system", "content": "rule two"}, {"role": "system", "content": ""}]
    t.gw.call(model_call("local-coder", messages=msgs))
    eq(t.run.calls(t.vllm)[-1]["json"]["messages"], [{"role": "system", "content": "rule one\n\nrule two"}, {"role": "user", "content": "q"}], "upstream messages")


@case("routing")
def only_system_messages(t):
    """Only system messages: accepted (non-empty array), sent with no user turn."""
    b = t.gw.call(model_call("local-coder", messages=[{"role": "system", "content": "s"}])).json
    eq(b["status"], "ok", "status")
    eq(t.run.calls(t.vllm)[-1]["json"]["messages"], [{"role": "system", "content": "s"}], "upstream messages")


@case("routing")
def upstream_reply_model_ignored(t):
    """The echoed model is the resolved name, whatever model the upstream reply names."""
    t.vllm.chat["local-coder"] = {"json": upstreams.chat_completion(model="something-else")}
    eq(t.gw.call(model_call("local-coder")).json["model"], "local-coder", "model")


@case("routing", gw=None)
def rate_limits_parked_on_agent_route(t):
    """CSAGENT_RATE_LIMIT_* does not apply to model.call (unified agent rate limiting is parked): three calls at 1/min all dispatch."""
    gw = t.gateway("ratelimit", env={"CSAGENT_RATE_LIMIT_OLLAMA": "1"})
    eq([gw.call(model_call("ollama-small")).json["status"] for _ in range(3)], ["ok", "ok", "ok"], "statuses")
    eq(len(t.run.calls(t.ollama)), 3, "upstream calls")


@case("routing", gw=None)
def duplicate_discovered_name(t):
    """Observed (open, Overnight G5 1): the same name discovered on Ollama and vLLM, absent from models.json, resolves deterministically to Ollama rather than failing closed; /v1/models lists both rows."""
    t.ollama.tags = ["ollama-small", "dupe"]
    t.vllm.models = t.vllm.models + ["dupe"]
    gw = t.gateway("dupe")
    eq(sorted(m["provider"] for m in gw.get("/v1/models").json["models"] if m["name"] == "dupe"), ["ollama", "vllm"], "rows")
    sub(gw.call(model_call("dupe")).json, {"status": "ok", "provider": "ollama", "model": "dupe"}, "resolution")
    eq((len(t.run.calls(t.ollama)), len(t.run.calls(t.vllm))), (1, 0), "upstream calls")
