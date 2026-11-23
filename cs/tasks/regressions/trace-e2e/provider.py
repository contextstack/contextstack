#!/usr/bin/env python3
"""Loopback OpenAI upstream scripted per TRACE_E2E_SCENARIO."""
import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

USAGE = (37, 12)
CHARGE = 0.02


def tool(tool_id, name, **args):
    return {"id": tool_id, "type": "function",
            "function": {"name": name, "arguments": json.dumps(args)}}


def chat_completion(text="ok", tool_calls=None, usage=USAGE, cost=CHARGE):
    msg = {"role": "assistant", "content": text}
    if tool_calls:
        msg["tool_calls"] = tool_calls
    u = {"prompt_tokens": usage[0], "completion_tokens": usage[1], "total_tokens": usage[0] + usage[1]}
    if cost is not None:
        u["cost"] = cost
    return {
        "id": "chatcmpl-spec", "object": "chat.completion", "model": "m",
        "choices": [{"index": 0, "finish_reason": "stop", "message": msg}], "usage": u,
    }


def chat_chunk(delta=None, finish=None, usage=None, choices=True, cost=None):
    body = {
        "id": "chatcmpl-spec", "object": "chat.completion.chunk", "model": "m",
        "choices": [{"index": 0, "delta": delta or {}, "finish_reason": finish}] if choices else [],
    }
    if usage:
        body["usage"] = {
            "prompt_tokens": usage[0], "completion_tokens": usage[1],
            "total_tokens": sum(usage),
        }
        if cost is not None:
            body["usage"]["cost"] = cost
    return body


