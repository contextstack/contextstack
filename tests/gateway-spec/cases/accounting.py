"""Accounting: decimal amount string, estimate versus charged, the three cost
classes, provider-reported charges, $0 on every error, budget fields accepted
not enforced except budget.max_usd on api calls (area spend)."""
import json

from harness.gateway import model_call
from harness.run import absent, case, eq, sub
from harness import upstreams


def cost(t, name, usage=(3, 2), charged=None, mock=None, **fields):
    (mock or t.vllm).chat[name] = {"json": upstreams.chat_completion(usage=usage, cost=charged)}
    # Reservation prices max_output; raise the request ceiling so settlement stays within it.
    b = t.gw.call(model_call(name, budget={"max_output_tokens": max(usage[1], 4096)}, **fields)).json
    eq(b["status"], "ok", name + " status")
    return b


@case("accounting")
def api_class(t):
    """api: amount = cost_per_1k * (in+out) / 1000 as a 6-dp string; est_usd = amount as a number; charged_usd = est_usd."""
    b = cost(t, "fake-paid", usage=(1200, 34))
    eq(b["usage"], {"input_tokens": 1200, "output_tokens": 34}, "usage")
    eq(b["cost"], {"currency": "USD", "amount": "0.012340", "class": "api", "est_usd": 0.01234, "charged_usd": 0.01234}, "cost")


@case("accounting")
def subscription_class(t):
    """subscription: est_usd carries the API-equivalent amount, charged_usd 0."""
    b = cost(t, "fake-sub", usage=(500, 500))
    eq(b["cost"], {"currency": "USD", "amount": "0.020000", "class": "subscription", "est_usd": 0.02, "charged_usd": 0}, "cost")


@case("accounting")
def free_local_class(t):
    """free_local with no price: amount 0.000000, est_usd 0, charged_usd 0."""
    b = cost(t, "vllm-big", usage=(1000, 1000))
    eq(b["cost"], {"currency": "USD", "amount": "0.000000", "class": "free_local", "est_usd": 0, "charged_usd": 0}, "cost")


@case("accounting")
def ollama_shadow_price(t):
    """Ollama models without cost_per_1k take the catalog shadow price (0.002/1k) as an estimate; still free_local, charged 0."""
    b = cost(t, "ollama-small", usage=(400, 100), mock=t.ollama)
    eq(b["cost"], {"currency": "USD", "amount": "0.001000", "class": "free_local", "est_usd": 0.001, "charged_usd": 0}, "cost")


@case("accounting")
def rounding(t):
    """The amount string rounds to 6 places; est_usd is that string's value."""
    b = cost(t, "fake-paid", usage=(1, 0))  # 0.01 * 1 / 1000 = 0.00001
    eq(b["cost"]["amount"], "0.000010", "amount")
    b = cost(t, "fake-paid", usage=(0, 0))
    eq(b["cost"], {"currency": "USD", "amount": "0.000000", "class": "api", "est_usd": 0, "charged_usd": 0}, "zero tokens")


@case("accounting")
def provider_reported_charge(t):
    """usage.cost from the provider: charged and charged_usd carry it; est_usd stays the estimate."""
    from cases.spend import spent

    before, _ = spent(t.gw)
    b = cost(t, "fake-paid", usage=(100, 100), charged=0.5)
    eq(b["cost"], {"currency": "USD", "amount": "0.002000", "class": "api", "charged": 0.5, "est_usd": 0.002, "charged_usd": 0.5, "over_reservation": True}, "cost")
    b = cost(t, "vllm-big", usage=(100, 100), charged=0.25, run_id="reported-local")
    eq(b["cost"], {"currency": "USD", "amount": "0.000000", "class": "free_local", "charged": 0.25, "est_usd": 0, "charged_usd": 0.25}, "free_local with a reported charge")
    b = cost(t, "fake-sub", usage=(100, 100), charged=0.125, run_id="reported-sub")
    sub(b["cost"], {"class": "subscription", "charged": 0.125, "charged_usd": 0.125}, "subscription charge")
    for run_id, charge in [("reported-local", 0.25), ("reported-sub", 0.125)]:
        path = t.gw.home / ".contextstack/gateway/traces" / (run_id + ".jsonl")
        spans = [json.loads(line) for line in path.read_text().splitlines()]
        settlements = [span["attributes"] for span in spans if span["kind"] == "settlement"]
        eq(len(settlements), 1, run_id + " settlement count")
        sub(settlements[0], {"charged_usd": charge, "basis": "provider_reported", "partial": False}, run_id)
    eq(spent(t.gw)[0], round(before + 0.875, 6), "all three routes debit the provider budget")
    b = cost(t, "fake-paid", usage=(100, 100), charged=0)
    eq(b["cost"], {"currency": "USD", "amount": "0.002000", "class": "api", "charged": 0, "est_usd": 0.002, "charged_usd": 0}, "reported zero charge wins")


