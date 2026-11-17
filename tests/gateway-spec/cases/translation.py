"""Provider history over the OpenAI-compatible protocol: bytes sent upstream,
native tool IDs and arguments, error results, malformed-exchange refusal,
the Hermes XML dialect, image refusal, upstream failure classes."""
from harness.gateway import model_call
from harness.run import case, eq, no_egress, sub
from harness import upstreams

TOOLS = [{"name": "fs_read", "description": "read a file", "schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
         {"name": "bare"},
         {"description": "nameless, dropped"}]


@case("translation")
def request_bytes(t):
    """Upstream body: model, max_tokens, hoisted system, roles, tools as functions (schema -> parameters, default object); no auth header keyless."""
    t.gw.call(model_call("local-coder", messages=[{"role": "system", "content": "sys"}, {"role": "user", "content": "q"}], tool_schema=TOOLS))
    req = t.run.calls(t.vllm)[-1]
    eq(req["json"], {"model": "local-coder", "max_tokens": 16,
                     "messages": [{"role": "system", "content": "sys"}, {"role": "user", "content": "q"}],
                     "tools": [{"type": "function", "function": {"name": "fs_read", "description": "read a file",
                                                                 "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}},
                               {"type": "function", "function": {"name": "bare", "parameters": {"type": "object"}}}]}, "upstream body")
    eq(req["path"], "/v1/chat/completions", "path")
    eq(req["headers"].get("content-type"), "application/json", "content-type")
    eq("authorization" in req["headers"], False, "no Authorization for a keyless local endpoint")


@case("translation")
def default_max_tokens(t):
    """max_output_tokens absent or 0 sends max_tokens 4096."""
    t.gw.call(model_call("local-coder", budget={"max_output_tokens": 0}))
    eq(t.run.calls(t.vllm)[-1]["json"]["max_tokens"], 4096, "max_tokens")


@case("translation")
def native_tool_calls(t):
    """Native tool_calls: ids kept, string arguments decoded to JSON, object arguments kept, absent arguments become {}, stop_reason tool_use."""
    t.vllm.chat["local-coder"] = {"json": upstreams.chat_completion(text="", finish="tool_calls", tool_calls=[
        {"id": "call_1", "type": "function", "function": {"name": "fs_read", "arguments": "{\"path\": \"a.txt\"}"}},
        {"id": "call_2", "type": "function", "function": {"name": "bare"}},
        {"id": "call_3", "type": "function", "function": {"name": "obj", "arguments": {"k": 1}}}])}
    b = t.gw.call(model_call("local-coder", tool_schema=TOOLS)).json
    sub(b, {"status": "ok", "message": {"role": "assistant", "content": "", "stop_reason": "tool_use"}}, "response")
    eq(b["tool_calls"], [{"id": "call_1", "name": "fs_read", "args": {"path": "a.txt"}},
                         {"id": "call_2", "name": "bare", "args": {}},
                         {"id": "call_3", "name": "obj", "args": {"k": 1}}], "tool_calls")


@case("translation")
def tool_args_empty_or_unparseable(t):
    """Arguments "" or whitespace mean {}; arguments that are not JSON (alone or beside a valid call) fail the whole call as runtime_fault gateway.provider_failed, $0, no partial tool_calls; never an empty 200 (decided 2026-11-10)."""
    def reply(*calls):
        t.vllm.chat["local-coder"] = {"json": upstreams.chat_completion(text="", finish="tool_calls", tool_calls=[
            {"id": "c%d" % i, "type": "function", "function": {"name": n, "arguments": a}} for i, (n, a) in enumerate(calls, 1)])}
        r = t.gw.call(model_call("local-coder"))
        eq((r.status, len(r.raw) > 0), (200, True), "status and body present")
        return r.json
    for what, args in (("empty string", ""), ("whitespace", "  ")):
        sub(reply(("bare", args)), {"status": "ok", "tool_calls": [{"id": "c1", "name": "bare", "args": {}}]}, what)
    for what, calls in (("not json", [("bad", "{\"path\": ")]), ("valid beside malformed", [("ok", "{}"), ("bad", "not json")])):
        b = reply(*calls)
        sub(b, {"status": "error", "cost": {"currency": "USD", "amount": "0.000000"},
                "error": {"class": "runtime_fault", "code": "gateway.provider_failed", "recoverable": False}}, what)
        eq("tool_calls" in b, False, what + ": no partial result")


@case("translation")
def hermes_xml_tool_calls(t):
    """WIRE clause 2: Qwen/Hermes <function=..><parameter=..> text becomes structured tool_calls (xml-N ids); JSON values parsed."""
    text = "thinking\n<function=fs_read>\n<parameter=path>\nsrc/a.txt\n</parameter>\n<parameter=limit>\n20\n</parameter>\n</function>\n<function=noop></function>"
    t.vllm.chat["local-coder"] = {"json": upstreams.chat_completion(text=text)}
    b = t.gw.call(model_call("local-coder")).json
    eq(b["tool_calls"], [{"id": "xml-1", "name": "fs_read", "args": {"limit": 20, "path": "src/a.txt"}},
                         {"id": "xml-2", "name": "noop", "args": {}}], "tool_calls")
    eq(b["message"], {"role": "assistant", "content": text, "stop_reason": "tool_use"}, "message keeps the text")


@case("translation")
def native_tool_calls_win_over_xml(t):
    """When native tool_calls are present, XML in the text is not parsed."""
    t.vllm.chat["local-coder"] = {"json": upstreams.chat_completion(text="<function=x></function>", finish="tool_calls", tool_calls=[
        {"id": "n1", "type": "function", "function": {"name": "fs_read", "arguments": "{}"}}])}
    eq(t.gw.call(model_call("local-coder")).json["tool_calls"], [{"id": "n1", "name": "fs_read", "args": {}}], "tool_calls")


@case("translation")
def tool_history_forwarded(t):
    """A complete native exchange is forwarded: assistant tool_calls with arguments as the caller's exact JSON text, tool role with tool_call_id; is_error is not sent."""
    msgs = [{"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "name": "fs_read", "args": {"path": "a"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "denied", "is_error": True}]
    eq(t.gw.call(model_call("local-coder", messages=msgs)).json["status"], "ok", "status")
    eq(t.run.calls(t.vllm)[-1]["json"]["messages"], [
        {"role": "user", "content": "go"},
        {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "fs_read", "arguments": "{\"path\": \"a\"}"}}]},
        {"role": "tool", "content": "denied", "tool_call_id": "c1"}], "upstream messages (args forwarded as the caller's raw JSON text)")


@case("translation")
def legacy_tool_observation(t):
    """A tool message without tool_call_id is legacy text: sent as a user turn prefixed [tool observation]."""
    t.gw.call(model_call("local-coder", messages=[{"role": "user", "content": "q"}, {"role": "tool", "content": "out"}]))
    eq(t.run.calls(t.vllm)[-1]["json"]["messages"], [{"role": "user", "content": "q"}, {"role": "user", "content": "[tool observation]\nout"}], "upstream messages")


@case("translation")
def malformed_tool_history_refused(t):
    """A broken native chain is refused before dispatch (never flattened to text): unmatched result, missing result, bad args."""
    bad = {
        "unmatched result": [{"role": "user", "content": "q"}, {"role": "tool", "tool_call_id": "zz", "content": "x"}],
        "missing result": [{"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "name": "f", "args": {}}]}, {"role": "user", "content": "q"}],
        "missing final result": [{"role": "user", "content": "q"}, {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "name": "f", "args": {}}]}],
        "non-object args": [{"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "name": "f", "args": [1]}]}, {"role": "tool", "tool_call_id": "c1", "content": "x"}],
    }
    for what, msgs in bad.items():
        b = t.gw.call(model_call("local-coder", messages=msgs)).json
        sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "runtime_fault", "code": "gateway.transport_failed"}}, what)
        eq(b["error"]["message"].startswith("tool history:"), True, what + " message names tool history")
    eq(t.run.calls(t.vllm), [], "upstream requests")


@case("translation")
def images_refused_openai_compat(t):
    """WIRE clause 3: openai_compat refuses image blocks fail-closed, neutral or native, same class; no upstream request."""
    native = {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}
    neutral = {"type": "image", "media_type": "image/png", "base64": "iVBORw0KGgo="}
    for what, block in (("native", native), ("neutral", neutral), ("text block", {"type": "text", "text": "hi"})):
        b = t.gw.call(model_call("local-coder", messages=[{"role": "user", "content": [block]}])).json
        sub(b, {"status": "error", "cost": {"amount": "0.000000"},
                "error": {"class": "runtime_fault", "code": "gateway.transport_failed", "message": "openai_compat: block content unsupported (role user)"}}, what)
    eq(t.run.calls(t.vllm), [], "upstream requests")


@case("translation")
def invalid_neutral_image_block(t):
    """A neutral block missing media_type or base64: gateway.invalid_image_block, fatal_session, $0, before any provider."""
    for block in ({"type": "image", "media_type": "image/png"}, {"type": "image", "base64": "AAAA"}, {"type": "image", "media_type": "", "base64": "AAAA"}):
        b = t.gw.call(model_call("local-coder", messages=[{"role": "tool", "content": [block]}])).json
        sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "fatal_session", "code": "gateway.invalid_image_block"}}, str(block))
    eq(t.run.calls(t.vllm), [], "upstream requests")


