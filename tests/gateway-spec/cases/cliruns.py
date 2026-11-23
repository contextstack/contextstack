"""Coding-agent work orders the gateway runs (Director.Plan step 5): cancel by
execution id (`POST /v1/agent/cli.run/{id}/cancel`) and live output
(`stream: true`). The fake executor stands in for each agent CLI, printing
canned stream-json / JSONL lines and sleeping so a cancel lands mid-run. No
real CLI, no model call."""
import json
import os
import sys
import time
from pathlib import Path

from harness.run import case, eq, sub
from cases.admission import POOL, idle
from cases.workorders import alive, order, wait_file

CHUNK = "agent.gateway.cli_run.chunk.v1"
DEP = "cli-agents"
# Each agent's own streaming output: the text it carries, interleaved with events that carry none.
CANNED = {
    "claude-code": [
        '{"type":"system","subtype":"init","session_id":"s"}',
        '{"type":"assistant","message":{"content":[{"type":"text","text":"Hel"}]}}',
        '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Read","input":{}}]}}',
        '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"x"}]}}',
        '{"type":"assistant","message":{"content":[{"type":"text","text":"lo."}]}}',
        '{"type":"result","subtype":"success","result":"Hello.","total_cost_usd":0.01}',
    ],
    "codex": [
        '{"type":"thread.started","thread_id":"t"}',
        '{"type":"item.completed","item":{"id":"i0","type":"reasoning","text":"thinking"}}',
        '{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Done"}}',
        '{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":" twice."}}',
        '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}',
    ],
    "gemini-cli": [
        '{"type":"init","session_id":"s","model":"gemini"}',
        '{"type":"message","role":"user","content":"q"}',
        '{"type":"message","role":"assistant","content":"Gem","delta":true}',
        '{"type":"message","role":"assistant","content":"ini.","delta":true}',
        '{"type":"result","status":"success","stats":{}}',
    ],
    "cursor-agent": [
        '{"type":"system","subtype":"init","apiKeySource":"login","session_id":"s","model":"Auto"}',
        '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"q"}]},"session_id":"s"}',
        '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Cur"}]},"session_id":"s"}',
        '{"type":"tool_call","subtype":"started","call_id":"c","tool_call":{"readToolCall":{"args":{}}}}',
        '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"sor."}]},"session_id":"s"}',
        '{"type":"result","subtype":"success","is_error":false,"result":"Cursor.","session_id":"s"}',
    ],
    "antigravity": [
        '{"event":"init","conversation_id":"c","init":{"cwd":"."}}',
        '{"event":"step_update","conversation_id":"c","step_type":"user_input","state":"DONE"}',
        '{"event":"step_update","conversation_id":"c","step_type":"agent_response","state":"ACTIVE","text_delta":"Anti"}',
        '{"event":"step_update","conversation_id":"c","step_type":"tool","state":"DONE","tool_name":"run_command"}',
        '{"event":"step_update","conversation_id":"c","step_type":"agent_response","state":"DONE","text_delta":"gravity."}',
        '{"event":"result","result":{"status":"SUCCESS","response":"Antigravity."}}',
    ],
}
# cursor-* model templates share cursor-agent's stream-json shape (dialect follows the binary).
for _name in ("cursor-sol", "cursor-codex", "cursor-composer", "cursor-grok", "cursor-sonnet"):
    CANNED[_name] = CANNED["cursor-agent"]
CANNED["antigravity-hydrogen"] = CANNED["antigravity"]
TEXT = {"claude-code": "Hello.", "codex": "Done twice.", "gemini-cli": "Gemini.", "cursor-agent": "Cursor.",
        "antigravity": "Antigravity.", "antigravity-hydrogen": "Antigravity.",
        "cursor-sol": "Cursor.", "cursor-codex": "Cursor.", "cursor-composer": "Cursor.",
        "cursor-grok": "Cursor.", "cursor-sonnet": "Cursor."}