@case("accounting", gw=None)
def subscription_stream_fault_preserves_reported_charge(t):
    """A charge received before the stream fails still settles against spend."""
    from cases.spend import spent

    gw = t.gateway("acct-subscription-fault", limits=None)
    frames = upstreams.chat_stream(["hello"], usage=(3, 2), cost=0.25)[:-1]
    t.vllm.chat["fake-sub"] = {"sse": frames + [{"cut": True}]}
    got = gw.call(model_call("fake-sub", stream=True, run_id="sub-fault")).sse()
    eq(got[-1]["kind"], "error", "terminal fault")
    sub(got[-1]["cost"], {"charged": 0.25, "charged_usd": 0.25}, "terminal cost")
    eq(spent(gw), (0.25, 0.25), "reported charge debited once")
    path = gw.home / ".contextstack/gateway/traces/sub-fault.jsonl"
    spans = [json.loads(line) for line in path.read_text().splitlines()]
    settlements = [span["attributes"] for span in spans if span["kind"] == "settlement"]
    eq(len(settlements), 1, "settlement count")
    sub(settlements[0], {"basis": "provider_reported", "partial": True, "charged_usd": 0.25}, "partial settlement")


@case("accounting")
def negative_charge_refused(t):
    """A negative provider charge is an error ($0), never a credit."""
    t.vllm.chat["fake-paid"] = {"json": upstreams.chat_completion(cost=-1)}
    b = t.gw.call(model_call("fake-paid")).json
    sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "runtime_fault", "code": "gateway.transport_failed",
                                                                      "message": "vllm: negative provider charge"}}, "negative")


@case("accounting")
def errors_cost_nothing(t):
    """WIRE clause 5 and the $0 rule: errors carry amount 0.000000, no class, no est_usd/charged_usd, zero usage."""
    t.vllm.chat["fake-paid"] = {"json": {}, "status": 500}
    for what, body in (("upstream 500", model_call("fake-paid")), ("unknown model", model_call("nope")),
                       ("cli", model_call("claude-code"))):
        b = t.gw.call(body).json
        eq(b["cost"], {"currency": "USD", "amount": "0.000000"}, what + " cost")
        eq(b["usage"], {"input_tokens": 0, "output_tokens": 0}, what + " usage")
        absent(b, ["message", "tool_calls", "model", "provider"], what)


@case("accounting")
def budget_fields_not_enforced(t):
    """max_input_tokens is accepted and not enforced; budget.max_usd bounds only api calls (area spend), so a tiny one on free_local still dispatches."""
    b = t.gw.call(model_call("local-coder", budget={"max_usd": "0.0000001", "max_input_tokens": 1})).json
    eq(b["status"], "ok", "status")
    eq(len(t.run.calls(t.vllm)), 1, "upstream requests")
    eq("max_usd" in t.run.calls(t.vllm)[0]["json"], False, "nothing budget-shaped sent upstream")


@case("accounting", gw=None)
def stream_partial_output_settles_on_fault(t):
    """After streamed text, a transport fault settles priced tokens once and reports that charge on its terminal error chunk."""
    from cases.spend import limits, spent

    gw = t.gateway("acct-partial-stream", limits=limits(1))
    frames = upstreams.chat_stream(["hello", " world"], terminal=False) + [{"cut": True}]
    t.vllm.default_chat = {"sse": frames}
    got = gw.call(model_call("fake-paid", stream=True, budget={"max_usd": "1"})).sse()
    eq([f["kind"] for f in got], ["delta", "delta", "error"], "kinds")
    day, _ = spent(gw)
    eq(day > 0, True, "source spend counted")
    eq("partial=1" in gw.log_text(), True, "partial USAGE")
    eq(len(t.run.calls(t.vllm)), 1, "no retry after output started")
    eq(round(got[-1]["cost"].get("charged_usd"), 6), day, "terminal charge equals settled spend")


