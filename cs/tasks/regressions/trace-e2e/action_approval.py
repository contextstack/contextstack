"""Scripted harness actions against a fake node API and a recording sandbox."""
import json
import shutil
import sys
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ACTION = {"argv": ["/usr/bin/curl", "-q", "--", "https://docs.rs/serde"],
          "needs": ["network"], "reason": "Read the requested documentation."}


@contextmanager
def director_fixture(stack, job, model, go, *, cli=False):
    from estate import CS, REPO, write_json

    root = stack.estate
    for name in ("request_action", "request_action.json"):
        shutil.copy2(CS / "tools/bin" / name, root / "tools/bin" / name)
    wrapper = root.parent / "bin/cs-sandbox" if cli else root / "cs-sandbox"
    if cli:
        wrapper.unlink()
    wrapper.write_text(f"#!{sys.executable}\n" + """
import json, os, sys
from pathlib import Path
if "--action-widening" not in sys.argv:
    os.execv(SANDBOX, [SANDBOX, *sys.argv[1:]])
assert sys.argv[1:2] == ["--policy"]
assert sys.argv[3] == "--action-widening"
assert json.loads(sys.argv[4]) == {"network": True, "delete": False, "write_paths": []}
assert sys.argv[5:] == ["--", "/usr/bin/curl", "-q", "--", "https://docs.rs/serde"]
assert "ACTION_TEST_TOKEN" not in os.environ
with Path("action-marker").open("a") as marker:
    marker.write("ran once\\n")
print("approved command output")
""".replace("SANDBOX", repr(str(REPO / "target/release/cs-sandbox"))))
    wrapper.chmod(0o755)
    (root / "policy").mkdir(exist_ok=True)
    (root / "ops/bin/_node_env.py").write_text(
        'def node_env(path=None): return {"ACTION_TEST_TOKEN": "fake-action-node"}\n')
    stack.child_env["CS_TOOL_PREFIX"] = f"{wrapper} --policy {root / 'policy/sandbox.json'} --"

    received = []
    record = {}

    class Director(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            received.append((self.path, body, self.headers["Authorization"]))
            op = self.path.rsplit("/", 1)[-1]
            if op == "action-approvals" and not record:
                record.update(id="approval-fixture", status="pending", **ACTION,
                              write_paths=[], requesting_model=model)
            elif op == "poll":
                record.update(status="approved" if go else "needs_owner",
                              verdict="go" if go else "no_go", reviewer_model="fixture-reviewer",
                              review_reason="fixture independent review")
            elif op == "claim":
                record["claimed_at"] = "now"
            elif op == "outcome":
                record["outcome"] = {"exit_code": body["exit_code"], "status": "succeeded"}
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("ContextStack-Protocol", "2")
            self.send_header("ContextStack-Installation", "fixture")
            self.end_headers()
            self.wfile.write(json.dumps(record).encode())

    with ThreadingHTTPServer(("127.0.0.1", 0), Director) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            write_json(root / "policy/spool.json", {"receiver": {
                "kind": "director", "node": "fixture", "installation": "fixture",
                "url": f"http://127.0.0.1:{server.server_port}", "token_env": "ACTION_TEST_TOKEN",
            }})
            yield received
        finally:
            server.shutdown()
            thread.join()
    assert all(token == "Bearer fake-action-node" for _, _, token in received), received
    assert sum(path.endswith("/action-approvals") for path, _, _ in received) == 2, received
    assert received[0][1]["context"]["goal"] == "Respond and finish.\n", received
    assert all(path.startswith(f"/api/v1/nodes/me/jobs/{job}/action-approvals")
               for path, _, _ in received), received
    assert sum(path.endswith("/claim") for path, _, _ in received) == int(go), received
    assert sum(path.endswith("/outcome") for path, _, _ in received) == int(go), received


def assert_execution(run_dir, received, go):
    assert received[0][1]["run_id"] == run_dir.name, received
    marker = run_dir / "workspace/action-marker"
    if go:
        assert marker.read_text() == "ran once\n"
    else:
        assert not marker.exists()


def run(stack, spec, go):
    from estate import jsonl

    job = "action-fixture-job"
    with director_fixture(stack, job, "fake-paid", go) as received:
        run_dir = stack.run_spec(spec, job_id=job)
    assert_execution(run_dir, received, go)
    spans = jsonl(run_dir / "trace.jsonl")
    actions = [span["attributes"]["action_approval"] for span in spans
               if span["kind"] == "tool_process" and span["name"] == "request_action"]
    assert len(actions) == 2, actions
    assert all(action["verdict"] == ("go" if go else "no_go") for action in actions), actions
    assert all(action["reviewer_model"] == "fixture-reviewer" for action in actions), actions
    return run_dir


def cli_test(stack, go):
    from _lane_mcp import consume_binding, stage

    binding = stack.scratch / "action-mcp-binding"
    token = "ab" * 32
    job = "trace-cli"
    binding.write_text(json.dumps({"job": job, "token": token}))
    assert not consume_binding(stack.scratch, "other-job", token)
    assert not consume_binding(stack.scratch, job, "ba" * 32)
    assert consume_binding(stack.scratch, job, token)
    assert not consume_binding(stack.scratch, job, token)

    config = {"mcpServers": {"contextstack": {"command": "/fixture/python", "args": ["proxy"]}}}
    codex = stage("codex", ["exec", "--ignore-user-config", "goal"], {}, config)
    assert codex[:3] == ["exec", "-c", 'mcp_servers.contextstack.command="/fixture/python"']
    assert 'mcp_servers.contextstack.args=["proxy"]' in codex and codex[-1] == "goal"
    cursor = stack.scratch / "cursor"
    cursor.mkdir()
    args = ["--print", "goal"]
    assert stage("cursor-agent", args, {"CURSOR_CONFIG_DIR": str(cursor)}, config) == args
    assert json.loads((cursor / "mcp.json").read_text()) == config

    with director_fixture(stack, job, job, go, cli=True) as received:
        run = stack.run_cli(action=go)
    assert_execution(run, received, go)
    assert not (run / "action-mcp-binding").exists()
    assert not list((run / "tmp").glob("lane-mcp-capability-*"))
    return run


def fake_cli_source(go):
    return r"""
import json, os, subprocess, sys
assert "--strict-mcp-config" in sys.argv
assert any("Use the request_action MCP tool" in arg for arg in sys.argv)
assert "ACTION_TEST_TOKEN" not in os.environ
server = json.loads(sys.argv[sys.argv.index("--mcp-config") + 1])["mcpServers"]["contextstack"]
with subprocess.Popen([server["command"], *server["args"]], stdin=subprocess.PIPE,
                      stdout=subprocess.PIPE, text=True) as proxy:
    def rpc(method, params):
        proxy.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}) + "\n")
        proxy.stdin.flush()
        reply = json.loads(proxy.stdout.readline())
        assert reply["id"] == 1, reply
        return reply["result"]

    assert rpc("initialize", {"protocolVersion": "2025-06-18"})["protocolVersion"] == "2025-06-18"
    assert [tool["name"] for tool in rpc("tools/list", {})["tools"]] == ["request_action"]
    action = ACTION_LITERAL
    refused = rpc("tools/call", {"name": "request_action", "arguments": dict(action, job_id="another-job")})
    assert refused["isError"], refused
    for expected in EXPECTED_LITERAL:
        answer = rpc("tools/call", {"name": "request_action", "arguments": action})
        assert not answer["isError"] and expected in answer["content"][0]["text"], answer
    proxy.stdin.close()
    assert proxy.wait(timeout=3) == 0
""".replace("ACTION_LITERAL", repr(ACTION)).replace("EXPECTED_LITERAL", repr(
        ("approved command output", "single execution claim") if go else ("waiting for a human decision",) * 2))