def cli_run(gw, work_order, stream=False, execution_id=None, caller=None, **kw):
    body = {"work_order": work_order}
    if stream:
        body["stream"] = True
    if execution_id:
        body["execution_id"] = execution_id
    headers = {"X-ContextStack-Caller": caller} if caller else None
    return gw.post("/v1/agent/cli.run", body, headers=headers, **kw)


def cancel(gw, execution_id, caller=None, reason=None, **kw):
    headers = {"X-ContextStack-Caller": caller} if caller else None
    body = {"reason": reason} if reason is not None else None
    return gw.post("/v1/agent/cli.run/%s/cancel" % execution_id, body, headers=headers, **kw)


def canned(executor, pause=0.0, **fake):
    fake["lines"] = [[line, pause] for line in CANNED[executor]]
    return order(fake, executor=executor)


def deltas(frames):
    return "".join(f["content"] for f in frames if f["kind"] == "delta")


def started(t, name):
    """A sleeping run's pidfile path and work order: two lines out, then 30 s asleep, with a grandchild."""
    pidfile = str(t.gw.dir / (name + "-pids.json"))
    lines = [[CANNED["claude-code"][1], 0], [CANNED["claude-code"][4], 0]]
    return pidfile, order({"lines": lines, "sleep_s": 30, "grandchild": True, "pidfile": pidfile}, executor="claude-code")


@case("cliruns", gw="agents")
def clirun_stream_equals_unary(t):
    """For claude, codex, gemini, cursor-agent (and cursor-* model templates), and antigravity executors: the stream's deltas are the agent's text, in order, and its one stop chunk's result is exactly the unary reply; every chunk names the execution id."""
    for executor in CANNED:
        wo = canned(executor)
        unary = cli_run(t.gw, wo)
        eq(unary.status, 200, executor + " unary status")
        r = cli_run(t.gw, wo, stream=True, execution_id="spec-eq-" + executor)
        eq((r.status, r.headers.get("content-type")), (200, "text/event-stream"), executor + " stream status")
        eq(r.headers.get("x-contextstack-execution-id"), "spec-eq-" + executor, executor + " id header")
        frames = r.sse()
        eq([f["kind"] for f in frames][-1], "stop", executor + " terminal")
        eq(sum(f["kind"] == "stop" for f in frames), 1, executor + " one stop")
        eq({(f["schema_version"], f["execution_id"]) for f in frames}, {(CHUNK, "spec-eq-" + executor)}, executor + " family and id")
        eq([f["index"] for f in frames], list(range(len(frames))), executor + " indexes")
        eq(deltas(frames), TEXT[executor], executor + " delta text")
        eq(frames[-1]["result"], unary.json, executor + " stop result == unary reply")
    idle(t.gw, DEP)


@case("cliruns", gw="agents")
def clirun_stream_is_live(t):
    """Deltas arrive while the agent is still running, not at the end."""
    pidfile = str(t.gw.dir / "live-pids.json")
    lines = [[CANNED["claude-code"][1], 1.5], [CANNED["claude-code"][4], 0]]
    c = t.gw.conn(15)
    try:
        c.request("POST", "/v1/agent/cli.run", body=json.dumps({"stream": True, "work_order": order(
            {"lines": lines, "pidfile": pidfile}, executor="claude-code")}),
            headers={"Content-Type": "application/json", "Authorization": "Bearer " + t.gw.token})
        r = c.getresponse()
        first = None
        while first is None:
            line = r.readline()
            if not line:
                break
            if line.startswith(b"data: "):
                first = json.loads(line[6:])
        pids = wait_file(pidfile)
        eq((first["kind"], first["content"]), ("delta", "Hel"), "first chunk")
        eq(alive(pids["pid"]), True, "agent still running when the first delta arrived")
        rest = r.read().decode()
    finally:
        c.close()
    eq('"kind":"stop"' in rest, True, "stream ends with stop")
    idle(t.gw, DEP)


