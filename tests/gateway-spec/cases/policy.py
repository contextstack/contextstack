"""Provider evolution, policy side (V2.1 phase 5): capability and
egress intersection (protocol ∩ endpoint ∩ local ∩ tenant, denial wins),
gateway.policy_denied before any permit or connection (unary and stream),
discovery skipping denied hosts, cross-host redirect refusal, and offline
fail-closed to loopback when the tenant policy is missing, invalid or
expired. The tenant layer is a temp file named by CSGATEWAY_POLICY."""
import datetime
import json
import signal

from harness.gateway import model_call
from harness.run import case, eq, no_egress, sub
from harness import upstreams
from cases.catalog import BOX_MODEL, box, catalog, endpoints, held_call, ov, overlay_dir, revision, write

TENANT = "csgateway.policy.v1"
TOOL = [{"name": "fs_read", "description": "read a file", "schema": {"type": "object"}}]


def iso(dt):
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def tenant(revision=1, expires_in_s=3600, **rules):
    now = datetime.datetime.now(datetime.timezone.utc)
    doc = {"schema": TENANT, "tenant": "spec", "revision": revision, "issued_at": iso(now - datetime.timedelta(seconds=60)),
           "expires_at": iso(now + datetime.timedelta(seconds=expires_in_s))}
    doc.update(rules)
    return doc


def policy_file(t, name, doc):
    p = t.run.tmp / ("policy-%s.json" % name)
    if doc is not None:
        p.write_text(doc if isinstance(doc, str) else json.dumps(doc))
    return p


def denied(t, b, what):
    sub(b, {"status": "error", "cost": {"currency": "USD", "amount": "0.000000"},
            "error": {"class": "fatal_session", "code": "gateway.policy_denied", "recoverable": False}}, what)


def anthropic_posts(t):
    return t.run.calls(t.anthropic, "/v1/messages")


def cloud_gateway(t, name, **env):
    store = t.run.credential_store(name, anthropic="sk-ant-spec-FAKE", openai="sk-spec-FAKE")
    return t.gateway(name, env=dict(env, CSGATEWAY_CREDENTIALS=str(store)))


