"""Admission: atomic reservation, bounded queue, queue_full and queue_timeout,
cancellation while queued and in flight, deadlines, shared host capacity,
stream permits, idempotent release (no leaked capacity), timing rows.

Shared gateway "admit": deployment vllm-spec (all vLLM fixture models)
max_concurrent 1, max_queued 1, max_wait_ms 5000; cli-spec for work orders."""
import concurrent.futures
import time

from harness.gateway import model_call
from harness.run import case, eq, sub
from harness import upstreams

POOL = concurrent.futures.ThreadPoolExecutor(max_workers=8)


def state(gw, dep="vllm-spec"):
    return gw.admission()["deployments"][dep]


def idle(gw, dep="vllm-spec", timeout=8):
    gw.wait(lambda: state(gw, dep) == {"active": 0, "queued": 0}, timeout, dep + " idle")


def hold(t, model="local-coder", gate="g"):
    t.vllm.chat[model] = {"hold": gate, "then": {"json": upstreams.chat_completion(model=model)}}


def capacity_refusal(b, reason, dep="vllm-spec"):
    sub(b, {"status": "error", "cost": {"currency": "USD", "amount": "0.000000"}, "usage": {"input_tokens": 0, "output_tokens": 0},
            "error": {"class": "retryable_gateway", "code": "gateway.capacity_exhausted", "recoverable": True}}, reason)
    eq(b["error"]["message"], "%s: %s" % (reason, dep), reason + " message")


@case("admission", gw="admit")
def admission_report(t):
    """/v1/admission and /v1/models expose estate, per-deployment capacity and live state."""
    b = t.gw.admission()
    sub(b, {"estate": "spec", "capacity": {"vllm-spec": 1, "cli-spec": 1},
            "deployments": {"vllm-spec": {"active": 0, "queued": 0}, "cli-spec": {"active": 0, "queued": 0}}}, "admission")
    eq([d["id"] for d in b["configuration"]["deployments"]], ["vllm-spec", "cli-spec"], "configuration echoed")
    m = t.gw.get("/v1/models").json
    sub(m, {"estate": "spec", "deployment_capacity": {"vllm-spec": 1, "cli-spec": 1}}, "/v1/models")


@case("admission", gw="admit")
def queue_then_full(t):
    """One active, one queued, the third refused queue_full at once with $0; the queued call runs when the first releases; nothing leaks."""
    hold(t)
    first = POOL.submit(t.gw.call, model_call("local-coder", request_id="a"))
    t.vllm.events.wait(lambda ev: ev, 5)
    second = POOL.submit(t.gw.call, model_call("local-coder", request_id="b"))
    t.gw.wait(lambda: state(t.gw) == {"active": 1, "queued": 1}, what="one active, one queued")
    started = time.monotonic()
    capacity_refusal(t.gw.call(model_call("vllm-big", request_id="c")).json, "queue_full")
    eq(time.monotonic() - started < 1, True, "queue_full is immediate")
    eq(len(t.run.calls(t.vllm)), 1, "only the admitted call reached upstream")
    t.vllm.release("g")
    eq([f.result(10).json["status"] for f in (first, second)], ["ok", "ok"], "held and queued calls complete")
    eq(len(t.run.calls(t.vllm)), 2, "upstream calls")
    idle(t.gw)


@case("admission", gw="admit")
def cancel_while_queued(t):
    """A queued caller that disconnects leaves the queue; its request never reaches upstream; capacity is not leaked."""
    hold(t)
    first = POOL.submit(t.gw.call, model_call("local-coder", request_id="a"))
    t.vllm.events.wait(lambda ev: ev, 5)
    gone = POOL.submit(t.gw.abandon, "/v1/agent/model.call", model_call("vllm-big", request_id="gone"), 0.6)
    t.gw.wait(lambda: state(t.gw)["queued"] == 1, what="queued")
    gone.result(5)
    t.gw.wait(lambda: state(t.gw) == {"active": 1, "queued": 0}, what="queue drained by the disconnect")
    t.vllm.release("g")
    eq(first.result(10).json["status"], "ok", "held call")
    idle(t.gw)
    eq([c["json"]["model"] for c in t.run.calls(t.vllm)], ["local-coder"], "upstream calls (abandoned call never dispatched)")
    eq(t.gw.call(model_call("vllm-big")).json["status"], "ok", "capacity usable afterwards")