@case("cliruns", gw="agents")
def clirun_stream_unsupported(t):
    """An executor with no streaming mode (not a claude, codex or gemini agent): stream:true is one SSE error chunk, fatal_session gateway.streaming_unsupported, before admission or any child; unary still serves it."""
    pidfile = str(t.gw.dir / "nostream-pids.json")
    r = cli_run(t.gw, order({"pidfile": pidfile, "stdout": "plain"}), stream=True, execution_id="spec-nostream")
    frames = r.sse()
    eq(len(frames), 1, "one chunk")
    sub(frames[0], {"schema_version": CHUNK, "execution_id": "spec-nostream", "kind": "error"}, "chunk")
    sub(frames[0]["error"], {"class": "fatal_session", "code": "gateway.streaming_unsupported"}, "error")
    time.sleep(0.3)
    eq(os.path.exists(pidfile), False, "no child started")
    eq(cancel(t.gw, "spec-nostream").status, 404, "never registered")
    eq(cli_run(t.gw, order({"stdout": "plain"})).json, {"stdout": "plain", "stderr": "", "exit_code": 0}, "unary serves")


@case("cliruns", gw="agents")
def clirun_cancel_mid_run(t):
    """Cancel while the agent runs: the reply comes after the process group is gone and the permit released; the original call ends 200 with outcome cancelled and the output written so far; a second cancel is already_finished (cancelled)."""
    pidfile, wo = started(t, "cancel")
    call = POOL.submit(cli_run, t.gw, wo, False, "spec-cancel-1")
    pids = wait_file(pidfile)
    eq(alive(pids["pid"]) and alive(pids["grandchild"]), True, "agent and descendant running")
    t.gw.wait(lambda: t.gw.admission()["deployments"][DEP]["active"] == 1, what="permit held")
    r = cancel(t.gw, "spec-cancel-1")
    eq((r.status, r.json), (200, {"execution_id": "spec-cancel-1", "cancelled": True, "result": "cancelled"}), "cancel reply")
    eq((alive(pids["pid"]), alive(pids["grandchild"])), (False, False), "process group gone when cancel answers")
    eq(t.gw.admission()["deployments"][DEP], {"active": 0, "queued": 0}, "permit released when cancel answers")
    orig = call.result(10)
    eq(orig.status, 200, "original call status")
    eq(orig.headers.get("x-contextstack-execution-id"), "spec-cancel-1", "id header")
    b = orig.json
    eq((b["outcome"], b["exit_code"]), ("cancelled", -1), "cancelled outcome")
    eq(b["stdout"].splitlines(), [CANNED["claude-code"][1], CANNED["claude-code"][4]], "output so far")
    again = cancel(t.gw, "spec-cancel-1")
    eq((again.status, again.json), (200, {"execution_id": "spec-cancel-1", "cancelled": False, "result": "already_finished", "outcome": "cancelled"}), "second cancel")


@case("cliruns", gw="agents")
def clirun_cancel_mid_stream(t):
    """Cancel a streamed run: the deltas so far, then a stop chunk whose result is the cancelled outcome (not an error chunk)."""
    pidfile, wo = started(t, "cancel-stream")
    call = POOL.submit(cli_run, t.gw, wo, True, "spec-cancel-2")
    pids = wait_file(pidfile)
    time.sleep(0.2)
    eq(cancel(t.gw, "spec-cancel-2").json["result"], "cancelled", "cancel reply")
    frames = call.result(10).sse()
    eq(deltas(frames), "Hello.", "deltas before the cancel")
    eq(frames[-1]["kind"], "stop", "terminal is stop")
    sub(frames[-1]["result"], {"outcome": "cancelled", "exit_code": -1}, "cancelled result")
    eq(alive(pids["grandchild"]), False, "descendant gone")
    idle(t.gw, DEP)


@case("cliruns", gw="agents")
def clirun_cancel_after_finish(t):
    """Cancelling a run that already ended is idempotent: already_finished, with how it ended."""
    eq(cli_run(t.gw, canned("codex"), execution_id="spec-done-1").status, 200, "run")
    r = cancel(t.gw, "spec-done-1")
    eq((r.status, r.json), (200, {"execution_id": "spec-done-1", "cancelled": False, "result": "already_finished", "outcome": "completed"}), "cancel after finish")
    eq(cancel(t.gw, "spec-done-1").json, r.json, "again: same answer")


