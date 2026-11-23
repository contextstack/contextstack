"""Gateway upstream classification and bounded retries (audit items 7–8)."""
import time

from harness.gateway import model_call
from harness.run import case, eq, sub
from harness import upstreams


def sequence(specs):
    """Return a chat responder that yields specs in order, then repeats the last."""
    state = {"i": 0}

    def respond(h, body):
        i = min(state["i"], len(specs) - 1)
        state["i"] += 1
        spec = specs[i]
        return spec(h, body) if callable(spec) else spec

    return respond


@case("translation")
def permanent_request_errors_no_retry(t):
    """400 and 413 refuse without gateway retries."""
    for status, code in ((400, "gateway.provider_request_rejected"), (413, "gateway.provider_request_rejected")):
        t.vllm.chat["local-coder"] = {"json": {"error": {"message": "bad"}}, "status": status}
        b = t.gw.call(model_call("local-coder")).json
        sub(b, {"status": "error", "error": {"class": "fatal_session", "code": code, "recoverable": False}}, "status %d" % status)
        eq(len(t.run.calls(t.vllm)), 1, "status %d: one upstream call" % status)
        t.run.reset()


@case("translation")
def quota_body_no_retry(t):
    """Quota-exhausted bodies refuse as gateway.provider_quota_exhausted with no retry."""
    t.vllm.chat["local-coder"] = {
        "json": {"error": {"type": "insufficient_quota", "message": "billing"}},
        "status": 429,
    }
    b = t.gw.call(model_call("local-coder")).json
    sub(b, {"status": "error", "error": {"class": "fatal_session", "code": "gateway.provider_quota_exhausted", "recoverable": False}}, "quota")
    eq(len(t.run.calls(t.vllm)), 1, "one upstream call")


@case("translation")
def retry_after_then_ok(t):
    """429 with Retry-After then 200: one retry after the hint, success."""
    t.vllm.chat["local-coder"] = sequence([
        {"status": 429, "headers": {"Retry-After": "1"}, "json": {"error": {"message": "slow down"}}},
        {"json": upstreams.chat_completion(text="recovered")},
    ])
    start = time.monotonic()
    b = t.gw.call(model_call("local-coder", budget={"timeout_ms": 15000})).json
    elapsed = time.monotonic() - start
    sub(b, {"status": "ok", "message": {"content": "recovered"}}, "ok after retry")
    eq(len(t.run.calls(t.vllm)), 2, "two upstream calls")
    eq(elapsed >= 0.9, True, "honoured Retry-After")


@case("translation")
def two_retries_then_ok(t):
    """503 twice then 200 succeeds after two gateway retries."""
    fail = {"status": 503, "json": {"error": {"message": "down"}}}
    t.vllm.chat["local-coder"] = sequence([fail, fail, {"json": upstreams.chat_completion(text="third")}])
    b = t.gw.call(model_call("local-coder")).json
    sub(b, {"status": "ok", "message": {"content": "third"}}, "ok")
    eq(len(t.run.calls(t.vllm)), 3, "three upstream calls")


@case("translation")
def three_transient_failures_carry_retry_after(t):
    """503 three times returns an error with retry_after_ms populated."""
    spec = {"status": 503, "headers": {"Retry-After": "1"}, "json": {"error": {"message": "down"}}}
    t.vllm.chat["local-coder"] = sequence([spec, spec, spec])
    b = t.gw.call(model_call("local-coder", budget={"timeout_ms": 20000})).json
    sub(b, {"status": "error", "error": {"class": "retryable_gateway", "code": "gateway.provider_transient", "retry_after_ms": 1000}}, "error")
    eq(len(t.run.calls(t.vllm)), 3, "three upstream calls")


@case("translation")
def transient_retries_stop_at_deadline(t):
    """A long Retry-After cannot defer retries past budget.timeout_ms; only one upstream attempt is made."""
    spec = {"status": 503, "headers": {"Retry-After": "30"}, "json": {"error": {"message": "down"}}}
    t.vllm.chat["local-coder"] = sequence([spec, spec, spec])
    b = t.gw.call(model_call("local-coder", budget={"timeout_ms": 400})).json
    sub(b, {"status": "error", "error": {"class": "retryable_gateway", "code": "gateway.provider_transient"}}, "error")
    eq(len(t.run.calls(t.vllm)), 1, "deadline blocks a second upstream call")