@case("admission", gw="admit")
def cancel_in_flight(t):
    """A caller disconnecting while upstream holds the call: the upstream request is cancelled and the permit released."""
    hold(t)
    POOL.submit(t.gw.abandon, "/v1/agent/model.call", model_call("local-coder", request_id="gone"), 0.5)
    t.vllm.events.wait(lambda ev: any("disconnected" in e for e in ev), timeout=6)
    idle(t.gw)
    t.vllm.release("g")
    eq(t.gw.call(model_call("vllm-big")).json["status"], "ok", "capacity usable afterwards")


@case("admission", gw="admit")
def deadline_in_flight(t):
    """budget.timeout_ms expiring while upstream is slow: runtime_fault transport_failed near the deadline, upstream cancelled, permit released."""
    t.vllm.chat["local-coder"] = {"delay_s": 5, "json": upstreams.chat_completion()}
    started = time.monotonic()
    b = t.gw.call(model_call("local-coder", budget={"timeout_ms": 300})).json
    took = time.monotonic() - started
    sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "runtime_fault", "code": "gateway.transport_failed"}}, "deadline")
    eq("context deadline exceeded" in b["error"]["message"], True, "message names the deadline")
    eq(took < 2, True, "returned near the deadline (took %.2fs)" % took)
    t.vllm.events.wait(lambda ev: any("disconnected" in e for e in ev), timeout=5)
    idle(t.gw)


@case("admission", gw="admit")
def unbound_model_refused(t):
    """With admission configured, a model no deployment binds is refused (deployment_unbound), never dispatched around the gate."""
    b = t.gw.call(model_call("ollama-small")).json
    sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "retryable_gateway", "code": "gateway.capacity_exhausted",
                                                                      "message": "deployment_unbound: ollama/ollama-small"}}, "unbound")
    eq(t.run.calls(t.ollama), [], "upstream requests")


@case("admission", gw="admit")
def release_on_every_path(t):
    """Upstream 500, a refused tool history and an ok call each release exactly once: the gate returns to idle and admits again."""
    t.vllm.chat["vllm-big"] = {"json": {}, "status": 500}
    eq(t.gw.call(model_call("vllm-big")).json["error"]["code"], "gateway.provider_transient", "500")
    bad = [{"role": "user", "content": "q"}, {"role": "tool", "tool_call_id": "zz", "content": "x"}]
    eq(t.gw.call(model_call("local-coder", messages=bad)).json["error"]["code"], "gateway.transport_failed", "bad history")
    eq(t.gw.call(model_call("local-coder")).json["status"], "ok", "ok")
    idle(t.gw)
    rows = t.gw.log_rows("admission_timing")
    eq(len(rows) >= 3, True, "timing rows logged")
    r = rows[-1]
    eq(sorted(r), ["config_sha", "deployment_id", "event", "model", "per_call_latency_ms", "queue_wait_ms", "request_id", "scope"], "timing row keys")
    sub(r, {"deployment_id": "vllm-spec", "model": "local-coder", "request_id": "spec-req-1", "scope": "model_call",
            "config_sha": t.gw.admission()["config_sha"]}, "timing row")


@case("admission", gw=None)
def queue_timeout(t):
    """A queued call waits at most max_wait_ms, then capacity_exhausted queue_timeout with $0."""
    gw = t.gateway("qtimeout", t.run.config(deployments=[t.run.vllm_deployment(max_queued=1, max_wait_ms=300)]))
    hold(t)
    first = POOL.submit(gw.call, model_call("local-coder"))
    t.vllm.events.wait(lambda ev: ev, 5)
    started = time.monotonic()
    capacity_refusal(gw.call(model_call("vllm-big")).json, "queue_timeout")
    took = time.monotonic() - started
    eq(0.25 <= took < 1.5, True, "waited about max_wait_ms (took %.2fs)" % took)
    t.vllm.release("g")
    eq(first.result(10).json["status"], "ok", "held call")
    idle(gw)


