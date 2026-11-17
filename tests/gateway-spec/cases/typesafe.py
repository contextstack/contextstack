"""TypeSafe typed questions (POST /v1/ai/typed-question): a keyed, pay-as-you-go source that is not a
chat model. The body is TypeSafe's own request, forwarded unchanged to the catalog endpoint `typesafe`
(`{endpoint}/systemone`, here an overlay patch to a loopback mock) with the stored key; the reply is
TypeSafe's status and body unchanged, priced at (input + output) tokens x the catalog rate ($0.042 per
million tokens) into the `typesafe` source budget. Refusals before any connection
are {"error": WIRE error}. Fake keys only; nothing leaves the machine."""
import json

from harness import upstreams
from harness.gateway import model_call
from harness.run import case, eq, no_egress, sub
from cases.catalog import ov, overlay_dir

PATH = "/v1/ai/typed-question"
KEY = "ts-spec-FAKE"
REQUEST = {"model": "jev-latest", "state": "I was charged twice for one order.",
           "questions": {"billing": {"type": "noul", "instructions": "Is this message about billing?"}}}
ANSWER = {"model": "jev-1.14.0", "answers": {"billing": {"type": "noul", "noul": 0.98}},
          "usage": {"input_tokens": 1000, "output_tokens": 1000}}
DEPLOY = {"id": "api-typesafe", "provider": "typesafe", "models": ["jev-latest"], "max_concurrent": 1,
          "max_queued": 0, "max_wait_ms": 0}


def mock(t, reply):
    m = upstreams.Mock("typesafe")
    m.routes[("POST", "/v1/systemone")] = reply
    listener = upstreams.Listener(m)
    t.owned_listeners = getattr(t, "owned_listeners", []) + [(m, listener)]
    return m, listener.url


def gateway(t, name, reply=None, limits=None, config=None, **env):
    """A gateway whose `typesafe` endpoint is a loopback mock, with a fake key in a file store. `config` may be a
    function of the mock's endpoint URL (a deployment must name the endpoint it binds)."""
    m, url = mock(t, reply or {"json": ANSWER})
    if callable(config):
        config = config(url + "/v1")
    d = overlay_dir(t, "ts-" + name, {"t.json": ov([{"name": "typesafe", "endpoint": url + "/v1"}])})
    store = t.run.credential_store("ts-" + name, typesafe=KEY)
    e = {"CSGATEWAY_PROVIDERS_DIR": str(d), "CSGATEWAY_CREDENTIALS": str(store)}
    e.update(env)
    kw = {} if limits is None else {"limits": limits}
    return t.gateway("ts-" + name, config, env=e, **kw), m, url


def calls(m):
    return [r for r in m.requests.items if r["path"] == "/v1/systemone"]


def spent(gw):
    try:
        s = json.loads(gw.spend_path.read_text())
    except FileNotFoundError:
        return None
    return round(s.get("sources", {}).get("typesafe", {}).get("day_usd", 0), 9)


def budget(usd):
    return {"sources": {"typesafe": {"usd": usd, "period": "day"}}}


@case("typesafe", gw=None)
def typed_question_forwarded_and_priced(t):
    """A TypeSafe request reaches {endpoint}/systemone byte for byte with the stored key as a bearer token; TypeSafe's
    answer comes back unchanged with X-ContextStack-Cost-Usd = (input + output) tokens x $0.042/M, counted into the source budget."""
    gw, m, _ = gateway(t, "ok", limits=budget(1))
    raw = json.dumps(REQUEST).encode()
    r = gw.post(PATH, raw=raw, headers={"X-ContextStack-Request-Id": "ts-1"})
    eq(r.status, 200, "status")
    eq(r.json, ANSWER, "answer unchanged")
    eq((r.headers.get("x-contextstack-cost-usd"), r.headers.get("x-contextstack-cost-class")), ("0.000084000", "api"), "cost headers")
    up = calls(m)
    eq(len(up), 1, "one upstream request")
    eq((up[0]["raw"], up[0]["headers"].get("authorization")), (raw, "Bearer " + KEY), "body unchanged, stored key")
    eq(spent(gw), 0.000084, "typesafe source spend")
    sub({s["source"]: s for s in gw.get("/v1/budgets").json["sources"]}.get("typesafe"), {"source": "typesafe"}, "budgets report")
    eq("USAGE provider=typesafe model=jev-latest in=1000 out=1000 cost=0.000084000 class=api req=ts-1" in gw.log_text(), True, "usage line")
    no_egress(t)


