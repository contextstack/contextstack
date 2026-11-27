#!/usr/bin/env python3
"""Missing run records retain job diagnostics; other Director faults stay errors."""
import importlib.util
import sys
from pathlib import Path

sys.dont_write_bytecode = True
kit = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location("cs_mcp", kit / "scripts/cs_mcp.py")
mcp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcp)


class Director(mcp.Director):
    def __init__(self, status=404, run_ids=False):
        self.status = status
        self.job = {"job_id": "failed", "state": "error", "error": "executor_exit_1",
                    "run_ids" if run_ids else "run_id": ["missing"] if run_ids else "missing"}

    def request(self, method, path, **kwargs):
        assert method == "GET"
        if path == "jobs/failed":
            return 200, {"job": self.job}
        assert path == "runs/missing", path
        return self.status, {"error": "fixture response"}


for run_ids in (False, True):
    out = mcp.Tools(Director(run_ids=run_ids), "fixture").task_output("failed")
    assert out["state"] == "error" and out["error"] == "executor_exit_1", out
    assert out["run_id"] == "missing" and out["output"] is None, out
    assert "missing" in out["note"], out

for status in (401, 500):
    try:
        mcp.Tools(Director(status=status), "fixture").task_output("failed")
    except mcp.Refused as exc:
        assert str(status) in str(exc), exc
    else:
        raise AssertionError("Director %s must remain an error" % status)

print("ok   task_output: missing run returns job state/error; other failures propagate")


class InspectionDirector(mcp.Director):
    def __init__(self):
        self.calls = []
        self.status = 200

    def request(self, method, path, **kwargs):
        self.calls.append((method, path, kwargs))
        if self.status != 200:
            return self.status, {"code": "owner_only"}
        if path.endswith("check.json"):
            return 200, {"nested": [{"output": "Bearer mock-fixture-token"}]}
        if path.endswith("candidate.diff"):
            return 200, (b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n"
                         b"-old\n+Bearer mock-fixture-token\n"
                         b"diff --git a/mode only b/mode only\nold mode 100644\nnew mode 100755\n"
                         b"diff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new\n")
        if path == "me":
            return 200, {"conversation_id": "lead-conversation"}
        if path == "conversations/lead-conversation/peer-messages":
            if method == "GET":
                return 200, {"peers": self.peers}
            return 201, {"turn_id": "peer-wake"}
        return 200, {"message": "ok"}


director = InspectionDirector()
tools = mcp.Tools(director, "fixture")


def call(tool, **args):
    result = mcp.handle(tools, {"method": "tools/call", "params": {"name": tool, "arguments": args}})
    return mcp.json.loads(result["content"][0]["text"]), result["isError"]


def collect(name, **args):
    offset, chunks = 0, []
    while offset is not None:
        result, error = call(name, **args, offset=offset, limit=7)
        assert not error and result["offset"] == offset and len(result["content"]) <= 7, result
        chunks.append(result["content"])
        offset = result["next_offset"]
    text = "".join(chunks)
    assert len(text) == result["total_chars"] and "Bearer <redacted>" in text, result
    assert "mock-fixture-token" not in text, text
    return result


procedures, error = call("procedure")
assert not error and {"heartbeat", "owner-decisions", "release", "review"} <= set(procedures["procedures"])
for name in ("heartbeat", "owner-decisions", "release", "review"):
    result, error = call("procedure", action="read", name=name)
    assert not error and result["content"].startswith("# "), result
for name in ("../review", "review.md", "%2e%2e", "/review", "Review", "unknown", ""):
    assert call("procedure", action="read", name=name)[1], name

def listed_tools():
    return {tool["name"]: tool for tool in mcp.handle(tools, {"method": "tools/list"})["tools"]}

director.peers = []
assert "message_conversation" not in listed_tools()
director.peers = ["client-session"]
assert listed_tools()["message_conversation"]["inputSchema"]["properties"]["to"]["enum"] == director.peers
assert call("message_conversation", to="client-session", text="brief", idempotency_key="brief-1") == (
    {"turn_id": "peer-wake"}, False)
assert director.calls[-1] == ("POST", "conversations/lead-conversation/peer-messages", {
    "body": {"to": "client-session", "text": "brief"}, "key": "brief-1"})

collect("run_file", run_id="20261126-150000.123456789-visible-t1", path="check.json")
result = collect("lane_diff", run_id="20261126-150000.123456789-visible-t1")
assert list(result)[0] == "files" and result["files"] == ["a.txt", "mode only", "new", "old"], result
before = len(director.calls)
for args in ({"path": "../check.json"}, {"path": "evidence/%2e%2e/cli.log"}, {"run_id": "../visible"},
             {"offset": -1}, {"limit": 0}, {"limit": 50001}):
    assert call("run_file", **({"run_id": "visible", "path": "cli.log"} | args))[1], args
assert len(director.calls) == before

assert call("steer_task", intent_id="running", message="Use the new acceptance criterion.") == ({"message": "ok"}, False)
assert director.calls[-1] == ("PUT", "intents/running/owner-messages",
                              {"body": {"message": "Use the new acceptance criterion."}})
assert call("steer_task", intent_id="../other", message="x")[1]
assert call("steer_task", intent_id="running", message=" ")[1]
assert call("heartbeat", project_id="project", action="set")[1]
assert not call("heartbeat", project_id="project", action="set", interval_seconds=60)[1]
assert director.calls[-1] == ("PUT", "projects/project/heartbeat",
                              {"body": {"interval_seconds": 60, "conversation_id": "lead-conversation"}})
assert not call("heartbeat", project_id="project", action="set", interval_seconds=None)[1]
assert director.calls[-1] == ("PUT", "projects/project/heartbeat", {"body": {"interval_seconds": None}})
assert not call("heartbeat", project_id="project")[1]
assert director.calls[-1][:2] == ("GET", "projects/project/heartbeat")

director.status = 403
for name, args in (("run_file", {"run_id": "hidden", "path": "cli.log"}), ("lane_diff", {"run_id": "hidden"}),
                   ("steer_task", {"intent_id": "hidden", "message": "x"}), ("heartbeat", {"project_id": "hidden"})):
    result, error = call(name, **args)
    assert error and "403" in result["error"] and "content" not in result, result
director.status = 404
result, error = call("steer_task", intent_id="running", message="x")
assert error and "404" in result["error"], result
print("ok   lead MCP procedures, paged and redacted evidence, scope refusals, steering and heartbeat requests")