@case("admission", gw=None)
def no_queue_configured(t):
    """max_queued 0: the second concurrent call is queue_full immediately."""
    gw = t.gateway("noqueue", t.run.config(deployments=[t.run.vllm_deployment(max_queued=0)]))
    hold(t)
    first = POOL.submit(gw.call, model_call("local-coder"))
    t.vllm.events.wait(lambda ev: ev, 5)
    capacity_refusal(gw.call(model_call("local-coder")).json, "queue_full")
    t.vllm.release("g")
    first.result(10)
    idle(gw)


@case("admission", gw=None)
def shared_host_capacity(t):
    """Two deployments on one host with capacity 1: a call on the second is refused while the first holds the host."""
    deps = [t.run.vllm_deployment("vllm-a", models=["local-coder"], host="box", max_concurrent=1),
            {"id": "vllm-b", "provider": "vllm", "models": ["vllm-big"], "host": "box", "max_concurrent": 1, "max_queued": 0, "max_wait_ms": 0}]
    gw = t.gateway("host", t.run.config(deployments=deps, deployment_hosts={"box": 1}))
    hold(t)
    first = POOL.submit(gw.call, model_call("local-coder"))
    t.vllm.events.wait(lambda ev: ev, 5)
    capacity_refusal(gw.call(model_call("vllm-big")).json, "queue_full", dep="vllm-b")
    t.vllm.release("g")
    first.result(10)
    gw.wait(lambda: gw.admission()["deployments"] == {"vllm-a": {"active": 0, "queued": 0}, "vllm-b": {"active": 0, "queued": 0}}, what="idle")
    eq(gw.call(model_call("vllm-big")).json["status"], "ok", "second deployment admits once the host is free")


@case("admission", gw=None)
def estate_capacity_share(t):
    """capacity shares: this estate's share is its concurrency limit (2 of {spec: 2, other: 3})."""
    dep = t.run.vllm_deployment(max_concurrent=5, capacity={"spec": 2, "other": 3})
    gw = t.gateway("share", t.run.config(deployments=[dep], estate="spec"))
    eq(gw.admission()["capacity"], {"vllm-spec": 2}, "capacity")
    hold(t)
    held = [POOL.submit(gw.call, model_call("local-coder", request_id=str(i))) for i in range(2)]
    gw.wait(lambda: state(gw)["active"] == 2, what="two active")
    capacity_refusal(gw.call(model_call("vllm-big")).json, "queue_full")
    t.vllm.release("g")
    eq([f.result(10).json["status"] for f in held], ["ok", "ok"], "held calls")
    idle(gw)


@case("admission", gw=None, needs=("tls_intercept",))
def stream_holds_permit(t):
    """A stream holds its permit until the terminal event; a disconnect mid-stream releases it."""
    store = t.run.credential_store("stream-admit", anthropic="sk-ant-spec-FAKE")
    dep = {"id": "anthropic-spec", "provider": "anthropic", "endpoint": "https://api.anthropic.com", "models": ["claude-spec"],
           "max_concurrent": 1, "max_queued": 0, "max_wait_ms": 0}
    gw = t.gateway("stream-admit", t.run.config(deployments=[dep]), env={"CSGATEWAY_CREDENTIALS": str(store)})
    slow = upstreams.anthropic_stream(["a"], terminal=False) + [{"sleep": 0.8}] + upstreams.anthropic_stream(["b"])[2:]
    t.anthropic.messages = {"sse": slow}
    running = POOL.submit(gw.call, model_call("claude-spec", stream=True))
    gw.wait(lambda: state(gw, "anthropic-spec")["active"] == 1, what="stream active")
    capacity_refusal(gw.call(model_call("claude-spec")).json, "queue_full", dep="anthropic-spec")
    eq([f["kind"] for f in running.result(10).sse()], ["delta", "delta", "stop"], "stream completed")
    idle(gw, "anthropic-spec")
    endless = upstreams.anthropic_stream(["a"], terminal=False) + [{"sleep": 10}]
    t.anthropic.messages = {"sse": endless}
    POOL.submit(gw.abandon, "/v1/agent/model.call", model_call("claude-spec", stream=True), 0.5)
    gw.wait(lambda: state(gw, "anthropic-spec")["active"] == 1, what="second stream active")
    idle(gw, "anthropic-spec", timeout=6)