@case("streaming")
def stream_partial_output_no_retry(t):
    """After streamed output has started, a disconnect is not retried."""
    frames = upstreams.chat_stream(["partial"], terminal=False) + [{"cut": True}]
    t.vllm.default_chat = {"sse": frames}
    chunks = t.gw.call(model_call("local-coder", stream=True)).sse()
    eq(chunks[0]["content"], "partial", "output before disconnect")
    eq(chunks[-1]["error"]["code"], "gateway.transport_failed", "terminal failure")
    eq(len(t.run.calls(t.vllm)), 1, "one upstream stream open")


@case("translation")
def unary_body_abort_no_retry(t):
    """A 200 response whose body aborts mid-read is not retried."""
    t.vllm.chat["local-coder"] = {"abort_body": True}
    b = t.gw.call(model_call("local-coder", budget={"timeout_ms": 15000})).json
    sub(b, {"status": "error", "error": {"class": "runtime_fault", "code": "gateway.transport_failed"}}, "body abort")
    eq(len(t.run.calls(t.vllm)), 1, "one upstream call")


# Accounting belongs to the logical request, outside both retry loops. Non-2xx
# error bodies are diagnostic, even if they contain usage-shaped fields.
def usage_rows(gw, request_id):
    return [line.split("USAGE ", 1)[1] for line in gw.log_text().splitlines()
            if "USAGE " in line and ("req=" + request_id + " ") in line]


@case("accounting", gw=None)
def retries_bill_success_once(t):
    """Unary and stream-open retries bill only the successful attempt, in logs and persisted spend."""
    from cases.spend import limits, spent

    for stream in (False, True):
        for failure in (408, 409, 425, 429, 500, 502, 503, 504, 529):
            name = "retry-bill-%s-%s" % (stream, failure)
            gw = t.gateway(name, limits=limits(1))
            # Deliberately different failed-attempt numbers catch accidental merging.
            fail = {
                "status": failure, "headers": {"Retry-After": "0"},
                "json": upstreams.chat_completion(usage=(9000, 8000), cost=0.9),
            }
            success = ({"sse": upstreams.chat_stream(["recovered"], usage=(37, 19), cost=0.0042)}
                       if stream else {"json": upstreams.chat_completion(usage=(37, 19), cost=0.0042)})
            t.vllm.chat["fake-paid"] = sequence([fail, success])
            before = len(t.run.calls(t.vllm))
            response = gw.call(model_call("fake-paid", stream=stream, request_id=name))
            b = response.sse()[-1] if stream else response.json
            if stream:
                eq(b["kind"], "stop", name + " terminal")
            else:
                eq(b["status"], "ok", name + " status")
            eq(b["usage"], {"input_tokens": 37, "output_tokens": 19}, name + " usage")
            sub(b["cost"], {"amount": "0.000560", "charged_usd": 0.0042}, name + " cost")
            eq(len(t.run.calls(t.vllm)) - before, 2, name + " attempts")
            eq(spent(gw), (0.0042, 0.0042), name + " spend")
            rows = usage_rows(gw, name)
            eq(len(rows), 1, name + " one usage record")
            eq(rows[0].startswith("provider=vllm model=fake-paid in=37 out=19 cost=0.000560 "), True, name + " record")
            eq("partial=1" in rows[0], False, name + " complete")


@case("accounting", gw=None)
def timeout_settles_reservation_without_attempt_usage(t):
    """The shared request deadline is a local transport fault, not a retryable per-attempt timeout."""
    from cases.spend import limits, spent

    for stream in (False, True):
        name = "timeout-bill-%s" % stream
        gw = t.gateway(name, limits=limits(1))
        t.vllm.chat["fake-paid"] = sequence([
            {"delay_s": 1, "json": upstreams.chat_completion(usage=(9000, 8000))},
            {"json": upstreams.chat_completion(usage=(37, 19))},
        ])
        before = len(t.run.calls(t.vllm))
        response = gw.call(model_call("fake-paid", stream=stream, request_id=name, budget={"timeout_ms": 200}))
        b = response.sse()[-1] if stream else response.json
        eq(b["error"]["code"], "gateway.transport_failed", name + " timeout")
        eq(len(t.run.calls(t.vllm)) - before, 1, name + " deadline prevents retry")
        eq(usage_rows(gw, name), [], name + " no usage")
        charge = b["cost"]["charged_usd"]
        assert charge > 0, name
        eq(b["cost"]["est_usd"], charge, name + " reservation estimate")
        eq(spent(gw), (round(charge, 6),) * 2, name + " persisted settlement")


