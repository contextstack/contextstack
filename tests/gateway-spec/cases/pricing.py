"""Per-model rates, cache token pricing, reservation ceiling, and over-reservation settlement."""
from concurrent.futures import ThreadPoolExecutor

from harness import upstreams
from harness.gateway import model_call
from harness.run import case, eq, sub

from .spend import denied, limits, spent

ASYMMETRIC = {
    "name": "fake-asymmetric",
    "provider": "vllm",
    "tier": 3,
    "capability": "general",
    "cost_class": "api",
    "cost_per_1k": 0.01,
    "rates": {"input": 1.0, "output": 4.0, "cache_read": 0.1, "cache_write": 2.0},
}

POOL = ThreadPoolExecutor(max_workers=2)


def asymmetric_json(usage, cached_tokens=0):
    return upstreams.chat_completion(usage=usage, cached_tokens=cached_tokens)


def gw_asymmetric(t, name, lim):
    t.vllm.models.append("fake-asymmetric")
    cfg = t.run.config(extra_models=[ASYMMETRIC])
    return t.gateway(name, config=cfg, limits=lim)


@case("pricing", gw=None)
def asymmetric_rates(t):
    """Input and output price independently; blended cost_per_1k still prices entries without rates."""
    gw = gw_asymmetric(t, "pricing-asym", limits(1))
    t.vllm.chat["fake-asymmetric"] = {"json": asymmetric_json((1000, 500))}
    b = gw.call(model_call("fake-asymmetric")).json
    eq(b["cost"]["amount"], "0.003000", "input 1/M + output 4/M")
    eq(b["cost"]["charged_usd"], 0.003, "wallet")


@case("pricing", gw=None)
def cache_tokens_priced(t):
    """OpenAI-style cached_tokens are billed at cache_read rate (fresh prompt at input rate)."""
    gw = gw_asymmetric(t, "pricing-cache", limits(1))
    t.vllm.chat["fake-asymmetric"] = {"json": asymmetric_json((3000, 500), cached_tokens=2000)}
    b = gw.call(model_call("fake-asymmetric")).json
    eq(b["cost"]["amount"], "0.003200", "cache read at 0.1/M")


@case("pricing", gw=None)
def reservation_covers_worst_case(t):
    """Admission holds input estimate at input rate plus max_output at output rate (not blended)."""
    gw = gw_asymmetric(t, "pricing-reserve", limits(10))
    t.vllm.chat["fake-asymmetric"] = {"json": asymmetric_json((10, 10))}
    b = gw.call(model_call("fake-asymmetric", budget={"max_output_tokens": 500, "max_usd": "0.003"})).json
    eq(b["status"], "ok", "under ceiling")
    denied(
        gw.call(model_call("fake-asymmetric", budget={"max_output_tokens": 2500, "max_usd": "0.01"})).json,
        "output ceiling dominates",
        "ceiling",
    )


@case("pricing", gw=None)
def over_reservation_flagged(t):
    """When actual output exceeds the sampling ceiling priced at admission, settlement beats reservation and is flagged."""
    gw = gw_asymmetric(t, "pricing-over", limits(1))
    t.vllm.chat["fake-asymmetric"] = {"json": asymmetric_json((100, 5000))}
    b = gw.call(model_call("fake-asymmetric", budget={"max_output_tokens": 16, "max_usd": "1"})).json
    eq(b["status"], "ok", "settles")
    eq(b["cost"]["over_reservation"], True, "flagged")
    eq(b["cost"]["est_usd"], 0.0201, "true list price")
    eq(spent(gw), (0.0201, 0.0201), "budget sees the real total")


@case("pricing", gw=None)
def concurrent_holds_use_reservations(t):
    """Two reservations at once cannot jointly exceed the budget."""
    gw = gw_asymmetric(t, "pricing-concurrent", limits(0.05))
    big = {"max_output_tokens": 8000, "max_usd": "1"}
    t.vllm.chat["fake-asymmetric"] = {"hold": "paid", "then": {"json": asymmetric_json((100, 100))}}
    first = POOL.submit(gw.call, model_call("fake-asymmetric", budget=big, request_id="first"))
    t.vllm.events.wait(lambda ev: ev, 5)
    b = gw.call(model_call("fake-asymmetric", budget=big, request_id="second")).json
    denied(b, "second in flight", "exhausted", spent_usd=0)
    eq(b["error"]["budget"]["held_usd"] >= 0.032, True, "first hold is the reservation")
    t.vllm.release("paid")
    sub(first.result(15).json, {"status": "ok"}, "first")
    eq(spent(gw)[0] > 0, True, "settled charge")