@case("policy", gw=None)
def capability_denied_before_permit(t):
    """A local policy denying tools refuses a tool-bearing call with gateway.policy_denied at $0, before admission: no upstream request, no permit taken, even while the deployment is full."""
    d = overlay_dir(t, "nocaps", {"p.json": ov(policy={"capabilities": {"tools": False}})})
    gw = t.gateway("pol-tools", t.run.config(deployments=[t.run.vllm_deployment()]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    t.vllm.chat["local-coder"] = {"hold": "full", "then": {"json": upstreams.chat_completion()}}
    th, out = held_call(gw, model_call("local-coder", request_id="holder"))
    t.vllm.events.wait(lambda ev: any(e.get("held") == "full" for e in ev))
    denied(t, gw.call(model_call("vllm-big", tool_schema=TOOL)).json, "tools while the deployment is full")
    eq(gw.admission()["deployments"]["vllm-spec"], {"active": 1, "queued": 0}, "no permit or queue slot taken")
    t.vllm.release("full")
    th.join(10)
    eq(out["r"].json["status"], "ok", "holder")
    eq([c["json"]["model"] for c in t.run.calls(t.vllm)], ["local-coder"], "upstream requests")
    eq(gw.call(model_call("vllm-big")).json["status"], "ok", "without tools")
    eq(catalog(gw)["policy"]["local_layers"], 1, "local layers")
    sub(endpoints(gw)["vllm"], {"capabilities": {"images": False, "tools": False, "streaming": True}}, "effective vllm")


@case("policy", gw=None)
def endpoint_declaration_narrows(t):
    """An endpoint's declared capabilities narrow it: an overlay endpoint declared without tools refuses tool calls with gateway.policy_denied and no request."""
    m, url = t.mock("notools")
    d = overlay_dir(t, "decl", {"box.json": ov([box(url, capabilities={"images": False, "tools": False, "streaming": False})])})
    gw = t.gateway("pol-decl", t.run.config(extra_models=[BOX_MODEL]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    denied(t, gw.call(model_call("box-model", tool_schema=TOOL)).json, "tools on an endpoint declared without them")
    eq(t.run.calls(m), [], "no upstream request")
    eq(gw.call(model_call("box-model")).json["status"], "ok", "without tools")


@case("policy", gw=None)
def intersection_denial_wins(t):
    """Egress is local ∩ tenant by host and capability is protocol ∩ endpoint ∩ local ∩ tenant: a host the local layer allows but the tenant denies is denied; a host outside a local allow list is denied; loopback still serves."""
    d = overlay_dir(t, "both", {"p.json": ov(policy={"egress": {"allow_hosts": ["loopback", "*.anthropic.com"]}})})
    pol = policy_file(t, "both", tenant(egress={"deny_hosts": ["api.anthropic.com"]}, capabilities={"tools": False}))
    gw = cloud_gateway(t, "pol-both", CSGATEWAY_PROVIDERS_DIR=str(d), CSGATEWAY_POLICY=str(pol))
    denied(t, gw.call(model_call("claude-spec")).json, "tenant denies what local allows")
    denied(t, gw.call(model_call("gpt-spec")).json, "outside the local allow list")
    eq(gw.call(model_call("local-coder")).json["status"], "ok", "loopback")
    denied(t, gw.call(model_call("local-coder", tool_schema=TOOL)).json, "tenant denies tools")
    eq((anthropic_posts(t), t.run.calls(t.openai)), ([], []), "no cloud request")
    e = endpoints(gw)
    eq({k: e[k]["egress"] == "allowed" for k in ("anthropic", "openai", "vllm", "ollama")},
       {"anthropic": False, "openai": False, "vllm": True, "ollama": True}, "reported egress")
    eq(e["anthropic"]["capabilities"], {"images": True, "tools": False, "streaming": True}, "effective anthropic capabilities")
    sub(catalog(gw)["policy"], {"state": "valid", "tenant": "spec", "revision": 1, "local_layers": 1}, "policy")
    no_egress(t)


@case("policy", gw=None)
def stream_denied_before_connection(t):
    """A policy denying streaming refuses a stream request with one SSE error chunk (gateway.policy_denied, $0, not streaming_unsupported: the endpoint could) and no upstream request; /v1/models says streaming false; unary to the same endpoint serves."""
    d = overlay_dir(t, "nostream", {"p.json": ov(policy={"capabilities": {"streaming": False}})})
    gw = cloud_gateway(t, "pol-stream", CSGATEWAY_PROVIDERS_DIR=str(d))
    r = gw.call(model_call("claude-spec", stream=True))
    eq((r.status, r.headers.get("content-type", "").split(";")[0]), (200, "text/event-stream"), "stream response")
    frames = r.sse()
    eq(len(frames), 1, "one frame")
    sub(frames[0], {"schema_version": "agent.gateway.model_call.chunk.v2", "kind": "error", "cost": {"amount": "0.000000"},
                    "error": {"class": "fatal_session", "code": "gateway.policy_denied"}}, "error chunk")
    eq(anthropic_posts(t), [], "no upstream request for the stream")
    eq({r["name"]: r["streaming"] for r in gw.get("/v1/models").json["models"]}.get("claude-spec"), False, "/v1/models reports the policy")
    eq(gw.call(model_call("claude-spec")).json["status"], "ok", "unary")
    no_egress(t)


@case("policy", gw=None)
def discovery_skips_denied_host(t):
    """Discovery never contacts a denied host: the endpoint is reported static with an egress-denied reason, and its models refuse with gateway.policy_denied."""
    m, url = t.mock("denied-box")
    url = url.replace("127.0.0.1", "localhost")
    d = overlay_dir(t, "skip", {"box.json": ov([box(url)], policy={"egress": {"deny_hosts": ["localhost"]}})})
    gw = t.gateway("pol-skip", t.run.config(extra_models=[BOX_MODEL]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    h = {e["endpoint_id"]: e for e in gw.get("/v1/discovery/health").json["endpoints"]}
    eq((h["spec-box"]["mode"], "egress denied" in h["spec-box"].get("reason", "")), ("static", True), "spec-box discovery: %r" % h["spec-box"])
    denied(t, gw.call(model_call("box-model")).json, "box-model")
    eq(m.requests.items, [], "denied host never contacted")
    eq(gw.call(model_call("local-coder")).json["status"], "ok", "127.0.0.1 is not localhost by name")


@case("policy")
def cross_host_redirect_refused(t):
    """An upstream redirect to another host is refused (egress was decided for the endpoint's host): a classed $0 error, nothing reaches the other host; a same-host redirect is followed."""
    m, url = t.mock("redirect-target")
    t.vllm.chat["vllm-big"] = {"redirect": url + "/v1/chat/completions", "status": 307}
    t.vllm.chat["vllm-extra"] = {"redirect": "http://elsewhere.invalid/v1/chat/completions", "status": 307}
    for name in ("vllm-big", "vllm-extra"):
        b = t.gw.call(model_call(name)).json
        sub(b, {"status": "error", "cost": {"amount": "0.000000"}}, name)
        eq(bool(b["error"].get("class")), True, name + " classed")
    eq(m.requests.items, [], "other loopback host never reached")
    no_egress(t)
    t.vllm.chat["local-coder"] = {"redirect": "/v1/moved", "status": 307}
    t.vllm.routes[("POST", "/v1/moved")] = {"json": upstreams.chat_completion(text="moved")}
    try:
        sub(t.gw.call(model_call("local-coder")).json, {"status": "ok", "message": {"content": "moved"}}, "same-host redirect")
    finally:
        del t.vllm.routes[("POST", "/v1/moved")]


def offline(t, gw, state):
    sub(catalog(gw)["policy"], {"state": state}, "policy state")
    eq(gw.call(model_call("local-coder")).json["status"], "ok", state + ": loopback serves")
    denied(t, gw.call(model_call("claude-spec")).json, state + ": cloud")
    denied(t, gw.call(model_call("gpt-spec")).json, state + ": cloud")
    eq((anthropic_posts(t), t.run.calls(t.openai)), ([], []), state + ": no cloud request")
    e = endpoints(gw)
    eq({k: e[k]["egress"] == "allowed" for k in ("anthropic", "openai", "vllm", "ollama", "mock")},
       {"anthropic": False, "openai": False, "vllm": True, "ollama": True, "mock": True}, state + ": reported egress")
    no_egress(t)


@case("policy", gw=None)
def offline_missing_policy_loopback_only(t):
    """A configured tenant policy that is missing fails closed: loopback endpoints serve, every other endpoint is denied (never the embedded defaults)."""
    offline(t, cloud_gateway(t, "pol-missing", CSGATEWAY_POLICY=str(policy_file(t, "missing", None))), "missing")


@case("policy", gw=None)
def offline_invalid_policy_loopback_only(t):
    """An invalid tenant policy (no expires_at; or not strictly decodable) fails closed to loopback, like a missing one."""
    doc = tenant()
    del doc["expires_at"]
    gw = cloud_gateway(t, "pol-invalid", CSGATEWAY_POLICY=str(policy_file(t, "invalid", doc)))
    eq(catalog(gw)["policy"]["state"].startswith("invalid"), True, "state")
    offline(t, gw, catalog(gw)["policy"]["state"])
    gw2 = cloud_gateway(t, "pol-unknown-field", CSGATEWAY_POLICY=str(policy_file(t, "unknown", dict(tenant(), colour="red"))))
    eq(catalog(gw2)["policy"]["state"].startswith("invalid"), True, "unknown field")
    code, log = t.gateway("pol-relative", env={"CSGATEWAY_POLICY": "policy.json"}, expect_start=False).start_fails()
    eq((code != 0, "CSGATEWAY_POLICY" in log), (True, True), "relative policy path refused")


@case("policy", gw=None, fast=False)
def offline_expiry_during_run(t):
    """Validity is checked on every call against the clock: a valid tenant policy admits the cloud endpoint, and after expires_at the same process admits loopback only, with no reload."""
    gw = cloud_gateway(t, "pol-expiry", CSGATEWAY_POLICY=str(policy_file(t, "expiry", tenant(expires_in_s=3))))
    sub(catalog(gw)["policy"], {"state": "valid"}, "before")
    eq(gw.call(model_call("claude-spec")).json["status"], "ok", "cloud before expiry")
    gw.wait(lambda: catalog(gw)["policy"]["state"] == "expired", timeout=6, what="expiry")
    t.anthropic.requests.clear()
    offline(t, gw, "expired")


@case("policy", gw=None)
def tenant_reload_rejects(t):
    """A tenant reload is rejected, keeping the held policy, on a rollback (lower revision), the same revision with different content, or a missing/corrupt file; a higher revision publishes."""
    p = policy_file(t, "reload", tenant(revision=2))
    gw = t.gateway("pol-reload", env={"CSGATEWAY_POLICY": str(p)})
    r1 = revision(gw)
    for what, doc in (("rollback", tenant(revision=1)), ("same revision, new content", tenant(revision=2, egress={"deny_hosts": ["example.com"]})),
                      ("corrupt", "{broken"), ("missing", None)):
        before = catalog(gw)["last_reload"]
        if doc is None:
            p.unlink()
        else:
            p.write_text(doc if isinstance(doc, str) else json.dumps(doc))
        gw.send(signal.SIGHUP)
        c = gw.wait(lambda: (lambda c: c if c["last_reload"] != before and not c["last_reload"]["ok"] else None)(catalog(gw)), timeout=5, what=what + " rejected")
        sub(c, {"revision": r1, "policy": {"state": "valid", "revision": 2}}, what + ": held policy kept")
    p.write_text(json.dumps(tenant(revision=3)))
    gw.send(signal.SIGHUP)
    gw.wait(lambda: catalog(gw)["policy"].get("revision") == 3, timeout=5, what="revision 3 published")
    eq((revision(gw) != r1, catalog(gw)["last_reload"]["ok"]), (True, True), "published")
