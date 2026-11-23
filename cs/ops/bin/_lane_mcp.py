"""Job-bound action approval MCP server and CLI staging."""
import hmac
import io
import json
import os
import re
import subprocess
import sys
from contextlib import redirect_stdout
from pathlib import Path

from _action_approval import director_client, request_action
from _estate_io import native_sandbox_binary
from _lead_mcp import HostMCPBridge

ROOT = Path(__file__).resolve().parents[2]
DISCOVERY = "Use the request_action MCP tool for actions your sandbox blocks, such as package downloads or writes outside the workspace."
UNAVAILABLE = "Action approval could not be completed. Continue without the action if possible."
PROTOCOLS = {"2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"}


def consume_binding(run_dir, job, token):
    if not isinstance(token, str) or not re.fullmatch(r"[0-9a-f]{64}", token):
        return False
    path = run_dir / "action-mcp-binding"
    try:
        record = json.loads(path.read_text())
        if record["job"] == job and hmac.compare_digest(record["token"], token):
            path.unlink()
            return True
    except (OSError, ValueError, KeyError, TypeError):
        pass
    return False


def action_prefix():
    sandbox = native_sandbox_binary(ROOT)
    if not sandbox.is_file():
        return None
    try:
        director_client(ROOT)
    except (OSError, ValueError, KeyError, TypeError):
        return None
    return [str(sandbox), "--policy", str(ROOT / "policy/sandbox.json"), "--"]


def start(workspace, tmp, run_dir, model, job, prefix):
    env = {key: os.environ[key] for key in ("HOME", "LANG", "CS_CONFIG_ROOT")
           if key in os.environ}
    env.update(TMPDIR=str(run_dir), PYTHONDONTWRITEBYTECODE="1")
    command = {"command": sys.executable, "args": [str(Path(__file__).resolve()),
               str(run_dir), model, job, json.dumps(prefix)]}
    return HostMCPBridge({"mcpServers": {"contextstack": command}}, workspace,
                         env=env, secret_dir=tmp, loopback=True)


def stage(binary, args, env, config):
    server = config["mcpServers"]["contextstack"]
    if binary == "codex":
        # --ignore-user-config suppresses config.toml, so inject the trusted server as overrides.
        settings = ["-c", "mcp_servers.contextstack.command=" + json.dumps(server["command"]),
                    "-c", "mcp_servers.contextstack.args=" + json.dumps(server["args"]),
                    "-c", "mcp_servers.contextstack.tool_timeout_sec=180"]
        return [args[0], *settings, *args[1:]]
    if binary == "cursor-agent":
        path = Path(env["CURSOR_CONFIG_DIR"]) / "mcp.json"
        path.write_text(json.dumps(config) + "\n")
        return args
    value = json.dumps(config)
    allowed = ["--allowedTools", "mcp__contextstack__request_action"]
    if "--mcp-config" in args:
        args[args.index("--mcp-config") + 1] = value
        return [*allowed, *args]
    return [*allowed, "--strict-mcp-config", "--mcp-config", value, *args]


def response(message, tool, run_action):
    method, params = message.get("method"), message.get("params", {})
    if method == "initialize":
        version = params.get("protocolVersion")
        return {"protocolVersion": version if version in PROTOCOLS else "2024-11-05",
                "serverInfo": {"name": "contextstack", "version": "1"},
                "capabilities": {"tools": {}}}
    if method == "ping":
        return {}
    if method == "tools/list":
        return {"tools": [tool]}
    if method == "tools/call" and params.get("name") == tool["name"]:
        # The caller supplies the command alone. Job, run, model and workspace are bound by the host.
        arguments = params.get("arguments")
        output = io.StringIO()
        if not isinstance(arguments, dict) or set(arguments) - set(tool["inputSchema"]["properties"]):
            code = 1
            output.write("Supply argv, needs, reason, and optional write_paths only.")
        else:
            try:
                with redirect_stdout(output):
                    code = run_action(arguments)
            except (KeyError, ValueError, OSError, RuntimeError, subprocess.TimeoutExpired):
                output.write(UNAVAILABLE)
                code = 1
        return {"content": [{"type": "text", "text": output.getvalue().rstrip()}],
                "isError": code != 0}
    raise LookupError


def serve(input_stream, output_stream, run_action):
    descriptor = json.loads((ROOT / "tools/bin/request_action.json").read_text())
    tool = {key: descriptor[key] for key in ("description", "inputSchema")}
    tool["name"] = "request_action"
    while line := input_stream.readline(1 << 20):
        if not line.endswith("\n"):
            break
        reply = {"jsonrpc": "2.0", "id": None}
        try:
            message = json.loads(line)
        except ValueError:
            reply["error"] = {"code": -32700, "message": "Parse error"}
        else:
            if isinstance(message, dict) and "id" not in message and "method" in message:
                continue
            try:
                if not isinstance(message, dict):
                    raise TypeError
                reply["id"] = message.get("id")
                reply["result"] = response(message, tool, run_action)
            except LookupError:
                reply["error"] = {"code": -32601, "message": "Method not found"}
            except (AttributeError, TypeError):
                reply["error"] = {"code": -32602, "message": "Invalid parameters"}
        print(json.dumps(reply), file=output_stream, flush=True)


if __name__ == "__main__":
    run, model, job, prefix = sys.argv[1:]
    workspace = Path.cwd()
    prefix = json.loads(prefix)
    serve(sys.stdin, sys.stdout, lambda action: request_action(
        ROOT, Path(run), model, "", action, job=job, workspace=workspace, prefix=prefix))