@case("accounting", gw=None)
def paid_stream_deadline_after_output_reports_settled_charge(t):
    """A paid stream that exceeds its deadline after output settles once and reports the settled charge on its terminal error chunk."""
    from cases.spend import limits, spent
    from cases.upstream_retry import usage_rows

    request_id = "paid-deadline-after-output"
    gw = t.gateway("acct-paid-stream-deadline", limits=limits(1))
    frames = upstreams.chat_stream(["hello"], terminal=False) + [{"sleep": 1}]
    t.vllm.default_chat = {"sse": frames}
    got = gw.call(model_call(
        "fake-paid",
        stream=True,
        request_id=request_id,
        budget={"max_usd": "1", "timeout_ms": 200},
    )).sse()
    eq([f["kind"] for f in got], ["delta", "error"], "kinds")
    day, _ = spent(gw)
    eq(day > 0, True, "source spend counted")
    eq(len(usage_rows(gw, request_id)), 1, "one partial settlement")
    eq(len(t.run.calls(t.vllm)), 1, "no retry after output started")
    eq(round(got[-1]["cost"].get("charged_usd"), 6), day, "terminal charge equals settled spend")

@case("accounting", gw=None)
def stream_client_disconnect_settles_partial(t):
    """A client that disconnects mid-stream still settles streamed output and releases the hold."""
    from cases.spend import POOL, budgets, limits, spent

    gw = t.gateway("acct-client-drop", limits=limits(1))
    frames = upstreams.chat_stream(["hello", " world"], terminal=False) + [{"sleep": 30}]
    t.vllm.default_chat = {"sse": frames}
    POOL.submit(gw.abandon, "/v1/agent/model.call", model_call("fake-paid", stream=True, budget={"max_usd": "1"}), 0.5)
    gw.wait(lambda: "partial=1" in gw.log_text(), timeout=5, what="partial settlement")
    # The spend record is written just after the USAGE line; wait for it rather than read once.
    gw.wait(lambda: spent(gw)[0] > 0, timeout=5, what="source spend counted")
    day, _ = spent(gw)
    eq(day > 0, True, "source spend counted")
    eq("partial=1" in gw.log_text(), True, "partial USAGE")
    _, rows = budgets(gw)
    eq(rows["vllm"]["budget"]["held_usd"], 0, "hold released")


@case("accounting", gw=None)
def stream_fault_before_output_no_settle_retries(t):
    """A transient open failure before any delta retries and settles only on success; a full retry exhaustion settles nothing."""
    from cases.spend import limits, spent
    from cases.upstream_retry import sequence

    gw = t.gateway("acct-stream-retry", limits=limits(1))
    fail = {"status": 503, "json": {"error": {"message": "down"}}}
    ok = {"json": upstreams.chat_completion(text="ok", usage=(5, 2))}
    t.vllm.default_chat = sequence([fail, ok])
    got = gw.call(model_call("fake-paid", stream=True)).sse()
    eq(got[-1]["kind"], "stop", "ok after one retry")
    eq(spent(gw)[0], 0.00007, "one settlement")
    eq("partial=1" in gw.log_text(), False, "no partial settlement")
    eq(len(t.run.calls(t.vllm)), 2, "two upstream opens")

    t.run.reset()
    gw2 = t.gateway("acct-stream-exhaust", limits=limits(1))
    t.vllm.default_chat = sequence([fail, fail, fail])
    gw2.call(model_call("fake-paid", stream=True))
    eq(spent(gw2)[0], 0, "nothing counted on error")
    eq(len(t.run.calls(t.vllm)), 3, "three upstream opens")


@case("accounting")
def budget_max_usd_must_be_string(t):
    """budget.max_usd is a decimal string on the wire; a JSON number does not decode (bad_request)."""
    b = t.gw.call(model_call("fake-paid", budget={"max_usd": 0.5})).json
    sub(b, {"status": "error", "error": {"code": "gateway.bad_request"}}, "numeric max_usd")
