#!/usr/bin/env python3
"""Rust-only runner regressions. Run with the path to a built cs binary."""

import argparse
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

LAB = Path(__file__).resolve().parents[2] / "cs"
REGRESSIONS = LAB / "tasks/regressions"
V2 = REGRESSIONS / "runner-characterisation-v2"
sys.path.insert(0, str(REGRESSIONS / "runner-characterisation"))
import _harness


@contextmanager
def gateway(responses, models=None):
    """A scripted gateway. A `("sse", frames)` reply streams; `models` answers the runner's probe."""
    requests = []
    finished = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            if models is None:
                self.send_error(501, "no probe")
                return
            body = json.dumps({"models": models}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            requests.append(self.rfile.read(int(self.headers["Content-Length"])))
            index = len(requests) - 1
            if index >= len(responses):
                self.send_error(500, "unexpected gateway request")
                return
            body = responses[index]
            if body is None:  # hang: a model call still in flight
                finished.wait()
                return
            kind = "application/json"
            if isinstance(body, tuple):
                kind, body = "text/event-stream", b"".join(b"data: " + f + b"\n\n" for f in body[1])
            self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/v1/agent/model.call", requests
        finally:
            finished.set()
            server.shutdown()
            thread.join()


def response(text="done", args=None, usage=None):
    calls = b"[]" if args is None else b'[{"id":"1","name":"fx_tool","args":' + args + b"}]"
    extra = b"" if usage is None else b',"usage":' + json.dumps(usage).encode()
    return (
        b'{"status":"ok","model":"fixture","message":{"content":' + json.dumps(text).encode()
        + b'},"tool_calls":' + calls
        + b',"cost":{"class":"free_local","est_usd":0,"charged_usd":0}' + extra + b"}"
    )


def executable(path, script):
    path.write_text("#!/bin/sh\nset -eu\n" + script, encoding="utf-8")
    path.chmod(0o755)


def fixture(root, *, seconds=30, tool=None, pre_tool=None, post_turn=None, model="fixture", usd=0):
    estate = _harness.build_estate(
        root, V2 / "cases/happy-fs-write", {}, LAB, V2
    )
    executable(estate / "tasks/fx/check", "exit 0\n")
    for name, script in (("pre_tool", pre_tool), ("post_turn", post_turn)):
        if script is not None:
            executable(estate / "hooks/bin" / name, script)
    if tool is not None:
        executable(estate / "tools/bin/fx_tool", tool)
    (estate / "specs/fx.yaml").write_text(f"""id: fx-quality
model: {model}
system: prompts/coder-min.md
tools: {'[fx_tool]' if tool is not None else '[]'}
pre_tool: {'[pre_tool]' if pre_tool is not None else '[]'}
post_turn: {'[post_turn]' if post_turn is not None else '[]'}
budget: {{turns: 3, tool_calls: 5, usd: {usd}, seconds: {seconds}}}
tasks: [tasks/fx]
trials: 1
hypothesis: "fixture: Rust runner quality fixture"
""", encoding="utf-8")
    return estate


def run_dir(estate):
    run, = (estate / "ledger/runs").iterdir()
    return run


def events(run):
    return [json.loads(line) for line in (run / "events.jsonl").read_text().splitlines()]


def output(turn, delta=None, discard=False):
    payload = {"turn": turn, "partial": delta is not None, "delta": delta or ""}
    if discard:
        payload["discard"] = True
    return {"kind": "model.output", "payload": payload}


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def verify(estate, binary, build, responses, verdict, stop, *, before_exit=None, timeout=12, models=None):
    with gateway(responses, models) as (endpoint, requests):
        with _harness.launch(binary, estate, _harness.child_env(estate, endpoint, {}), {}) as proc:
            try:
                if before_exit:
                    before_exit(proc)
                out, err = proc.communicate(timeout=timeout)
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate()
        assert proc.returncode == (0 if verdict == "pass" else 1), (out, err)
    run = run_dir(estate)
    terminal = read_json(run / "terminal.json")
    finalized = read_json(run / "finalized.json")
    check = read_json(run / "check.json")
    row = read_json(estate / "ledger/ledger.jsonl")
    assert (terminal["run_id"], terminal["runner_implementation"], terminal["runner_build"]) == (
        run.name, "rust", build
    ), terminal
    for record in (terminal, finalized, row):
        assert record["stop"].startswith(stop), record
    assert finalized["verdict"] == row["verdict"] == verdict, (finalized, row)
    assert row["transcript"] == f"ledger/runs/{run.name}", row
    assert check["skipped"] == (verdict == "void"), check
    return run, requests


def test_raw_args(root, binary, build):
    estate = fixture(root, tool="cat > tool.json\n", pre_tool="cat > hook.json\n")
    # Include both a value rounded by f64 and one outside its representable range.
    args = b'{\n  "fraction":9007199254740993.0,"exponent":1e400\n}'
    one_line = args.replace(b"\n", b"")
    run, requests = verify(
        estate, binary, build, [response("call", args), response()], "pass", "done"
    )
    assert (run / "workspace/tool.json").read_bytes() == args
    hook = (run / "workspace/hook.json").read_bytes()
    assert json.loads(hook)["name"] == "fx_tool" and b'"args":' + args in hook, hook
    assert b'"args":' + one_line in (run / "messages.jsonl").read_bytes()
    assert one_line in (run / "transcript.md").read_bytes()
    assert len(requests) == 2 and b'"args":' + args in requests[1], requests


def test_unicode(root, binary, build):
    estate = fixture(root)
    text = "before\u2028between\u2029after\u0085end"
    run, _ = verify(estate, binary, build, [response(text)], "pass", "done")
    assert text in (run / "transcript.md").read_text(encoding="utf-8")


def test_overflow_budget(root, binary, build):
    estate = fixture(root, seconds=2**63 - 1)
    _, requests = verify(estate, binary, build, [], "pass", "budget:seconds")
    assert requests == [], requests


def test_event_failure(root, binary, build):
    estate = fixture(
        root, tool="exit 0\n", pre_tool="rm ../messages.jsonl\nmkdir ../messages.jsonl\n"
    )
    run, _ = verify(estate, binary, build, [response("call", b"{}")], "void", "error: messages.jsonl")
    assert (run / "messages.jsonl").is_dir()


def test_interrupt(root, binary, build):
    estate = fixture(root, post_turn="""
(touch hook-entered; sleep 3; touch late-hook-write) &
wait
""")

    def interrupt(proc):
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline and proc.poll() is None:
            if list(estate.glob("ledger/runs/*/workspace/hook-entered")):
                proc.send_signal(signal.SIGINT)
                return
            time.sleep(0.01)
        raise AssertionError("runner never reached the post-turn hook")

    run, _ = verify(estate, binary, build, [response()], "void", "interrupted", before_exit=interrupt)
    assert_cancelled(run, "SIGINT")
    time.sleep(3)
    assert not (run / "workspace/late-hook-write").exists(), "hook wrote after interruption"


def envelope(estate, run):
    return read_json(estate / "ledger/outbox" / (run.name + ".json"))


def test_usage_totals(root, binary, build):
    estate = fixture(root, tool="exit 0\n")
    replies = [response("call", b"{}", {"input_tokens": 10, "output_tokens": 5}),
               response(usage={"input_tokens": 7, "output_tokens": 3})]
    run, _ = verify(estate, binary, build, replies, "pass", "done")
    terminal = read_json(run / "terminal.json")
    assert (terminal["input_tokens"], terminal["output_tokens"]) == (17, 8), terminal
    assert (terminal["terminal_state"], terminal["cancel_reason"]) == ("completed", None), terminal
    usage = envelope(estate, run)["usage"]
    assert (usage["input_tokens"], usage["output_tokens"]) == (17, 8), usage


def test_usage_missing_is_null(root, binary, build):
    estate = fixture(root, tool="exit 0\n")
    replies = [response("call", b"{}", {"input_tokens": 10, "output_tokens": 5}),
               response(usage={"input_tokens": 7})]
    run, _ = verify(estate, binary, build, replies, "pass", "done")
    terminal = read_json(run / "terminal.json")
    assert (terminal["input_tokens"], terminal["output_tokens"]) == (17, None), terminal
    usage = envelope(estate, run)["usage"]
    assert (usage["input_tokens"], usage["output_tokens"]) == (17, None), usage


def sigterm_when(estate, ready):
    def send(proc):
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline and proc.poll() is None:
            if ready():
                proc.send_signal(signal.SIGTERM)
                return
            time.sleep(0.01)
        raise AssertionError("runner never reached the cancellation point")
    return send


def assert_cancelled(run, cause="SIGTERM"):
    terminal = read_json(run / "terminal.json")
    assert (terminal["terminal_state"], terminal["cancel_reason"]) == ("cancelled", cause), terminal
    assert terminal["verdict"] == "void" and terminal["stop"] == "interrupted", terminal
    assert read_json(run / "finalized.json")["verdict"] == "void"
    assert envelope(run.parent.parent.parent, run)["run_id"] == run.name
    return terminal


def test_sigterm_mid_tool(root, binary, build):
    estate = fixture(root, tool="""
(sleep 3; touch late-group-write) &
touch tool-entered
sleep 30
""")
    ready = lambda: list(estate.glob("ledger/runs/*/workspace/tool-entered"))
    started = time.monotonic()
    run, requests = verify(estate, binary, build, [response("call", b"{}", {"input_tokens": 4, "output_tokens": 2})],
                           "void", "interrupted", before_exit=sigterm_when(estate, ready))
    assert time.monotonic() - started < 8, "the tool's sleep outlived the cancellation"
    terminal = assert_cancelled(run)
    assert (terminal["input_tokens"], terminal["output_tokens"], terminal["tool_calls"]) == (4, 2, 1), terminal
    events = [json.loads(line) for line in (run / "messages.jsonl").read_text().splitlines()]
    assert events[-1]["event"] == "tool" and events[-1]["error"], events[-1]
    assert len(requests) == 1, requests
    time.sleep(3.5)
    assert not (run / "workspace/late-group-write").exists(), "the tool's process group survived SIGTERM"


def test_sigterm_mid_model_call(root, binary, build):
    estate = fixture(root)
    requests_seen = []
    with gateway([None]) as (endpoint, requests):
        requests_seen = requests
        with _harness.launch(binary, estate, _harness.child_env(estate, endpoint, {}), {}) as proc:
            started = time.monotonic()
            try:
                sigterm_when(estate, lambda: bool(requests))(proc)
                out, err = proc.communicate(timeout=8)
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate()
        assert proc.returncode == 1, (out, err)
        assert time.monotonic() - started < 8
    run = run_dir(estate)
    terminal = assert_cancelled(run)
    assert (terminal["input_tokens"], terminal["output_tokens"]) == (None, None), terminal
    assert len(requests_seen) == 1
    assert "interrupt signal received" in (run / "transcript.md").read_text()


def test_descendant_pipe(root, binary, build):
    # The parent exits while its descendant still holds stdout. Cancel only after the child is ready.
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(30)
        child = ("import socket; "
                 f"s = socket.create_connection(('127.0.0.1', {listener.getsockname()[1]})); "
                 "s.sendall(b'ready'); s.recv(1); open('late-write', 'w').write('alive')")
        parent = f"import subprocess, sys; subprocess.Popen([sys.executable, '-c', {child!r}])"
        estate = fixture(root, tool=f"exec python3 -c {shlex.quote(parent)}\n")
        connections = []

        def cancel(proc):
            connection, _ = listener.accept()
            connections.append(connection)
            connection.settimeout(30)
            assert connection.recv(5) == b"ready"
            proc.send_signal(signal.SIGTERM)

        try:
            run, _ = verify(estate, binary, build, [response("call", b"{}")], "void", "interrupted",
                            before_exit=cancel, timeout=30)
            assert connections[0].recv(1) == b"", "the descendant survived cancellation"
            assert not (run / "workspace/late-write").exists()
        finally:
            for connection in connections:
                connection.close()


def test_image_limit(root, binary, build):
    # Keep the image itself valid. JSON whitespace pads the complete envelope to the limit.
    png = ("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8"
           "/x8AAwMCAO+jRuoAAAAASUVORK5CYII=")
    envelope = json.dumps({"text": "pixel", "image": {"media_type": "image/png", "base64": png}})
    limit = 16 * 1024 * 1024
    for size in (limit, limit + 1):
        case = root / str(size)
        case.mkdir()
        estate = fixture(case, tool="cat image.json\n")
        descriptor = estate / "tools/bin/fx_tool.json"
        descriptor.write_text(json.dumps({**read_json(descriptor), "image_out": True}))
        # Place the padded envelope in the tool's task setup.
        setup = estate / "tasks/fx/setup"
        setup.mkdir(exist_ok=True)
        (setup / "image.json").write_text(envelope + " " * (size - len(envelope)))
        _, requests = verify(
            estate, binary, build, [response("call", b"{}"), response()], "pass", "done"
        )
        message = json.loads(requests[1])["messages"][-1]
        assert message["role"] == "tool", message
        if size == limit:
            assert "is_error" not in message, message
            assert message["content"][1] == {
                "type": "image", "media_type": "image/png", "base64": png
            }, message
        else:
            assert message["is_error"] and f"output exceeds {limit} bytes" in message["content"]
            assert png not in message["content"], message


def gateway_binary():
    """The built cs-gateway (CSGATEWAY_BIN, else target/release, else target/debug); never the live install."""
    repo = LAB.parent
    candidates = [Path(os.environ["CSGATEWAY_BIN"])] if os.environ.get("CSGATEWAY_BIN") else [
        repo / "target/release/cs-gateway", repo / "target/debug/cs-gateway"]
    for path in candidates:
        if path.is_file() and ".contextstack" not in path.parts:
            return path
    raise SystemExit("quality: cs-gateway is not built: cargo build --release -p cs-gateway")


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


@contextmanager
def mock_gateway(root, paid=False):
    """A real csgateway-rs on a free port serving only the compiled-in mock: temp HOME, no credentials,
    no reachable upstream (any outbound connection hits a dead proxy). `paid`: the mock is api class at
    $1 per 1k tokens under a $100 source budget, so the gateway prices, counts and admits its calls."""
    home, port = root / "gw-home", free_port()
    (home / "tmp").mkdir(parents=True)
    models = home / "models.json"
    cost = {"cost_class": "api", "cost_per_1k": 1.0} if paid else {"cost_class": "free_local"}
    models.write_text(json.dumps({"models": [
        {"name": "mock-echo", "provider": "mock", "tier": 3, "capability": "general", **cost}]}))
    if paid:
        limits = home / ".contextstack/config/gateway/limits.json"
        limits.parent.mkdir(parents=True)
        limits.write_text(json.dumps({"sources": {"mock": {"usd": 100, "period": "day"}}}))
    dead = "http://127.0.0.1:9"
    env = {"HOME": str(home), "PATH": "/usr/bin:/bin", "LANG": "C", "TMPDIR": str(home / "tmp"),
           "CSGATEWAY_PORT": str(port), "CSAGENT_MODELS_CONFIG": str(models),
           "CSGATEWAY_AUTH_REQUIRED": "false", "CSGATEWAY_CREDENTIALS": "off",
           "CSGATEWAY_DISCOVERY_HEALTH": "off", "OLLAMA_ENDPOINT": dead, "VLLM_ENDPOINT": dead,
           "HTTP_PROXY": dead, "HTTPS_PROXY": dead, "http_proxy": dead, "https_proxy": dead}
    with open(home / "gateway.log", "ab") as log:
        proc = subprocess.Popen([str(gateway_binary())], cwd=home, env=env, stdout=log,
                                stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
    try:
        url = "http://127.0.0.1:%d" % port
        deadline = time.monotonic() + 20
        while True:
            assert proc.poll() is None, (home / "gateway.log").read_text()
            try:
                with urllib.request.urlopen(url + "/v1/health", timeout=2):
                    break
            except OSError:
                assert time.monotonic() < deadline, "mock gateway never became healthy"
                time.sleep(0.05)
        yield url
    finally:
        proc.terminate()
        try:
            proc.wait(5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


@contextmanager
def proxy(upstream):
    """Forward to `upstream`; `mode["stream"] = False` refuses the probe, so the runner stays unary."""
    mode, posts = {"stream": True}, []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def relay(self, body=None):
            if body is None and not mode["stream"]:
                self.send_error(404, "probe refused")
                return
            if body is not None:
                posts.append(json.loads(body))
            request = urllib.request.Request(upstream + self.path, data=body, headers={
                "Content-Type": "application/json"} if body is not None else {})
            with urllib.request.urlopen(request, timeout=30) as reply:
                raw, kind = reply.read(), reply.headers.get("Content-Type", "")
            self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            self.relay()

        def do_POST(self):
            self.relay(self.rfile.read(int(self.headers["Content-Length"])))

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/v1/agent/model.call", mode, posts
        finally:
            server.shutdown()
            thread.join()


# stimulus_sha binds the run id (capture-stimulus), so it differs between any two runs.
VOLATILE = {"run_id", "run_dir", "transcript", "workspace", "stimulus_dir", "stimulus_sha", "started", "ended", "seconds"}


def launch_once(binary, estate, endpoint):
    with _harness.launch(binary, estate, _harness.child_env(estate, endpoint, {}), {}) as proc:
        out, err = proc.communicate(timeout=20)
    runs = sorted((estate / "ledger/runs").iterdir(), key=lambda d: d.stat().st_mtime_ns)
    return proc.returncode, runs[-1], (out, err)


def record(run):
    """terminal.json without its timing and per-run names, plus the transcript with the run named generically."""
    terminal = {k: v for k, v in read_json(run / "terminal.json").items() if k not in VOLATILE}
    messages = (run / "messages.jsonl").read_text().replace(run.name, "<RUN>")
    transcript = (run / "transcript.md").read_text().replace(run.name, "<RUN>")
    return terminal, messages, transcript


def streamed_and_unary(root, binary, **spec):
    estate = fixture(root, model="mock-echo", **spec)
    with mock_gateway(root) as upstream, proxy(upstream) as (endpoint, mode, posts):
        code, streamed, out = launch_once(binary, estate, endpoint)
        assert code == 0, out
        assert posts and all(post.get("stream") is True for post in posts), posts
        mode["stream"], count = False, len(posts)
        code, unary, out = launch_once(binary, estate, endpoint)
        assert code == 0, out
        assert posts[count:] and all("stream" not in post for post in posts[count:]), posts
    assert record(streamed) == record(unary), (record(streamed), record(unary))
    assert not (unary / "events.jsonl").exists(), "a unary run writes no events file"
    lines = events(streamed)
    assert [line["seq"] for line in lines] == list(range(1, len(lines) + 1)), lines
    assert read_json(streamed / "finalized.json")["verdict"] == "pass"
    return [{k: v for k, v in line.items() if k != "seq"} for line in lines], read_json(streamed / "terminal.json")


def test_stream_text_turn(root, binary, build):
    lines, terminal = streamed_and_unary(root, binary)
    assert lines == [output(1, "mock "), output(1, "provider "), output(1, "response"), output(1)], lines
    assert (terminal["model"], terminal["stop"], terminal["turns"]) == ("mock-echo", "done", 1), terminal
    assert (terminal["input_tokens"], terminal["output_tokens"]) == (20, 30), terminal


def test_stream_tool_turn(root, binary, build):
    lines, terminal = streamed_and_unary(root, binary, tool="cat > tool.json\n")
    assert lines == [output(1), output(2, "mock "), output(2, "provider "), output(2, "response"), output(2)], lines
    assert (terminal["stop"], terminal["turns"], terminal["tool_calls"]) == ("done", 2, 1), terminal
    assert (terminal["input_tokens"], terminal["output_tokens"]) == (40, 60), terminal


def test_stream_error_mid_stream(root, binary, build):
    error = {"class": "runtime", "code": "gateway.upstream_error", "message": "boom"}
    frames = (b'{"kind":"delta","content":"half "}', b'{"kind":"delta","content":"said"}',
              b'{"kind":"error","cost":{"currency":"USD","amount":"0.000000"},"error":' + json.dumps(error).encode() + b"}")
    unary_error = json.dumps({"status": "error", "error": error}).encode()
    results = []
    for replies, models in (([("sse", frames)], [{"name": "fixture", "streaming": True}]), ([unary_error], None)):
        case = root / str(len(results))
        case.mkdir()
        run, requests = verify(fixture(case), binary, build, replies, "void", "error: gateway runtime [gateway.upstream_error]: boom", models=models)
        results.append((run, record(run)))
    (streamed, streamed_record), (unary, unary_record) = results
    normal = lambda r: (r[0] | {k: None for k in ("task_sha", "env_sha", "stimulus_sha")}, r[1], r[2])
    assert normal(streamed_record) == normal(unary_record), (streamed_record, unary_record)
    assert streamed_record[0]["cost_usd"] == 0 and streamed_record[0]["input_tokens"] is None, streamed_record
    assert "half" not in streamed_record[1] + streamed_record[2], "partial text reached the transcript"
    assert [{k: v for k, v in line.items() if k != "seq"} for line in events(streamed)] == [
        output(1, "half "), output(1, "said"), output(1, discard=True)]
    assert not (unary / "events.jsonl").exists()


def test_stream_unsupported_fallback(root, binary, build):
    estate = fixture(root, tool="exit 0\n")
    refusal = (b'{"kind":"error","cost":{"currency":"USD","amount":"0.000000"},"error":{"class":"fatal_session",'
               b'"code":"gateway.streaming_unsupported","message":"endpoint cannot stream"}}',)
    replies = [("sse", refusal), response("call", b"{}"), response()]
    run, requests = verify(estate, binary, build, replies, "pass", "done", models=[{"name": "fixture", "streaming": True}])
    bodies = [json.loads(r) for r in requests]
    assert [b.get("stream") for b in bodies] == [True, None, None], "one stream attempt, then unary for the run"
    assert bodies[0]["messages"] == bodies[1]["messages"], "the fallback resends the same call"
    terminal = read_json(run / "terminal.json")
    assert (terminal["turns"], terminal["tool_calls"], terminal["cost_usd"]) == (2, 1, 0), terminal
    assert not (run / "events.jsonl").exists(), "nothing streamed, so no events file"


def paid_run(root, binary, usd, tool, stream=True):
    """One run of the api-class mock; returns the run dir, its terminal record and the posted bodies."""
    estate = fixture(root, model="mock-echo", usd=usd, tool=tool)
    with mock_gateway(root, paid=True) as upstream, proxy(upstream) as (endpoint, mode, posts):
        mode["stream"] = stream
        code, run, out = launch_once(binary, estate, endpoint)
        assert code == 0, out
    return run, read_json(run / "terminal.json"), posts


def test_usd_ceiling_shrinks_by_charges(root, binary, build):
    """Each call's budget.max_usd is budget.usd less the run's charges so far ($0.05 a mock call)."""
    run, terminal, posts = paid_run(root, binary, 20, "exit 0\n")
    assert [p["budget"].get("max_usd") for p in posts] == ["20.000000", "19.950000"], posts
    assert (terminal["stop"], terminal["turns"], terminal["cost_class"]) == ("done", 2, "api"), terminal
    assert terminal["cost_usd"] == 0.1, terminal


def test_usd_ceiling_refusal_ends_the_run(root, binary, build):
    """8 KB of tool output lifts the second call's estimate (~$6.5) over what is left of $5.50 (5.45):
    the gateway refuses it at $0 and the run stops on the ceiling, not an error."""
    for stream in (True, False):
        case = root / ("stream" if stream else "unary")
        case.mkdir()
        tool = "head -c 8192 /dev/zero | tr '\\0' x\n"
        run, terminal, posts = paid_run(case, binary, 5.5, tool, stream)
        assert [p["budget"].get("max_usd") for p in posts] == ["5.500000", "5.450000"], posts
        assert (terminal["stop"], terminal["terminal_state"], terminal["turns"]) == (
            "budget:usd_ceiling", "completed", 2), terminal
        assert terminal["cost_usd"] == 0.05, "the refused call costs $0"
        assert read_json(run / "finalized.json")["stop"] == "budget:usd_ceiling"
        errors = [json.loads(line) for line in (run / "messages.jsonl").read_text().splitlines()]
        errors = [e for e in errors if e.get("event") == "error"]
        assert len(errors) == 1 and errors[0]["turn"] == 2, errors
        assert errors[0]["error"].startswith("gateway fatal_session [gateway.policy_denied]: "), errors


def test_usd_zero_sends_no_ceiling(root, binary, build):
    run, terminal, posts = paid_run(root, binary, 0, "exit 0\n")
    assert len(posts) == 2 and all("max_usd" not in p["budget"] for p in posts), posts
    assert (terminal["stop"], terminal["cost_usd"]) == ("done", 0.1), terminal


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    binary = parser.parse_args().binary.resolve(strict=True)
    build = "sha256:" + hashlib.sha256(binary.read_bytes()).hexdigest()
    for test in (
        test_raw_args, test_unicode, test_overflow_budget, test_event_failure,
        test_interrupt, test_descendant_pipe, test_image_limit, test_usage_totals,
        test_usage_missing_is_null, test_sigterm_mid_tool, test_sigterm_mid_model_call,
        test_stream_text_turn, test_stream_tool_turn, test_stream_error_mid_stream,
        test_stream_unsupported_fallback, test_usd_ceiling_shrinks_by_charges,
        test_usd_ceiling_refusal_ends_the_run, test_usd_zero_sends_no_ceiling,
    ):
        with tempfile.TemporaryDirectory(prefix=f"cs-{test.__name__}-") as root:
            test(Path(root), binary, build)
        print(f"ok {test.__name__}")


if __name__ == "__main__":
    main()
