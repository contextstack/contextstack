"""The Anthropic Messages protocol: request bytes, tool-name encoding, image
translation, tool results, and the stream lifecycle (deltas, terminal event,
partial then failure, EOF without stop, upstream error event, disconnect,
deadline, streamed tool_use and its equivalence with the unary reply). Served by a mock behind the
egress proxy's TLS intercept with a fake key from a temp credential store."""
import time

from harness.gateway import V1, model_call
from harness.run import case, eq, no_egress, norm, sub
from harness import upstreams
from cases.streaming import CHUNK_V1, CHUNK_V2, accumulate, unary_view

NEEDS = ("tls_intercept",)


def messages_calls(t):
    return [r for r in t.anthropic.requests.items if r["path"] == "/v1/messages"]


@case("anthropic", gw=None, needs=NEEDS)
def anthropic_tls_ca_file(t):
    """The local TLS intercept is trusted only when its generated CA is configured."""
    store = t.run.credential_store("tls-ca", anthropic="sk-ant-spec-FAKE")
    env = {"CSGATEWAY_CREDENTIALS": str(store)}
    empty_bundle = t.run.tmp / "empty-ca.pem"
    empty_bundle.touch()
    without_ca = t.gateway("without-ca", env=dict(env, SSL_CERT_FILE=str(empty_bundle)))
    b = without_ca.call(model_call("claude-spec")).json
    eq(b["status"], "error", "untrusted CA")
    eq(messages_calls(t), [], "untrusted server received no HTTP request")

    with_ca = t.gateway("with-ca", env=dict(env, SSL_CERT_FILE=str(t.run.tls.ca)))
    b = with_ca.call(model_call("claude-spec")).json
    eq(b["status"], "ok", "trusted CA")
    eq(len(messages_calls(t)), 1, "trusted server received the HTTP request")
    no_egress(t)


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_request_bytes(t):
    """Headers (x-api-key from the store, anthropic-version) and body: model, max_tokens, top-level system, tools with encoded names and input_schema."""
    tools = [{"name": "fs.read", "description": "d", "schema": {"type": "object"}}, {"name": "fs_read"}, {"name": "okf-x"}]
    b = t.gw.call(model_call("claude-spec", messages=[{"role": "system", "content": "sys"}, {"role": "user", "content": "q"}], tool_schema=tools)).json
    eq(b["status"], "ok", "status")
    req = messages_calls(t)[-1]
    sub(req["headers"], {"x-api-key": "sk-ant-spec-FAKE", "anthropic-version": "2023-06-01", "content-type": "application/json"}, "headers")
    eq(req["json"], {"model": "claude-spec", "max_tokens": 16, "system": "sys", "messages": [{"role": "user", "content": "q"}],
                     "tools": [{"name": "fs_dread", "description": "d", "input_schema": {"type": "object"}},
                               {"name": "fs_uread", "input_schema": {"type": "object"}},
                               {"name": "okf-x", "input_schema": {"type": "object"}}]}, "body")
    eq([s["host"] for s in t.proxy.seen.items if s["host"] == "api.anthropic.com"] != [], True, "reached via the intercepted host")
    no_egress(t)


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_tool_choice(t):
    """Named selection uses the same name encoding as declarations. Required becomes any."""
    tools = [{"name": "fs_write"}, {"name": "fs_edit"}]
    named = {"type": "function", "function": {"name": "fs_write"}}
    for choice, expected in ((named, {"type": "tool", "name": "fs_uwrite"}),
                             ("required", {"type": "any"})):
        b = t.gw.call(model_call("claude-spec", tool_schema=tools, tool_choice=choice)).json
        eq(b["status"], "ok", "status")
        eq(messages_calls(t)[-1]["json"]["tool_choice"], expected, "upstream tool_choice")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_unary_response(t):
    """Text blocks concatenate; tool_use blocks become tool_calls with names restored; stop_reason verbatim; api cost from tokens."""
    t.anthropic.messages = {"json": upstreams.anthropic_message([
        {"type": "text", "text": "a"}, {"type": "text", "text": "b"},
        {"type": "tool_use", "id": "toolu_1", "name": "fs_dread", "input": {"path": "x"}},
        {"type": "tool_use", "id": "toolu_2", "name": "undeclared_dname", "input": {}}], stop="tool_use", usage=(100, 50))}
    b = t.gw.call(model_call("claude-spec", tool_schema=[{"name": "fs.read"}], budget={"max_output_tokens": 64})).json
    b = norm(b)
    eq(b, {"schema_version": "agent.gateway.model_call.v2", "request_id": "spec-req-1", "status": "ok", "provider": "anthropic", "model": "claude-spec",
           "message": {"role": "assistant", "content": "ab", "stop_reason": "tool_use"},
           "tool_calls": [{"id": "toolu_1", "name": "fs.read", "args": {"path": "x"}}, {"id": "toolu_2", "name": "undeclared_dname", "args": {}}],
           "usage": {"input_tokens": 100, "output_tokens": 50},
           "cost": {"currency": "USD", "amount": "0.002250", "class": "api", "est_usd": 0.00225, "charged_usd": 0.00225, "over_reservation": True}}, "response")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_tool_name_refusals(t):
    """Tool names with a rune outside [A-Za-z0-9._-] refuse before dispatch (the '.'/'_' encoding is injective, so no valid pair collides)."""
    for what, tools in (("slash", [{"name": "fs/read"}]), ("space", [{"name": "ok"}, {"name": "a b"}])):
        b = t.gw.call(model_call("claude-spec", tool_schema=tools)).json
        sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "runtime_fault", "code": "gateway.transport_failed"}}, what)
    eq(messages_calls(t), [], "upstream requests")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_images(t):
    """Native image blocks pass verbatim; neutral blocks become the source shape, byte-equivalent (WIRE Images)."""
    png = "iVBORw0KGgoAAAANSUhEUg=="
    native = {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png}}
    neutral = {"type": "image", "media_type": "image/png", "base64": png}
    text = {"type": "text", "text": "see"}
    t.gw.call(model_call("claude-spec", messages=[{"role": "user", "content": [text, native]}]))
    t.gw.call(model_call("claude-spec", messages=[{"role": "user", "content": [text, neutral]}]))
    a, b = messages_calls(t)[-2:]
    eq(a["json"]["messages"], [{"role": "user", "content": [text, native]}], "native verbatim")
    eq(b["json"]["messages"], a["json"]["messages"], "neutral translated to the native shape")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_tool_history(t):
    """Assistant tool_calls become tool_use blocks (text first); tool results become user tool_result blocks with is_error; image in a tool result translated."""
    neutral = {"type": "image", "media_type": "image/png", "base64": "AAAA"}
    msgs = [{"role": "user", "content": "go"},
            {"role": "assistant", "content": "calling", "tool_calls": [{"id": "t1", "name": "fs.read", "args": {"p": 1}}, {"id": "t2", "name": "snap", "args": {}}]},
            {"role": "tool", "tool_call_id": "t1", "content": "boom", "is_error": True},
            {"role": "tool", "tool_call_id": "t2", "content": [{"type": "text", "text": "shot"}, neutral]},
            {"role": "tool", "content": "legacy"}]
    eq(t.gw.call(model_call("claude-spec", messages=msgs)).json["status"], "ok", "status")
    eq(messages_calls(t)[-1]["json"]["messages"], [
        {"role": "user", "content": "go"},
        {"role": "assistant", "content": [{"type": "text", "text": "calling"},
                                          {"type": "tool_use", "id": "t1", "name": "fs_dread", "input": {"p": 1}},
                                          {"type": "tool_use", "id": "t2", "name": "snap", "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "boom", "is_error": True}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "is_error": False, "content": [
            {"type": "text", "text": "shot"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}]}]},
        {"role": "user", "content": "[tool observation]\nlegacy"}], "upstream messages")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_status_classes(t):
    """401 provider_auth; 429/529 provider_transient; $0; message carries the upstream body."""
    for status, code in ((401, "gateway.provider_auth"), (429, "gateway.provider_transient"), (529, "gateway.provider_transient")):
        t.anthropic.messages = {"json": {"type": "error", "error": {"type": "x", "message": "said %d" % status}}, "status": status}
        b = t.gw.call(model_call("claude-spec")).json
        sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"code": code}}, str(status))
        eq("said %d" % status in b["error"]["message"], True, "%d message" % status)


