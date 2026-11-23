"""Streaming: SSE framing of refusals and chunk families, the
openai_compat and mock stream paths, the streaming capability refusal, and
the equivalence rule every adapter keeps: a stream's delta text plus its
stop chunk (provider, model, stop_reason, tool_calls, usage, cost) equal
the unary envelope for the same input (anthropic's side is in
cases/anthropic.py)."""
from harness.gateway import V1, V2, model_call
from harness.run import case, eq, norm, sub
from harness import upstreams

CHUNK_V1, CHUNK_V2 = "cs.gateway.model_call.chunk.v1", "agent.gateway.model_call.chunk.v2"
TOOLS = [{"name": "fs.read", "description": "read", "schema": {"type": "object"}}, {"name": "noop"}]


def error_chunk(r, family, code, cls="fatal_session", request_id="spec-req-1"):
    eq(r.status, 200, code + " status")
    eq(r.headers.get("content-type"), "text/event-stream", code + " content-type")
    frames = r.sse()
    eq(len(frames), 1, code + " frame count")
    sub(frames[0], {"schema_version": family, "request_id": request_id, "index": 0, "kind": "error",
                    "cost": {"currency": "USD", "amount": "0.000000"},
                    "error": {"class": cls, "code": code, "boundary": "gateway", "recoverable": False}}, code)
    eq(sorted(frames[0]), ["cost", "error", "index", "kind", "request_id", "schema_version"], code + " keys")
    return frames[0]


def accumulate(frames, what="stream"):
    """Fold a stream into the unary envelope's fields: delta text joined, the rest from the stop chunk."""
    eq([f["index"] for f in frames], list(range(len(frames))), what + " indexes")
    eq([f["kind"] for f in frames], ["delta"] * (len(frames) - 1) + ["stop"], what + " kinds")
    stop = frames[-1]
    out = {"provider": stop.get("provider"), "model": stop.get("model"),
           "message": {"role": "assistant", "content": "".join(f["content"] for f in frames[:-1]), "stop_reason": stop.get("stop_reason")},
           "usage": stop.get("usage"), "cost": stop.get("cost")}
    if "tool_calls" in stop:
        out["tool_calls"] = stop["tool_calls"]
    return out


def unary_view(body):
    """The unary envelope's reply fields (what a stream must reproduce)."""
    b = norm(body)
    eq(b.get("status"), "ok", "unary status")
    return {k: b[k] for k in ("provider", "model", "message", "tool_calls", "usage", "cost") if k in b}


def equivalent(t, gw, hint, unary_spec, stream_frames, mock=None, **kw):
    """The same input served unary and streamed: the folded stream equals the unary reply. Returns the frames."""
    mock = mock or t.vllm
    mock.default_chat = unary_spec
    want = unary_view(gw.call(model_call(hint, **kw)).json)
    mock.default_chat = {"sse": stream_frames} if stream_frames is not None else unary_spec
    r = gw.call(model_call(hint, stream=True, **kw))
    eq(r.headers.get("content-type"), "text/event-stream", "stream content-type")
    frames = r.sse()
    eq(accumulate(frames), want, "stream folded == unary reply")
    return frames


@case("streaming")
def stream_unknown_model(t):
    """stream:true with an unknown model: one SSE error chunk, family matching the request schema."""
    error_chunk(t.gw.call(model_call("nope", stream=True)), CHUNK_V2, "gateway.unknown_model")
    error_chunk(t.gw.call(model_call("nope", stream=True, schema=V1)), CHUNK_V1, "gateway.unknown_model")


@case("streaming")
def stream_empty_messages(t):
    """stream:true with no messages: SSE error chunk gateway.empty_messages."""
    error_chunk(t.gw.call(model_call("local-coder", stream=True, messages=[])), CHUNK_V2, "gateway.empty_messages")


@case("streaming")
def stream_invalid_image_block(t):
    """stream:true with a malformed neutral image: SSE error chunk gateway.invalid_image_block, no upstream."""
    error_chunk(t.gw.call(model_call("local-coder", stream=True, messages=[{"role": "user", "content": [{"type": "image", "base64": "x"}]}])),
                CHUNK_V2, "gateway.invalid_image_block")
    eq(t.run.calls(t.vllm), [], "upstream requests")


