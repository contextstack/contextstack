"""Compare the Rust gateway's durable records with the approved run trace fixtures."""
from collections import Counter
from datetime import datetime
import json
from pathlib import Path
import re

from cases.catalog import BOX_MODEL, box, ov, overlay_dir
from cases.spend import spent
from cases.upstream_retry import sequence
from harness import upstreams
from harness.gateway import model_call
from harness.run import case, eq, sub

FIXTURES = Path(__file__).resolve().parents[3] / "tracing/fixtures"
TRACE_DIR = Path(".contextstack/gateway/traces")
IDENTITY = {"job_id": "job-1", "request_id": "req-1", "step_id": 1}
RETRY = {"status": 503, "headers": {"Retry-After": "1"}, "json": {"error": {"message": "retry"}}}
USAGE = (37, 12)
CHARGE = 0.02


def traced(scenario, model, send_index=1, **kw):
    return model_call(model, run_id="trace-" + scenario, send_index=send_index, **IDENTITY, **kw)


def compare(gw, scenario, costs=None, reverse=False):
    # Read immediately after the terminal response. Nothing waits for a log flush.
    run_id = "trace-" + scenario
    rows = [json.loads(line) for line in (gw.home / TRACE_DIR / (run_id + ".jsonl")).read_text().splitlines()]
    if reverse:
        rows.reverse()
    fixture = json.loads((FIXTURES / (scenario + ".json")).read_text())
    expected = [r for r in fixture["spans"] if r["attributes"]["machine"] == "gateway"]
    fixture_ids = {r["span_id"]: r for r in expected}

    def key(row, by_id):
        # Fixture children inherit their send index via their parents.
        attrs = row["attributes"]
        send = attrs.get("send_index")
        if send is None:
            return (key(by_id[row["parent_id"]], by_id)[0], row["kind"], row["outcome"])
        return (send, row["kind"], row["outcome"])

    by_id = {r["span_id"]: r for r in rows}
    eq(len(by_id), len(rows), "unique span IDs")
    eq(Counter(key(r, by_id) for r in rows), Counter(key(r, fixture_ids) for r in expected), "fixture record kinds")
    exchanges = {r["attributes"]["send_index"]: r for r in rows if r["kind"] == "gateway_exchange"}
    times = {}

    for row in rows:
        sid, kind, attrs = row["span_id"], row["kind"], row["attributes"]
        send, _, _ = key(row, by_id)
        eq(row["trace_id"], run_id, "trace ID")
        sub(attrs, {**IDENTITY, "run_id": run_id, "send_index": send}, "record identity")
        assert isinstance(attrs["machine"], str) and attrs["machine"]
        for boundary in ("start", "end"):
            assert re.fullmatch(r".*\.\d{1,6}Z", row[boundary]), row
        start, end = (datetime.fromisoformat(row[b].replace("Z", "+00:00")) for b in ("start", "end"))
        assert start <= end and row["duration_ms"] >= 0, row
        times[sid] = (start, end)
        if kind in {"usage", "settlement", "first_output", "connection_open"}:
            eq((start, row["duration_ms"]), (end, 0), kind + " instant")

        matches = [r for r in expected if key(r, fixture_ids) == key(row, by_id)
                   and r["attributes"].get("attempt") == attrs.get("attempt")]
        eq(len(matches), 1, "matching fixture record")
        sample = matches[0]
        fields = {k: v for k, v in sample["attributes"].items()
                  if k not in {"machine", "request_id", "estimated_usd", "charged_usd"}}
        sub(attrs, fields, kind + " attributes")
        parent = row["parent_id"]
        if kind == "gateway_exchange":
            eq(parent, IDENTITY["request_id"], "model call parent")
        elif kind in ("connection_open", "first_output"):
            eq(by_id[parent]["kind"], "provider_attempt", kind + " parent")
            eq(by_id[parent]["parent_id"], exchanges[send]["span_id"], kind + " exchange")
            eq(by_id[parent]["outcome"], exchanges[send]["attributes"]["final_attempt_outcome"], kind + " attempt")
        else:
            eq(parent, exchanges[send]["span_id"], kind + " parent")
        if kind == "settlement":
            eq(attrs["charged_usd"], costs["charged_usd"], "settled wire charge")
        if kind == "usage":
            eq(attrs["estimated_usd"], costs["est_usd"], "measured list price")

    for row in rows:
        parent = row["parent_id"]
        if parent in times:
            start, end = times[row["span_id"]]
            parent_start, parent_end = times[parent]
            assert parent_start <= start <= end <= parent_end, row
    return rows