def stream(t, frames, schema=None, **kw):
    t.anthropic.messages = {"sse": frames}
    body = model_call("claude-spec", stream=True, **kw) if schema is None else model_call("claude-spec", stream=True, schema=schema, **kw)
    r = t.gw.call(body)
    eq(r.status, 200, "status")
    eq(r.headers.get("content-type"), "text/event-stream", "content-type")
    return r.sse()


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_normal(t):
    """Deltas in order (empty deltas dropped), then one stop chunk with provider, resolved model, usage from message_start/message_delta and cost; stream:true sent upstream."""
    frames = upstreams.anthropic_stream(["Hel", "", "lo"], usage=(11, 7))
    got = stream(t, frames)
    eq(got, [
        {"schema_version": CHUNK_V2, "request_id": "spec-req-1", "index": 0, "kind": "delta", "content": "Hel"},
        {"schema_version": CHUNK_V2, "request_id": "spec-req-1", "index": 1, "kind": "delta", "content": "lo"},
        {"schema_version": CHUNK_V2, "request_id": "spec-req-1", "index": 2, "kind": "stop", "stop_reason": "end_turn",
         "provider": "anthropic", "model": "claude-spec", "usage": {"input_tokens": 11, "output_tokens": 7},
         "cost": {"currency": "USD", "amount": "0.000270", "class": "api", "est_usd": 0.00027, "charged_usd": 0.00027}}], "chunks")
    req = messages_calls(t)[-1]
    eq((req["json"]["stream"], req["headers"].get("accept")), (True, "text/event-stream"), "stream request")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_v1_family(t):
    """An accepted v1 request streams cs.gateway.model_call.chunk.v1 frames."""
    got = stream(t, upstreams.anthropic_stream(["x"]), schema=V1)
    eq([f["schema_version"] for f in got], [CHUNK_V1, CHUNK_V1], "families")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_stop_reason(t):
    """message_delta's stop_reason reaches the stop chunk."""
    got = stream(t, upstreams.anthropic_stream(["x"], stop="max_tokens"))
    eq(got[-1]["stop_reason"], "max_tokens", "stop_reason")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_eof_without_stop(t):
    """WIRE clause 4: deltas then EOF without message_stop ends in an error chunk carrying the settled partial charge, never ok."""
    got = stream(t, upstreams.anthropic_stream(["par", "tial"], terminal=False))
    eq([f["kind"] for f in got], ["delta", "delta", "error"], "kinds")
    sub(got[-1], {"index": 0, "cost": {"currency": "USD", "amount": "0.000180", "class": "api",
                                      "est_usd": 0.00018},
                  "error": {"class": "runtime_fault", "code": "gateway.transport_failed",
                            "message": "anthropic stream ended without message_stop: unexpected EOF"}}, "terminal error (index restarts at 0)")
    eq(round(got[-1]["cost"]["charged_usd"], 6), 0.00018, "settled partial charge")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_error_event(t):
    """An upstream error event after partial deltas: error chunk runtime_fault gateway.upstream_error with the upstream message."""
    frames = upstreams.anthropic_stream(["a"], terminal=False) + [upstreams.sse("error", {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})]
    got = stream(t, frames)
    eq([f["kind"] for f in got], ["delta", "error"], "kinds")
    sub(got[-1], {"error": {"class": "runtime_fault", "code": "gateway.upstream_error", "message": "Overloaded"},
                  "cost": {"currency": "USD", "amount": "0.000180", "class": "api",
                           "est_usd": 0.00018}}, "error chunk")
    eq(round(got[-1]["cost"]["charged_usd"], 6), 0.00018, "settled partial charge")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_upstream_status(t):
    """A non-2xx before output: one $0 error chunk with the status class (529 retryable provider_transient)."""
    t.anthropic.messages = {"json": {"error": {"message": "overloaded"}}, "status": 529}
    got = t.gw.call(model_call("claude-spec", stream=True)).sse()
    eq(len(got), 1, "frames")
    sub(got[0], {"kind": "error", "cost": {"currency": "USD", "amount": "0.000000"},
                 "error": {"class": "retryable_gateway", "code": "gateway.provider_transient"}}, "error chunk")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_cut(t):
    """Transport cut mid-stream (connection closed after a delta): error chunk transport_failed, never a stop."""
    got = stream(t, upstreams.anthropic_stream(["a"], terminal=False) + [{"cut": True}])
    eq([f["kind"] for f in got], ["delta", "error"], "kinds")
    eq(got[-1]["error"]["code"], "gateway.transport_failed", "code")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_deadline(t):
    """budget.timeout_ms expires mid-stream: the stream ends in an error chunk, within the deadline plus slack."""
    frames = upstreams.anthropic_stream(["a"], terminal=False) + [{"sleep": 3}] + upstreams.anthropic_stream(["late"])[2:]
    started = time.monotonic()
    got = stream(t, frames, budget={"timeout_ms": 400})
    took = time.monotonic() - started
    eq([f["kind"] for f in got], ["delta", "error"], "kinds")
    sub(got[-1], {"error": {"class": "runtime_fault", "code": "gateway.transport_failed"}}, "error chunk")
    eq(took < 2.5, True, "returned near the deadline (took %.2fs)" % took)


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_client_disconnect(t):
    """The caller disconnecting mid-stream closes the upstream connection (the gateway stops reading), and a later call succeeds."""
    frames = upstreams.anthropic_stream(["a"], terminal=False) + sum(([{"sleep": 0.1}, upstreams.sse("content_block_delta", {
        "type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "x"}})] for _ in range(60)), [])
    t.anthropic.messages = {"sse": frames}
    t.gw.abandon("/v1/agent/model.call", model_call("claude-spec", stream=True), after_s=0.5)
    t.anthropic.events.wait(lambda ev: any(e.get("disconnected") for e in ev), timeout=8)
    t.anthropic.messages = {"json": upstreams.anthropic_message([{"type": "text", "text": "after"}])}
    eq(t.gw.call(model_call("claude-spec")).json["message"]["content"], "after", "next call")


def tool_frames(blocks, stop="tool_use", text="let me look", usage=(11, 9)):
    """A text block, then one tool_use block per (id, encoded name, [partial_json fragments]), then the stop."""
    frames = upstreams.anthropic_stream([text], usage=usage, terminal=False) + [
        upstreams.sse("content_block_stop", {"type": "content_block_stop", "index": 0})]
    for i, (tid, name, parts) in enumerate(blocks, start=1):
        frames.append(upstreams.sse("content_block_start", {"type": "content_block_start", "index": i,
                                                            "content_block": {"type": "tool_use", "id": tid, "name": name, "input": {}}}))
        frames += [upstreams.sse("content_block_delta", {"type": "content_block_delta", "index": i,
                                                         "delta": {"type": "input_json_delta", "partial_json": p}}) for p in parts]
        frames.append(upstreams.sse("content_block_stop", {"type": "content_block_stop", "index": i}))
    return frames + [upstreams.sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": usage[1]}}),
                     upstreams.sse("message_stop", {"type": "message_stop"})]


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_tool_use(t):
    """Streamed tool_use blocks reach the stop chunk as tool_calls: input_json_delta fragments joined and parsed, names restored, block order kept, an input with no fragments is {}. (Was anthropic_stream_tool_use_dropped, which pinned the bug of dropping them.)"""
    got = stream(t, tool_frames([("toolu_9", "fs_dread", ['{"pa', 'th": "a"}']), ("toolu_10", "noop", [])]),
                 tool_schema=[{"name": "fs.read"}, {"name": "noop"}])
    eq([(f["kind"], f.get("content"), f.get("stop_reason")) for f in got], [("delta", "let me look", None), ("stop", None, "tool_use")], "chunks")
    eq(got[-1]["tool_calls"], [{"id": "toolu_9", "name": "fs.read", "args": {"path": "a"}}, {"id": "toolu_10", "name": "noop", "args": {}}], "stop tool_calls")
    eq(sorted(got[-1]), ["cost", "index", "kind", "model", "provider", "request_id", "schema_version", "stop_reason", "tool_calls", "usage"], "stop chunk keys")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_equivalence(t):
    """Equivalence: for the same reply, text turn and tool turn, the folded stream equals the unary envelope (provider, model, content, stop_reason, tool_calls, usage, cost)."""
    tools = [{"name": "fs.read"}]
    for what, content, frames, stop in (
            ("text", [{"type": "text", "text": "Hello"}], upstreams.anthropic_stream(["Hel", "lo"], usage=(11, 7)), "end_turn"),
            ("tool", [{"type": "text", "text": "let me look"}, {"type": "tool_use", "id": "toolu_1", "name": "fs_dread", "input": {"path": "x", "n": [1, 2]}}],
             tool_frames([("toolu_1", "fs_dread", ['{"path": "x", ', '"n": [1, 2]}'])], usage=(11, 7)), "tool_use")):
        t.anthropic.messages = {"json": upstreams.anthropic_message(content, stop=stop, usage=(11, 7))}
        want = unary_view(t.gw.call(model_call("claude-spec", tool_schema=tools)).json)
        eq(accumulate(stream(t, frames, tool_schema=tools), what), want, what + ": stream folded == unary reply")


@case("anthropic", gw="cloud", needs=NEEDS)
def anthropic_stream_tool_args_not_json(t):
    """Streamed tool input that joins to non-JSON fails after output with a charged runtime_fault gateway.provider_failed and no partial tool calls."""
    got = stream(t, tool_frames([("toolu_1", "fs_dread", ['{"path": '])]), tool_schema=[{"name": "fs.read"}])
    eq([f["kind"] for f in got], ["delta", "error"], "kinds")
    sub(got[-1], {"index": 0, "cost": {"currency": "USD", "amount": "0.000300", "class": "api",
                                      "est_usd": 0.0003},
                  "error": {"class": "runtime_fault", "code": "gateway.provider_failed", "recoverable": False}}, "error chunk")
    eq(round(got[-1]["cost"]["charged_usd"], 6), 0.0003, "settled partial charge")