@case("cliruns", gw="agents")
def clirun_cancel_unknown(t):
    """An unknown or malformed execution id is 404 unknown_execution; the cancel route answers only POST."""
    for id_ in ("no-such-run", "bad%20id"):
        r = cancel(t.gw, id_)
        eq((r.status, r.json), (404, {"error": "unknown execution_id", "code": "unknown_execution"}), id_)
    r = t.gw.get("/v1/agent/cli.run/no-such-run/cancel")
    eq((r.status, r.headers.get("allow")), (405, "POST"), "GET")


@case("cliruns", gw="agents")
def clirun_cancel_unauthorized(t):
    """Only the starter may cancel: no bearer is 401; a run started with X-ContextStack-Caller refuses another or no caller name (403 not_owner) and keeps running; its own caller cancels it."""
    pidfile, wo = started(t, "owner")
    call = POOL.submit(cli_run, t.gw, wo, False, "spec-owned", "node-a")
    pids = wait_file(pidfile)
    eq(cancel(t.gw, "spec-owned", "node-a", auth=False).status, 401, "no bearer")
    for who in ("node-b", None):
        r = cancel(t.gw, "spec-owned", who)
        eq((r.status, r.json.get("code")), (403, "not_owner"), "caller %r" % who)
    eq(alive(pids["pid"]), True, "still running after refused cancels")
    eq(cancel(t.gw, "spec-owned", "node-a").json["result"], "cancelled", "owner cancels")
    eq(call.result(10).json["outcome"], "cancelled", "original call")
    eq(cancel(t.gw, "spec-owned", "node-b").status, 403, "finished run: still owner only")


@case("cliruns", gw="agents")
def clirun_execution_id_rules(t):
    """A caller-chosen execution_id is 1-128 of [A-Za-z0-9._:-] (else 400) and unique among running work orders (else 409, no second child); without one the gateway names the run in the response header."""
    r = cli_run(t.gw, order({}), execution_id="has space")
    eq((r.status, r.text), (400, "invalid execution_id\n"), "invalid id")
    pidfile, wo = started(t, "dup")
    call = POOL.submit(cli_run, t.gw, wo, False, "spec-dup")
    wait_file(pidfile)
    r = cli_run(t.gw, order({"stdout": "second"}), execution_id="spec-dup")
    eq((r.status, r.json.get("code")), (409, "execution_in_use"), "duplicate running id")
    cancel(t.gw, "spec-dup")
    call.result(10)
    gen = cli_run(t.gw, order({"stdout": "x"}))
    eq(gen.headers.get("x-contextstack-execution-id", "").startswith("lx-"), True, "generated id header")
    eq(gen.json, {"stdout": "x", "stderr": "", "exit_code": 0}, "unary reply unchanged")


@case("cliruns", gw=None)
def clirun_cancel_socket_refused(t):
    """The cancel route keeps cli.run's peer rule: never over the unix socket (403)."""
    from cases.transport import sock_dir
    path = os.path.join(sock_dir(t), "gw.sock")
    gw = t.gateway("clirun-socket", t.run.config(cli_executor=True), socket_path=path)
    r = gw.post("/v1/agent/cli.run/any/cancel", None, via_socket=True)
    eq((r.status, r.text), (403, "CLI execution is local only\n"), "socket cancel")
    eq(gw.post("/v1/agent/cli.run/any/cancel", None).status, 404, "TCP loopback reaches the registry")