@case("accounting", gw=None)
def partial_attempt_is_labeled_and_never_retried(t):
    """A billable partial stream is one labeled attempt; never merge it with an automatic retry."""
    from cases.spend import limits, spent

    gw = t.gateway("retry-bill-partial", limits=limits(1))
    frames = upstreams.chat_stream(["partial"], terminal=False)
    frames += [upstreams.sse_data(upstreams.chat_chunk(usage=(37, 19), choices=False, cost=0.0042)), {"cut": True}]
    t.vllm.chat["fake-paid"] = sequence([
        {"sse": frames}, {"sse": upstreams.chat_stream(["must not retry"], usage=(9000, 8000))},
    ])
    b = gw.call(model_call("fake-paid", stream=True, request_id="partial-bill")).sse()[-1]
    eq(b["error"]["code"], "gateway.transport_failed", "partial failure")
    eq(len(t.run.calls(t.vllm)), 1, "no retry after 2xx stream open")
    eq(spent(gw), (0.0042, 0.0042), "reported partial charge settled once")
    rows = usage_rows(gw, "partial-bill")
    eq(len(rows), 1, "one partial usage record including drop cleanup")
    eq(rows[0].startswith("provider=vllm model=fake-paid in=37 out=19 cost=0.000560 "), True, "metered partial tokens")
    eq(rows[0].endswith(" partial=1"), True, "explicit partial label")


@case("translation")
def close_before_headers_retries(t):
    """Free local unary and streaming POSTs recover on the second attempt."""
    for stream in (False, True):
        name = "close-before-headers-%s" % stream
        success = ({"sse": upstreams.chat_stream(["recovered"], usage=(37, 19))}
                   if stream else {"json": upstreams.chat_completion(text="recovered", usage=(37, 19))})
        t.vllm.chat["local-coder"] = sequence([{"drop": True}, success])
        before = len(t.run.calls(t.vllm))
        reply = t.gw.call(model_call("local-coder", stream=stream, request_id=name))
        if stream:
            chunks = reply.sse()
            eq(chunks[0]["content"], "recovered", "stream output")
            eq(chunks[-1]["kind"], "stop", "stream terminal")
        else:
            sub(reply.json, {"status": "ok", "message": {"content": "recovered"}}, "unary reply")
        eq(len(t.run.calls(t.vllm)) - before, 2, "upstream calls")
        rows = [row for row in t.gw.log_rows("provider_http_trace") if row.get("request_id") == name]
        eq(len(rows), 2, "two HTTP attempt traces")
        sub(rows[0], {"attempt": 1, "stream": stream, "failed": True, "connection_lost": True}, "failed attempt")
        eq("http_status" in rows[0], False, "no response headers")
        sub(rows[1], {"attempt": 2, "stream": stream, "failed": False, "http_status": 200}, "successful attempt")


@case("translation")
def repeated_close_before_headers_exhausts_retries(t):
    """A free local POST is retried at most twice."""
    t.vllm.chat["local-coder"] = {"drop": True}
    reply = t.gw.call(model_call("local-coder")).json
    eq(reply["error"]["code"], "gateway.provider_transient", "lost response")
    eq(len(t.run.calls(t.vllm)), 3, "bounded retry")


@case("accounting", gw=None)
def reset_after_post_settles_without_retry(t):
    """A connection lost after receiving the POST is a billable attempt."""
    from cases.spend import limits, spent

    for stream in (False, True):
        gw = t.gateway("reset-paid-%s" % stream, limits=limits(1))
        t.vllm.chat["fake-paid"] = sequence([
            {"drop": True}, {"json": upstreams.chat_completion(text="unexpected retry")},
        ])
        before = len(t.run.calls(t.vllm))
        reply = gw.call(model_call("fake-paid", stream=stream))
        body = reply.sse()[-1] if stream else reply.json
        eq(body["error"]["code"], "gateway.transport_failed", "lost response")
        eq(len(t.run.calls(t.vllm)) - before, 1, "POST is never replayed")
        charge = body["cost"]["charged_usd"]
        assert charge > 0
        eq(spent(gw), (round(charge, 6),) * 2, "persisted settlement")