def sse_data(obj):
    return ("data: %s\n\n" % json.dumps(obj)).encode()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready", required=True, type=Path)
    parser.add_argument("--scenario", required=True)
    args = parser.parse_args()
    scenario = args.scenario
    lock = threading.Lock()
    calls = 0

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            if self.path != "/v1/models":
                self.send_error(404)
                return
            models = ["fake-paid", "local-coder"] if scenario in {"Y", "Z"} else ["fake-paid"]
            body = json.dumps({"object": "list", "data": [{"id": model, "object": "model"} for model in models]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            if self.path != "/v1/chat/completions":
                self.send_error(404)
                return
            request = self.rfile.read(int(self.headers["Content-Length"]))
            nonlocal calls
            with lock:
                calls += 1
                call = calls
            if scenario in {"A", "G"} and call == 1:
                self.send_response(503)
                self.send_header("Retry-After", "1")
                self.end_headers()
                return
            if scenario == "A":
                return self._json(chat_completion("done"))

            if scenario == "C":
                return self._sse_slow()

            if scenario in {"action-go", "action-no-go"}:
                if call == 1:
                    from action_approval import ACTION
                    return self._json(chat_completion("", tool_calls=[
                        tool("action-first", "request_action", **ACTION),
                        tool("action-repeat", "request_action", **ACTION),
                    ]))
                observations = [str(m.get("content")) for m in json.loads(request)["messages"]
                                if m["role"] == "tool"]
                expected = ("approved command output", "single execution claim") if scenario == "action-go" else (
                    "waiting on the owner", "Continue without it if possible")
                if not all(any(word in observation for observation in observations) for word in expected):
                    return self.send_error(400, "missing action observation")
                return self._json(chat_completion("action observations received"))
            if scenario == "D":
                tools = [{"id": "1", "type": "function", "function": {"name": "fx_tool", "arguments": '{"mode":"sleep","seconds":0.25}'}}]
                return self._json(chat_completion("", tool_calls=tools, cost=0))

            if scenario == "F":
                if call <= 2:
                    tools = [{"id": "1", "type": "function", "function": {"name": "fx_tool", "arguments": '{"mode":"echo"}'}}]
                    return self._json(chat_completion("", tool_calls=tools, cost=0))
                return self._json(chat_completion("done", cost=0))

            if scenario == "G":
                return self._partial()

            if scenario == "I":
                tools = [{"id": "1", "type": "function", "function": {"name": "missing_tool", "arguments": "{}"}}]
                return self._json(chat_completion("", tool_calls=tools, cost=0))

            if scenario == "J":
                return self._partial(hold=True)

            if scenario in {"L", "O"}:
                time.sleep(4)
                return self._json(chat_completion("late"))

            if scenario == "Y":
                body = json.loads(request)
                assert "tool_choice" not in body
                if call == 1:
                    assert any("You are a coding worker" in m.get("content", "") for m in body["messages"])
                    assert {t["function"]["name"] for t in body["tools"]} == {
                        "fs_read", "fs_write", "fs_edit", "exec", "repo_search", "run_check", "request_action"}
                    args = json.dumps({"path": "result.txt", "content": "edited by native harness\n"})
                    malformed = '{"path":"result.txt","content":"' + "x" * 13_000
                    tools = [
                        {"id": "bad-write", "type": "function",
                         "function": {"name": "fs_write", "arguments": malformed}},
                        {"id": "1", "type": "function", "function": {"name": "fs_write", "arguments": args}},
                    ]
                    return self._json(chat_completion("", tool_calls=tools, cost=0))
                if call == 2:
                    result = next(m["content"] for m in body["messages"] if m.get("tool_call_id") == "bad-write")
                    assert "arguments were not valid JSON" in result and "fs_edit" in result, result
                    bare_exec = {"id": "shell", "type": "function",
                                 "function": {"name": "exec", "arguments": json.dumps("echo shell | tail -n 1")}}
                    return self._json(chat_completion("", tool_calls=[bare_exec, tool("verify", "run_check")], cost=0))
                shell = next(m["content"] for m in body["messages"] if m.get("tool_call_id") == "shell")
                assert shell.endswith("\nshell\n"), shell
                result = next(m["content"] for m in body["messages"] if m.get("tool_call_id") == "verify")
                assert "Check exit code: 0." in result and "fixture-cargo" in result, result
                return self._json(chat_completion("done", cost=0))

            if scenario == "Z":
                body = json.loads(request)
                messages = body["messages"]
                system = next(m["content"] for m in messages if m["role"] == "system")
                remaining = 5 - call
                reminder = f"Turns remaining (including this turn): {remaining}."
                if remaining <= 2:
                    reminder += "\nProduce the deliverable requested in the task goal now, then stop."
                workspace_prefix = "Your workspace (write deliverables here, relative paths resolve here): "
                workspace_lines = [line for line in system.splitlines() if line.startswith(workspace_prefix)]
                assert len(workspace_lines) == (call == 1 or remaining == 1), system
                if workspace_lines:
                    workspace = Path(workspace_lines[0].removeprefix(workspace_prefix))
                    assert workspace.is_absolute() and workspace.is_dir(), workspace
                    reminder += "\n" + workspace_lines[0]
                assert system.endswith("\n\n" + reminder), system
                assert system.count("Turns remaining (including this turn)") == 1, system
                assert system.count("Produce the deliverable") == (remaining <= 2), system
                names = {t["function"]["name"] for t in body["tools"]}
                assert names == ({"fs_write"} if remaining == 1 else
                                 {"fs_read", "fs_write", "exec", "repo_search", "request_action"}), names

                if remaining == 1:
                    assert body["tool_choice"] == {"type": "function", "function": {"name": "fs_write"}}
                else:
                    assert "tool_choice" not in body

                if call == 1:
                    assert any("You are a researcher" in m.get("content", "") for m in messages)
                    prefix = "Read the repository snapshot at "
                    goal = next(m["content"] for m in messages if m["role"] == "user")
                    assert goal.startswith(prefix), goal
                    snapshot = goal.removeprefix(prefix).split(". It is read-only.", 1)[0]
                    path = str(Path(snapshot) / "facts.txt")
                    calls_out = [
                        tool("read", "fs_read", path=path),
                        tool("grep", "exec", cmd=["grep", "snapshot fact", path]),
                        tool("write", "fs_write", path=path, content="wrong\n"),
                        tool("shell", "exec", cmd=["sh", "-c", 'echo wrong > "$1"', "sh", path]),
                        tool("read-back", "fs_read", path=path),
                        tool("missing", "fs_read", path="facts.txt"),
                        tool("redirect", "exec", cmd=["ls", "facts.txt", "2>/dev/null"]),
                        tool("relative", "exec", cmd=["ls", "facts.txt"]),
                        tool("files", "repo_search", mode="files", query="facts"),
                        tool("text", "repo_search", mode="text", query="snapshot fact", path=snapshot),
                        tool("parent", "repo_search", mode="files", query="facts", path="../"),
                        tool("absolute", "repo_search", mode="text", query="fact", path="/"),
                        tool("symlink", "repo_search", mode="files", query="facts", path="outside"),
                    ]
                    return self._json(chat_completion("", tool_calls=calls_out, cost=0))
                if call in (2, 3, 4):
                    results = {m["tool_call_id"]: m["content"] for m in messages if m["role"] == "tool"}
                    fact = "committed snapshot fact"
                    if call == 2:
                        assert "facts.txt" in results["missing"] and "path does not exist" in results["missing"], results
                        assert "2>/dev/null" in results["redirect"] and "Pass argv as a list" in results["redirect"], results
                        assert "Relative paths resolve in your workspace" in results["relative"], results
                        assert results["files"].splitlines()[1].endswith("/facts.txt"), results
                        assert results["text"].splitlines()[1].endswith(f"/facts.txt:1: {fact}"), results
                        for tool_id in ("parent", "absolute", "symlink"):
                            assert "ERROR" in results[tool_id] and "outside the repository view" in results[tool_id], results
                        return self._json(chat_completion("", tool_calls=[tool("confirm", "fs_read", path=".")], cost=0))
                    for tool_id in ("read", "grep", "read-back"):
                        assert results[tool_id].splitlines()[1:] == [fact], results
                    assert "ERROR" in results["write"].splitlines()[0], results
                    assert "ERROR" in results["shell"].splitlines()[0], results
                    assert any(reason in results["shell"].lower() for reason in
                               ("read-only file system", "operation not permitted", "permission denied")), results
                    if call == 3:
                        assert "ERROR" not in results["confirm"].splitlines()[0], results
                        return self._json(chat_completion("", tool_calls=[tool("recheck", "repo_search", mode="text", query=fact)], cost=0))
                    observed = results["recheck"].splitlines()[1].split(": ", 1)[1]
                    answer = tool("answer", "fs_write", path="answer.md", content=observed + "\n")
                    return self._json(chat_completion("", tool_calls=[answer], cost=0))
                return self._json(chat_completion("done", cost=0))

            if scenario == "M":
                return self._json(chat_completion("local ok", cost=0))

            if scenario == "N":
                # A subscription call: the gateway prices an estimate; the provider reports no charge.
                return self._json(chat_completion("sub ok", cost=None))

            return self._json(chat_completion("ok", cost=CHARGE))

        def _json(self, payload):
            data = json.dumps(payload).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def _sse_slow(self):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for chunk in (sse_data(chat_chunk({"role": "assistant", "content": ""})), sse_data(chat_chunk({"content": "first"}))):
                self.wfile.write(chunk)
                self.wfile.flush()
            time.sleep(0.45)
            for chunk in (
                sse_data(chat_chunk({"content": " second"})),
                sse_data(chat_chunk({}, "stop")),
                sse_data(chat_chunk(usage=USAGE, choices=False, cost=CHARGE)),
                b"data: [DONE]\n\n",
            ):
                self.wfile.write(chunk)
                self.wfile.flush()

        def _partial(self, hold=False):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(sse_data(chat_chunk({"content": "partial output"})))
            self.wfile.write(sse_data(chat_chunk(usage=USAGE, choices=False, cost=CHARGE)))
            self.wfile.flush()
            if hold:
                time.sleep(10)
            # Closing before [DONE] is an upstream transport fault.

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    ready_tmp = args.ready.with_suffix(".tmp")
    ready_tmp.write_text("http://127.0.0.1:%d" % server.server_port)
    ready_tmp.replace(args.ready)
    server.serve_forever()


if __name__ == "__main__":
    main()