@case("streaming")
def stream_schema_mismatch_is_envelope(t):
    """Before a schema is accepted there is no stream: schema_mismatch and malformed JSON answer a JSON envelope (v2 request family), not SSE."""
    r = t.gw.call(model_call("nope", stream=True, schema="bogus"))
    eq(r.headers.get("content-type"), "application/json", "schema mismatch content-type")
    sub(r.json, {"schema_version": V2, "status": "error", "error": {"code": "gateway.schema_mismatch"}}, "schema mismatch")
    r = t.gw.post("/v1/agent/model.call", raw=b'{"stream": true, "schema_version": ')
    eq(r.headers.get("content-type"), "application/json", "malformed content-type")
    sub(r.json, {"schema_version": V2, "status": "error", "error": {"code": "gateway.bad_request"}}, "malformed")


@case("streaming")
def stream_openai_compat_text(t):
    """openai_compat streams: content deltas in order (empty dropped), then one stop chunk with provider, resolved model, finish reason, usage from the usage chunk and cost; stream:true and include_usage sent upstream."""
    t.vllm.default_chat = {"sse": upstreams.chat_stream(["Hel", "", "lo"], usage=(11, 7))}
    got = t.gw.call(model_call("local-coder", stream=True)).sse()
    eq(got, [
        {"schema_version": CHUNK_V2, "request_id": "spec-req-1", "index": 0, "kind": "delta", "content": "Hel"},
        {"schema_version": CHUNK_V2, "request_id": "spec-req-1", "index": 1, "kind": "delta", "content": "lo"},
        {"schema_version": CHUNK_V2, "request_id": "spec-req-1", "index": 2, "kind": "stop", "stop_reason": "stop",
         "provider": "vllm", "model": "local-coder", "usage": {"input_tokens": 11, "output_tokens": 7},
         "cost": {"currency": "USD", "amount": "0.000000", "class": "free_local", "est_usd": 0, "charged_usd": 0}}], "chunks")
    req = t.run.calls(t.vllm)[-1]["json"]
    sub(req, {"model": "local-coder", "max_tokens": 16, "stream": True, "stream_options": {"include_usage": True},
              "messages": [{"role": "user", "content": "hi"}]}, "upstream body")
    got = t.gw.call(model_call("local-coder", stream=True, schema=V1)).sse()
    eq({f["schema_version"] for f in got}, {CHUNK_V1}, "v1 family")


@case("streaming")
def stream_openai_compat_equivalence_text(t):
    """Equivalence, text turn: an api-priced model with a provider-reported charge folds to exactly the unary envelope (content, stop_reason, usage, cost incl. charged)."""
    unary = {"json": upstreams.chat_completion("Hello there", usage=(400, 100), cost=0.5)}
    frames = upstreams.chat_stream(["Hello", " there"], usage=(400, 100), cost=0.5)
    got = equivalent(t, t.gw, "fake-paid", unary, frames)
    sub(got[-1]["cost"], {"amount": "0.005000", "class": "api", "charged": 0.5, "charged_usd": 0.5}, "stop cost")


@case("streaming")
def stream_openai_compat_equivalence_tools(t):
    """Equivalence, tool turn: argument fragments keyed by index assemble into the same tool_calls a unary reply carries (absent arguments mean {}), finish_reason tool_calls becomes stop_reason tool_use."""
    unary = {"json": upstreams.chat_completion("let me look", finish="tool_calls", usage=(9, 4), tool_calls=[
        {"id": "call_1", "type": "function", "function": {"name": "fs.read", "arguments": "{\"path\": \"a.txt\"}"}},
        {"id": "call_2", "type": "function", "function": {"name": "noop"}}])}
    frames = upstreams.chat_stream(["let me ", "look"], finish="tool_calls", usage=(9, 4), tool_deltas=[
        {"index": 0, "id": "call_1", "type": "function", "function": {"name": "fs.read", "arguments": ""}},
        {"index": 0, "function": {"arguments": "{\"pa"}},
        {"index": 1, "id": "call_2", "type": "function", "function": {"name": "noop"}},
        {"index": 0, "function": {"arguments": "th\": \"a.txt\"}"}}])
    got = equivalent(t, t.gw, "local-coder", unary, frames, tool_schema=TOOLS)
    eq(got[-1]["tool_calls"], [{"id": "call_1", "name": "fs.read", "args": {"path": "a.txt"}}, {"id": "call_2", "name": "noop", "args": {}}], "stop tool_calls")
    eq(got[-1]["stop_reason"], "tool_use", "stop_reason")