@case("cliruns", gw="agents")
def clirun_cancel_by_job_id(t):
    """The node cancels by the work order's CS_JOB_ID (its dispatch id, the Director job id) with {"reason"}: every running trial of that job is stopped, each call ends cancelled, the permits are released, the reason is logged; then the job id answers cancelled false."""
    runs = []
    for trial in ("1", "2"):
        pidfile, wo = started(t, "job-" + trial)
        body = {"work_order": wo, "environment": {"CS_JOB_ID": "dispatch-42", "CS_TRIAL": trial}}
        runs.append((pidfile, POOL.submit(t.gw.post, "/v1/agent/cli.run", body)))
    pids = [wait_file(p) for p, _ in runs]
    t.gw.wait(lambda: t.gw.admission()["deployments"][DEP]["active"] == 2, what="both trials admitted")
    r = cancel(t.gw, "dispatch-42", reason="director cancelled job")
    eq((r.status, r.json), (200, {"execution_id": "dispatch-42", "cancelled": True, "result": "cancelled"}), "cancel by job id")
    eq([alive(p["pid"]) or alive(p["grandchild"]) for p in pids], [False, False], "both process groups gone")
    eq(t.gw.admission()["deployments"][DEP], {"active": 0, "queued": 0}, "permits released")
    replies = [f.result(10) for _, f in runs]
    eq([x.json["outcome"] for x in replies], ["cancelled", "cancelled"], "both calls cancelled")
    ids = sorted(x.headers.get("x-contextstack-execution-id") for x in replies)
    eq(ids[0] == "dispatch-42" or ids[1] == "dispatch-42", True, "the job id names the first trial (%r)" % ids)
    row = [x for x in t.gw.log_rows("cli_cancel") if x["execution_id"] == "dispatch-42"][-1]
    sub(row, {"reason": "director cancelled job", "result": "cancelled", "status": 200}, "log row")
    again = cancel(t.gw, "dispatch-42", reason="again")
    eq((again.status, again.json["cancelled"], again.json["outcome"]), (200, False, "cancelled"), "after: cancelled false")


def receipt_request(wo, execution_id, stream=False):
    run_id = "20261120-120000.000000000-claude-code-task-t2"
    return {
        "work_order": dict(wo, trial=2), "stream": stream, "execution_id": execution_id,
        "environment": {"CS_JOB_ID": "receipt-job", "CS_TRIAL": "2", "CS_RUN_ID": "spoof",
                        "CS_EXECUTION_ID": "spoof", "CS_CLI_RECEIPT_PATH": "spoof"},
        "receipt": {"run_id": run_id, "trial": 2},
    }


CALLER = {"X-ContextStack-Caller": "receipt-owner"}
ROUTE = "/v1/agent/cli.run"


@case("cliruns", gw="agents")
def clirun_receipt_parity(t):
    """Opt-in unary and live SSE carry the same bound receipt; arbitrary agent JSON remains output."""
    fake = '{"run_id":"spoof","native_stop":"fake","native_exit":0,"usage":{"input_tokens":9000}}'
    release = t.gw.dir / "receipt-release"
    wo = order({"lines": [[CANNED["claude-code"][1], 0], [fake, 0]], "wait_for": str(release),
                "native_exit": 7, "exit": 1}, executor="claude-code")
    body = receipt_request(wo, "receipt-parity", True)
    conn = t.gw.conn(15)
    try:
        conn.request("POST", ROUTE, body=json.dumps(body), headers={
            **CALLER, "Content-Type": "application/json", "Authorization": "Bearer " + t.gw.token,
        })
        response = conn.getresponse()
        eq(response.status, 200, "stream status")
        first = json.loads(response.readline().removeprefix(b"data: "))
        sub(first, {"kind": "delta", "content": "Hel"}, "live delta")
        release.touch()  # The fake child cannot exit successfully until this delta has arrived.
        frames = [json.loads(line[6:]) for line in response.read().decode().splitlines()
                  if line.startswith("data: ")]
    finally:
        conn.close()

    sub(frames[-1], {"kind": "stop", "execution_id": body["execution_id"]}, "terminal frame")
    result = frames[-1]["result"]
    cs = Path(__file__).resolve().parents[3] / "cs"
    sys.path.insert(0, str(cs / "tasks/regressions/_lib"))
    try:
        from receipt_schema import matches_schema
    finally:
        sys.path.pop(0)
    matches_schema(json.loads((cs / "ops/execution-receipt.schema.json").read_text()), result["receipt"])
    sub(result["receipt"], {
        **body["receipt"], "job_id": "receipt-job", "execution_id": body["execution_id"],
        "native": {"stop": "executor_exit_7", "exit": 7}, "usage": {"input_tokens": 1, "output_tokens": 2},
        "workspace": "/fixture/workspace", "finish": {"mode": "task"},
    }, "trusted receipt")
    eq(result["exit_code"], 1, "executor exit separate from native exit")
    eq(fake in result["stdout"], True, "raw JSON remains stdout")
    body["stream"] = False
    eq(t.gw.post(ROUTE, body, headers=CALLER).json, result, "unary equals stop result")
    del body["receipt"]
    plain = t.gw.post(ROUTE, body, headers=CALLER)
    eq(plain.json, {k: v for k, v in result.items() if k != "receipt"}, "legacy reply unchanged")