@case("typesafe", gw=None)
def upstream_error_passes_through_uncounted(t):
    """A TypeSafe error (422 validation) is answered with its status and body; nothing is counted."""
    detail = {"detail": [{"loc": ["body", "questions"], "msg": "field required", "type": "missing"}]}
    gw, m, _ = gateway(t, "err", reply={"json": detail, "status": 422}, limits=budget(1))
    r = gw.post(PATH, REQUEST)
    eq((r.status, r.json, r.headers.get("x-contextstack-cost-usd")), (422, detail, "0.000000000"), "error passed through")
    eq(spent(gw) in (None, 0), True, "nothing counted")


@case("typesafe", gw=None)
def refusals_before_any_connection(t):
    """Not a TypeSafe request (400), no budget for the source (402 policy_denied, budget off), a model the typesafe
    deployment does not bind (429), no credential store (503 no_provider_credential): each before any upstream request."""
    gw, m, _ = gateway(t, "refuse", limits={"sources": {"openrouter": {"usd": 5, "period": "day"}}})
    r = gw.post(PATH, {"model": "jev-latest", "messages": [{"role": "user", "content": "hi"}]})
    eq(r.status, 400, "not a typed question" + " status")
    sub(r.json, {"error": {"code": "gateway.bad_request"}}, "not a typed question")
    r = gw.post(PATH, REQUEST)
    eq(r.status, 402, "budget status")
    sub(r.json["error"], {"code": "gateway.policy_denied", "budget": {"reason": "off", "source": "typesafe"}}, "budget off")
    eq(calls(m), [], "no upstream request")

    gw2, m2, _ = gateway(t, "unbound", limits=budget(1),
                         config=lambda url: t.run.config(deployments=[dict(DEPLOY, endpoint=url)]))
    sub(gw2.post(PATH, REQUEST).json, ANSWER, "bound model answered")
    r = gw2.post(PATH, dict(REQUEST, model="jev-unbound"))
    eq(r.status, 429, "unbound model" + " status")
    sub(r.json, {"error": {"code": "gateway.capacity_exhausted"}}, "unbound model")
    eq(len(calls(m2)), 1, "no upstream request for the unbound model")

    nokey = t.gateway("ts-nokey", limits=budget(1))
    r = nokey.post(PATH, REQUEST)
    eq(r.status, 503, "no credential store" + " status")
    sub(r.json, {"error": {"code": "gateway.no_provider_credential"}}, "no credential store")
    no_egress(t)


@case("typesafe", gw=None)
def keyless_hosts_start_with_a_typesafe_deployment(t):
    """A typesafe deployment in models.json does not stop a host without its key from starting: neither with no store
    (CSGATEWAY_CREDENTIALS=off, a node) nor with a store narrowed to other providers (CSGATEWAY_KEYCHAIN_PROVIDERS)."""
    cfg = t.run.config(deployments=[dict(DEPLOY, endpoint="https://api.typesafe.ai/v1")])
    off = t.gateway("ts-keyless-off", cfg)
    eq(off.get("/v1/models", auth=False).status, 200, "store off: starts")
    eq(off.post(PATH, REQUEST).status, 503, "store off: refused")
    store = t.run.credential_store("ts-narrow", openai="sk-spec-FAKE")
    narrow = t.gateway("ts-keyless-narrow", cfg, env={"CSGATEWAY_CREDENTIALS": str(store), "CSGATEWAY_KEYCHAIN_PROVIDERS": "openai"})
    eq(narrow.get("/v1/models", auth=False).status, 200, "narrowed store: starts")
    eq(narrow.post(PATH, REQUEST).status, 503, "narrowed store: refused")
    no_egress(t)


@case("typesafe", gw=None)
def chat_never_reaches_typesafe(t):
    """A models.json chat model whose provider is typesafe is refused policy_denied by model.call; nothing is sent."""
    gw, m, _ = gateway(t, "chat", config=t.run.config(extra_models=[{"name": "jev-chat", "provider": "typesafe", "tier": 3,
                                                                     "capability": "general", "cost_per_1k": 0.000042}]))
    b = gw.call(model_call("jev-chat")).json
    sub(b, {"status": "error", "error": {"code": "gateway.policy_denied"}}, "model.call")
    eq(calls(m), [], "no upstream request")