@case("streaming")
def stream_openai_compat_equivalence_hermes(t):
    """Equivalence, Hermes XML (clause 2): the streamed text carries the XML as the unary content does, and the stop chunk carries the lifted tool calls."""
    xml = "<function=fs_read>\n<parameter=path>\nsrc/a.txt\n</parameter>\n</function>"
    frames = upstreams.chat_stream([xml[:20], xml[20:]])
    got = equivalent(t, t.gw, "local-coder", {"json": upstreams.chat_completion(xml)}, frames, tool_schema=[{"name": "fs_read"}])
    eq((got[-1]["stop_reason"], got[-1]["tool_calls"]), ("tool_use", [{"id": "xml-1", "name": "fs_read", "args": {"path": "src/a.txt"}}]), "lifted")


@case("streaming")
def stream_openai_compat_unary_answer(t):
    """An OpenAI-compatible server that ignores `stream: true` and answers with the unary JSON body still gives a valid stream: the folded deltas and stop equal the unary reply, text and tool turns."""
    frames = equivalent(t, t.gw, "local-coder", {"json": upstreams.chat_completion("whole answer")}, None)
    eq([f["kind"] for f in frames], ["delta", "stop"], "one delta then the stop")
    equivalent(t, t.gw, "local-coder", {"json": upstreams.chat_completion("", finish="tool_calls", tool_calls=[
        {"id": "c1", "type": "function", "function": {"name": "fs.read", "arguments": "{\"path\": \"a\"}"}}])}, None, tool_schema=TOOLS)


@case("streaming")
def stream_openai_compat_bad_args(t):
    """Malformed arguments preserve the tool call and usage in both reply modes."""
    args = '{"path": '
    call = {"id": "c", "type": "function", "function": {"name": "fs.read", "arguments": args}}
    expected = [{"id": "c", "name": "fs.read", "args": {}, "invalid_arguments": True}]
    t.vllm.default_chat = {"json": upstreams.chat_completion("", finish="tool_calls", tool_calls=[call])}
    sub(t.gw.call(model_call("local-coder", tool_schema=TOOLS)).json,
        {"status": "ok", "tool_calls": expected}, "unary")
    t.vllm.default_chat = {"sse": upstreams.chat_stream(["x"], finish="tool_calls", tool_deltas=[
        {"index": 0, "id": "c", "function": {"name": "fs.read", "arguments": args}}])}
    got = t.gw.call(model_call("local-coder", stream=True, tool_schema=TOOLS)).sse()
    eq([f["kind"] for f in got], ["delta", "stop"], "kinds")
    eq(got[-1]["tool_calls"], expected, "flagged args in stop")


@case("streaming")
def stream_openai_compat_eof_without_done(t):
    """WIRE clause 4 for openai_compat: deltas then EOF without data: [DONE] is an error chunk (runtime_fault transport_failed, $0), never a stop, even after a finish_reason."""
    frames = upstreams.chat_stream(["par", "tial"], terminal=False) + [upstreams.sse_data(upstreams.chat_chunk({}, "stop"))]
    t.vllm.default_chat = {"sse": frames}
    got = t.gw.call(model_call("local-coder", stream=True)).sse()
    eq([f["kind"] for f in got], ["delta", "delta", "error"], "kinds")
    sub(got[-1], {"cost": {"amount": "0.000000"}, "error": {"class": "runtime_fault", "code": "gateway.transport_failed",
                                                           "message": "vllm stream ended without [DONE]: unexpected EOF"}}, "error chunk")


@case("streaming")
def stream_openai_compat_error_event(t):
    """An upstream error payload mid-stream: error chunk runtime_fault gateway.upstream_error with its message, $0."""
    t.vllm.default_chat = {"sse": upstreams.chat_stream(["a"], terminal=False) + ['data: {"error": {"message": "model overloaded", "code": 503}}\n\n']}
    got = t.gw.call(model_call("local-coder", stream=True)).sse()
    eq([f["kind"] for f in got], ["delta", "error"], "kinds")
    sub(got[-1], {"error": {"class": "runtime_fault", "code": "gateway.upstream_error", "message": "model overloaded"}, "cost": {"amount": "0.000000"}}, "error chunk")


@case("streaming")
def stream_openai_compat_upstream_status(t):
    """A non-2xx before streaming: one error chunk with the unary status class (503 retryable provider_transient, recoverable false on a chunk)."""
    t.vllm.default_chat = {"json": {"error": "down"}, "status": 503}
    got = t.gw.call(model_call("local-coder", stream=True)).sse()
    eq(len(got), 1, "frames")
    sub(got[0], {"kind": "error", "error": {"class": "retryable_gateway", "code": "gateway.provider_transient", "recoverable": False}}, "error chunk")