@case("translation")
def finish_reason_passthrough(t):
    """stop_reason: tool_calls -> tool_use; any other non-empty finish_reason verbatim; empty -> end_turn."""
    for finish, want in (("stop", "stop"), ("length", "length"), ("", "end_turn")):
        t.vllm.chat["local-coder"] = {"json": upstreams.chat_completion(finish=finish)}
        eq(t.gw.call(model_call("local-coder")).json["message"]["stop_reason"], want, "finish_reason " + repr(finish))


@case("translation")
def reasoning_fields_not_content(t):
    """reasoning / reasoning_content are not returned as content."""
    t.vllm.chat["local-coder"] = {"json": upstreams.chat_completion(text="answer", extra_message={"reasoning": "secret chain", "reasoning_content": "x"})}
    eq(t.gw.call(model_call("local-coder")).json["message"]["content"], "answer", "content")


@case("translation")
def no_choices(t):
    """An ok reply with no choices is ok, empty content, end_turn."""
    t.vllm.chat["local-coder"] = {"json": {"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 0}}}
    b = t.gw.call(model_call("local-coder")).json
    sub(b, {"status": "ok", "message": {"content": "", "stop_reason": "end_turn"}, "usage": {"input_tokens": 1, "output_tokens": 0}}, "no choices")


@case("translation")
def upstream_status_classes(t):
    """Upstream HTTP status -> error class: 401/403 auth; 400 request rejected; 429/5xx transient; always $0."""
    cases = (
        (401, "fatal_session", "gateway.provider_auth", False),
        (403, "fatal_session", "gateway.provider_auth", False),
        (400, "fatal_session", "gateway.provider_request_rejected", False),
        (429, "retryable_gateway", "gateway.provider_transient", True),
        (500, "retryable_gateway", "gateway.provider_transient", True),
        (503, "retryable_gateway", "gateway.provider_transient", True),
    )
    for status, cls, code, rec in cases:
        t.vllm.chat["local-coder"] = {"json": {"error": {"message": "upstream says %d" % status}}, "status": status}
        budget = {"timeout_ms": 3000} if status in (500, 503) else {}
        b = t.gw.call(model_call("local-coder", budget=budget)).json
        sub(b, {"status": "error", "usage": {"input_tokens": 0, "output_tokens": 0}, "cost": {"amount": "0.000000"},
                "error": {"class": cls, "code": code, "recoverable": rec, "boundary": "gateway"}}, "status %d" % status)
        eq("upstream says %d" % status in b["error"]["message"], True, "status %d message carries the upstream body" % status)
        eq("tool_calls" in b or "message" in b, False, "status %d: never a partial result" % status)
        t.run.reset()
    no_egress(t)


@case("translation")
def upstream_garbage(t):
    """A 200 with an undecodable body is runtime_fault; a dropped connection retries then returns transient, $0."""
    t.vllm.chat["local-coder"] = {"raw": b"<html>oops</html>"}
    b = t.gw.call(model_call("local-coder")).json
    sub(b, {"status": "error", "cost": {"amount": "0.000000"}, "error": {"class": "runtime_fault", "code": "gateway.transport_failed"}}, "not json")
    t.run.reset()
    t.vllm.chat["local-coder"] = {"drop": True}
    b = t.gw.call(model_call("local-coder", budget={"timeout_ms": 15000})).json
    sub(b, {"status": "error", "error": {"class": "retryable_gateway", "code": "gateway.provider_transient"}}, "dropped")
    eq(len(t.run.calls(t.vllm)), 3, "dropped: three upstream attempts")


@case("translation")
def upstream_unreachable(t):
    """A named model whose endpoint refuses connections: transient after retries, no fallback elsewhere."""
    gw = t.gateway("vllm-dead", env={"VLLM_ENDPOINT": "http://127.0.0.1:1"})
    b = gw.call(model_call("vllm-absent", budget={"timeout_ms": 5000})).json
    sub(b, {"status": "error", "error": {"class": "retryable_gateway", "code": "gateway.provider_transient"}}, "unreachable")
    eq(t.run.calls(t.ollama), [], "no fallback to ollama")
    no_egress(t)