@case("cliruns", gw="agents")
def clirun_receipt_invalid(t):
    """Binding validation occurs before dispatch, and malformed private metadata cannot become a receipt."""
    for knobs in ({"malformed_receipt": True}, {"receipt_run": "wrong"}, {"receipt_job": "wrong"}, {"omit_receipt": True}):
        body = receipt_request(canned("claude-code", **knobs), "receipt-invalid")
        unary = t.gw.post(ROUTE, body, headers=CALLER)
        eq((unary.status, unary.text), (502, "invalid or missing CLI execution receipt\n"), "invalid metadata")
        body["stream"] = True
        sub(t.gw.post(ROUTE, body, headers=CALLER).sse()[-1], {"kind": "error", "status": 502}, "SSE error")

    pidfile = str(t.gw.dir / "invalid-binding-pids.json")
    body = receipt_request(order({"pidfile": pidfile}), "receipt-bad-binding")
    eq(t.gw.post(ROUTE, body).status, 400, "receipt requires caller")
    body["environment"]["CS_TRIAL"] = "3"
    eq(t.gw.post(ROUTE, body, headers=CALLER).status, 400, "trial differs")
    eq(os.path.exists(pidfile), False, "binding refusal starts no child")


@case("cliruns", gw="agents")
def clirun_receipt_cancel_and_disconnect(t):
    """Cancellation requires the bound caller, and disconnect still tears down the process tree."""
    pidfile, wo = started(t, "receipt-cancel")
    body = receipt_request(wo, "receipt-cancel", True)
    call = POOL.submit(t.gw.post, ROUTE, body, headers=CALLER)
    pids = wait_file(pidfile)
    eq(cancel(t.gw, "receipt-job", "other").status, 403, "job cancel is caller-bound")
    eq(cancel(t.gw, body["execution_id"], "receipt-owner", auth=False).status, 401, "bearer required")
    eq(cancel(t.gw, body["execution_id"], "receipt-owner").json["result"], "cancelled", "owned cancel")
    result = call.result(10).sse()[-1]["result"]
    eq(result["outcome"], "cancelled", "terminal outcome")
    eq(result["receipt"], None, "cancelled before receipt production")
    eq(alive(pids["grandchild"]), False, "cancel reaped descendants")

    pidfile, wo = started(t, "receipt-disconnect")
    body = receipt_request(wo, "receipt-disconnect", True)
    conn = t.gw.conn(15)
    try:
        conn.request("POST", ROUTE, body=json.dumps(body), headers={
            **CALLER, "Content-Type": "application/json", "Authorization": "Bearer " + t.gw.token,
        })
        reply = conn.getresponse()
        eq(reply.status, 200, "connected")
        pids = wait_file(pidfile)
        reply.close()
    finally:
        conn.close()
    t.gw.wait(lambda: not any(alive(pid) for pid in (pids["pid"], pids["grandchild"])),
              what="disconnected execution tree gone")
    idle(t.gw, DEP)