@case("tracing", gw=None)
def trace_retry_then_success(t):
    """A: a transient failure and backoff precede the only billable attempt."""
    gw = t.gateway("trace-retry-success")
    t.vllm.chat["fake-paid"] = sequence([
        RETRY, {"json": upstreams.chat_completion(text="recovered", usage=USAGE, cost=CHARGE)},
    ])
    response = gw.call(traced("A", "fake-paid")).json
    eq(response["status"], "ok", "recovered")
    compare(gw, "A", response["cost"])
    eq(spent(gw), (CHARGE, CHARGE), "one paid settlement")
    assert response["cost"]["est_usd"] != CHARGE


def no_stream(t, scenario):
    name = "trace-no-stream-" + scenario
    m, url = t.mock(name)
    m.models = ["box-model"]
    d = overlay_dir(t, name, {"box.json": ov([box(url)])})
    gw = t.gateway(name, t.run.config(extra_models=[{**BOX_MODEL, "cost_class": "api"}]),
                   env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    return gw, m


@case("tracing", gw=None)
def trace_before_provider_refusal(t):
    """B: an unsupported stream leaves a refusal and zero attempts or settlement."""
    gw, mock = no_stream(t, "B")
    chunk = gw.call(traced("B", "box-model", stream=True)).sse()[-1]
    eq(chunk["error"]["code"], "gateway.streaming_unsupported", "stream refusal")
    eq(t.run.calls(mock), [], "no provider contact")
    compare(gw, "B")


@case("tracing", gw="cloud", needs=("tls_intercept",))
def trace_slow_anthropic_stream(t):
    """C: the Anthropic attempt stays open through the slow body."""
    frames = upstreams.anthropic_stream(["first", "second"], usage=USAGE)
    frames.insert(3, {"sleep": 0.4})
    t.anthropic.messages = {"sse": frames}
    stop = t.gw.call(traced("C", "claude-spec", stream=True)).sse()[-1]
    eq(stop["kind"], "stop", "Anthropic completion")
    rows = compare(t.gw, "C", stop["cost"])
    attempt = next(r for r in rows if r["kind"] == "provider_attempt")
    opening = next(r for r in rows if r["kind"] == "connection_open")
    first = next(r for r in rows if r["kind"] == "first_output")
    eq(attempt["attributes"]["provider"], "anthropic", "adapter attribution")
    assert attempt["duration_ms"] >= 375
    assert attempt["start"] <= opening["start"] <= first["start"] < attempt["end"]
    assert (datetime.fromisoformat(attempt["end"]) - datetime.fromisoformat(first["start"])).total_seconds() >= 0.375


def partial(t, scenario, tail, timeout_ms):
    gw = t.gateway("trace-partial-" + scenario)
    frames = upstreams.chat_stream(["partial"], terminal=False)
    frames += [upstreams.sse_data(upstreams.chat_chunk(usage=USAGE, choices=False, cost=CHARGE)), tail]
    stream = {"sse": frames}
    t.vllm.chat["fake-paid"] = sequence([RETRY, stream]) if scenario == "G" else stream
    error = gw.call(traced(scenario, "fake-paid", stream=True, budget={"timeout_ms": timeout_ms})).sse()[-1]
    eq(error["kind"], "error", "failed after output")
    compare(gw, scenario, error["cost"])
    eq(spent(gw), (CHARGE, CHARGE), "one partial charge")
    assert error["cost"]["est_usd"] != CHARGE


@case("tracing", gw=None)
def trace_partial_after_retry(t):
    """G: the stream faults after a retry, settling only the paid output."""
    partial(t, "G", {"cut": True}, 5000)


@case("tracing", gw=None)
def trace_partial_at_deadline(t):
    """J: deadline after output retains the same partial charge on disk and on the wire."""
    partial(t, "J", {"sleep": 1}, 300)


@case("tracing")
def trace_free_local_completion(t):
    """M: a free-local completion records usage and zero wallet charge."""
    t.vllm.chat["local-coder"] = {"json": upstreams.chat_completion(text="local", usage=USAGE)}
    reply = t.gw.call(traced("M", "local-coder")).json
    compare(t.gw, "M", reply["cost"])


@case("tracing")
def trace_subscription_completion(t):
    """N: subscription usage has a positive estimate and no settlement."""
    t.vllm.chat["fake-sub"] = {"json": upstreams.chat_completion(text="subscription", usage=(988, 12))}
    reply = t.gw.call(traced("N", "fake-sub")).json
    eq(reply["cost"]["est_usd"], 0.02, "subscription estimate")
    compare(t.gw, "N", reply["cost"])


@case("tracing", gw=None)
def trace_stream_refusal_and_unary_deadline(t):
    """O: two sends share the model call parent and retain distinct send indices."""
    gw, mock = no_stream(t, "O")
    refusal = gw.call(traced("O", "box-model", stream=True)).sse()[-1]
    eq(refusal["error"]["code"], "gateway.streaming_unsupported", "stream refusal")
    mock.chat["box-model"] = {"delay_s": 1, "json": upstreams.chat_completion()}
    fault = gw.call(traced("O", "box-model", 2, budget={"timeout_ms": 200})).json
    eq(fault["error"]["code"], "gateway.transport_failed", "unary deadline")
    rows = compare(gw, "O", fault["cost"], reverse=True)
    eq([r["attributes"]["send_index"] for r in rows if r["kind"] == "gateway_exchange"], [2, 1], "reverse arrival")



@case("tracing", gw=None)
def trace_client_disconnect(t):
    """Closing the downstream stream preserves its attempt and settles paid output once."""
    gw = t.gateway("trace-client-disconnect")
    frames = upstreams.chat_stream(["partial"], terminal=False)
    frames += [upstreams.sse_data(upstreams.chat_chunk(usage=USAGE, choices=False, cost=CHARGE)),
               {"sleep": 2}]
    t.vllm.chat["fake-paid"] = {"sse": frames}
    request = traced("disconnect", "fake-paid", stream=True)
    conn = gw.conn(timeout=5)
    headers = {"Content-Type": "application/json"}
    if gw.token:
        headers["Authorization"] = "Bearer " + gw.token
    try:
        conn.request("POST", "/v1/agent/model.call", json.dumps(request), headers)
        with conn.getresponse() as response:
            assert b"partial" in response.readline()
    finally:
        conn.close()

    path = gw.home / TRACE_DIR / "trace-disconnect.jsonl"
    rows = gw.wait(lambda: [json.loads(line) for line in path.read_text().splitlines()],
                   what="disconnect trace")
    exchange = next(r for r in rows if r["kind"] == "gateway_exchange")
    eq(exchange["outcome"], "client_disconnect", "exchange termination")
    sub(exchange["attributes"], {"client_saw_output": True, "attempts_started": 1,
                                 "final_attempt_outcome": "client_disconnect"}, "disconnected attempt")
    settlements = [r["attributes"] for r in rows if r["kind"] == "settlement"]
    eq(len(settlements), 1, "disconnect settlement count")
    sub(settlements[0], {"partial": True, "charged_usd": spent(gw)[0]}, "persisted partial charge")
    assert settlements[0]["charged_usd"] > 0


@case("tracing")
def unsupported_tool_choice(t):
    """The built-in mock retains its default tool behavior and records the dropped choice."""
    b = t.gw.call(traced("tool-choice", "mock-echo", tool_choice={
        "type": "function", "function": {"name": "fs_write"},
    }, tool_schema=[{"name": "other"}, {"name": "fs_write"}])).json
    eq(b["status"], "ok", "status")
    eq(b["tool_calls"][0]["name"], "other", "default mock tool selection")
    path = t.gw.home / TRACE_DIR / "trace-tool-choice.jsonl"
    exchange, = [row for row in map(json.loads, path.read_text().splitlines())
                 if row["kind"] == "gateway_exchange"]
    eq(exchange["attributes"]["tool_choice_dropped"], True, "exchange records omission")