@case("streaming")
def stream_mock_text(t):
    """The mock streams for free: its fixed text as word deltas, then a stop chunk equal to the unary reply (provider mock, 20/30 tokens)."""
    r = t.gw.call(model_call("mock-echo", stream=True)).sse()
    eq([f["content"] for f in r[:-1]], ["mock ", "provider ", "response"], "deltas")
    eq(accumulate(r), unary_view(t.gw.call(model_call("mock-echo")).json), "stream folded == unary reply")
    sub(r[-1], {"provider": "mock", "model": "mock-echo", "stop_reason": "end_turn", "usage": {"input_tokens": 20, "output_tokens": 30}}, "stop")


@case("streaming")
def stream_mock_tool_turn(t):
    """The mock's tool turn, unary and streamed alike: with tools declared and no tool result last, it calls the first declared tool (mock-call-1, args {}), stop tool_use, no text; after a tool result it answers with text."""
    stop = accumulate(t.gw.call(model_call("mock-echo", stream=True, tool_schema=TOOLS)).sse())
    eq(stop, unary_view(t.gw.call(model_call("mock-echo", tool_schema=TOOLS)).json), "stream folded == unary reply")
    sub(stop, {"message": {"role": "assistant", "content": "", "stop_reason": "tool_use"},
               "tool_calls": [{"id": "mock-call-1", "name": "fs.read", "args": {}}]}, "tool turn")
    after = [{"role": "user", "content": "go"}, {"role": "assistant", "content": "", "tool_calls": [{"id": "mock-call-1", "name": "fs.read", "args": {}}]},
             {"role": "tool", "tool_call_id": "mock-call-1", "content": "file text"}]
    stop = accumulate(t.gw.call(model_call("mock-echo", stream=True, tool_schema=TOOLS, messages=after)).sse())
    sub(stop, {"message": {"role": "assistant", "content": "mock provider response", "stop_reason": "end_turn"}}, "after the tool result")
    eq("tool_calls" in stop, False, "no tool calls after the result")


@case("streaming")
def stream_empty_hint_streams(t):
    """stream:true with an empty hint resolves free-local first (local-coder) and streams it: a stream never silently becomes unary."""
    t.vllm.default_chat = {"sse": upstreams.chat_stream(["ok"])}
    got = t.gw.call(model_call("", stream=True)).sse()
    sub(got[-1], {"kind": "stop", "provider": "vllm", "model": "local-coder"}, "stop")
    eq(t.run.calls(t.vllm)[-1]["json"].get("stream"), True, "streamed upstream")


@case("streaming", gw=None)
def stream_unsupported_refused(t):
    """An endpoint whose declaration cannot stream refuses stream:true with its own code: one SSE error chunk fatal_session gateway.streaming_unsupported, $0, before any connection; /v1/models says streaming false up front; unary serves."""
    from cases.catalog import BOX_MODEL, box, ov, overlay_dir
    m, url = t.mock("nostream")
    m.models = ["box-model"]
    d = overlay_dir(t, "nostream-decl", {"box.json": ov([box(url)])})
    gw = t.gateway("stream-decl", t.run.config(extra_models=[BOX_MODEL]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    rows = {r["name"]: r for r in gw.get("/v1/models").json["models"]}
    eq((rows["box-model"]["streaming"], rows["local-coder"]["streaming"]), (False, True), "/v1/models streaming")
    caps = {r["name"]: r["streaming"] for r in gw.get("/v1/capabilities").json["api_models"]}
    eq((caps["box-model"], caps["local-coder"]), (False, True), "/v1/capabilities streaming")
    f = error_chunk(gw.call(model_call("box-model", stream=True)), CHUNK_V2, "gateway.streaming_unsupported")
    eq(f["error"]["message"], "spec-box: streaming unsupported (protocol or endpoint declaration); call unary", "message")
    eq(t.run.calls(m), [], "no upstream request")
    sub(gw.call(model_call("box-model")).json, {"status": "ok", "model": "box-model"}, "unary serves")


@case("streaming")
def wire_documents_json_before_schema_acceptance(t):
    """The wire guide agrees with stream_schema_mismatch_is_envelope."""
    from pathlib import Path

    wire = (Path(__file__).resolve().parents[3] / "gateway" / "WIRE.md").read_text()
    assert "Malformed JSON and schema mismatches return a JSON error envelope" in wire
