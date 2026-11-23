"""Reuse the Director contract rig and deliver the real finalizer's outbox."""
import json
import os
import sys
from contextlib import contextmanager

from estate import REPO

sys.path.insert(0, str(REPO / "tests/director-spec"))
from cases.client import accept, owner, submitted
from cases.conversations import created, post
from cases.work_trace import mark_owner
from harness.client import new_key
from harness.node2 import land_body
from harness.run import Ctx
from harness.targets import RustTarget
from harness.wire import sha


@contextmanager
def director(scratch, label):
    binary = os.environ.get("DIRECTOR_BIN") or REPO / "target/release/cs-director"
    target = RustTarget(scratch, binary)
    try:
        yield Ctx("trace-" + label.lower(), target=target)
    finally:
        target.close()


def start_job(ctx):
    node = ctx.node2()
    response = submitted(ctx, owner(ctx), node)
    job_id = response["jobs"][0]["job_id"]
    accept(ctx, node, job_id)
    return node, job_id


def report_run(ctx, node, job_id, run_dir, *, expected_usage=None):
    run_id = run_dir.name
    state = run_dir.parent.parent
    envelope = json.loads((state / "outbox" / (run_id + ".json")).read_text())
    row_line = next(line for line in (state / "ledger.jsonl").read_text().splitlines()
                    if sha(line) == envelope["row_sha256"])
    for entry in envelope["files"]:
        data = (run_dir / entry["path"]).read_bytes()
        ctx.expect(node.call("PUT", "objects/" + entry["sha256"], raw=data).status == 200,
                   "upload finalized evidence", entry["path"])
    envelope["job_id"] = job_id
    send(ctx, node, envelope, row_line)
    finish_job(ctx, node, job_id, state="done", run_id=run_id)
    client = owner(ctx)
    response = client.call("GET", "jobs/" + job_id)
    ctx.expect(response.status == 200, "read Director job", response)
    job = response.json
    ctx.expect(job["run_ids"] == [run_id], "Director run inventory", job)
    budget = job["budget"]
    ctx.expect(budget["state"] == "settled" and budget["charged_usd"] is not None,
               "settled Director job charge", budget)
    response = client.call("GET", "runs/" + run_id)
    ctx.expect(response.status == 200, "read Director run", response)
    run = response.json
    for key, expected in (expected_usage or {}).items():
        ctx.expect(run["usage"][key] == expected, "Director run usage " + key, run["usage"])
    return {"run_id": run["run_id"], "job_id": run["job_id"]}, budget["charged_usd"]


def finish_job(ctx, node, job_id, **fields):
    terminal = json.dumps({"director_job": job_id, **fields})
    send(ctx, node, {
        "envelope_version": 1, "event_id": "job_terminal:" + job_id, "job_id": job_id,
        "files": [], "envelope_sha256": sha(b""), "row_sha256": sha(terminal),
    }, terminal)


def start_work(ctx):
    node, client = ctx.node2(), owner(ctx)
    conversation = created(ctx, client, node)
    turn = post(ctx, client, conversation, "Fix the regression.")
    client.upload_bundle()
    origin = {"kind": "conversation_turn", "conversation_id": conversation["conversation_id"],
              "turn_id": turn["turn_id"]}
    first = client.call("POST", "intents", body={**client.intent(node.name), "origin": origin},
                        key=new_key("trace-intent"))
    ctx.expect(first.status == 201, "start task from owner turn", first)
    task = first.json["intent"]["intent_id"]
    first_job = first.json["jobs"][0]["job_id"]
    mark_owner(ctx, client, task, turn["turn_id"], "instruction")
    accept(ctx, node, first_job)
    # A stuck dispatch may finish without a run. Report its node terminal receipt.
    finish_job(ctx, node, first_job, state="error", stop="stuck:repeat", error="stuck:repeat")
    first = client.call("GET", "jobs/" + first_job)
    ctx.expect(first.status == 200 and first.json["failure"]["code"] == "stuck",
               "first job finishes stuck", first)
    retry = client.call("POST", f"intents/{task}/jobs",
                        body={**client.intent(node.name), "meta": {"retry_of": first_job}},
                        key=new_key("trace-retry"))
    ctx.expect(retry.status == 201, "retry the stuck job", retry)
    job_id = retry.json["job_id"]
    accept(ctx, node, job_id)
    return node, client, task, job_id, origin


def work_trace(ctx, node, client, task, run_id, origin, intervene):
    land = node.land(run_id, land_body("landed"), new_key("trace-land"))
    ctx.expect(land.status == 201, "land the passing retry", land)
    if intervene:
        turn = post(ctx, client, {"conversation_id": origin["conversation_id"]}, "Did it finish?")
        mark_owner(ctx, client, task, turn["turn_id"], "intervention")
    reply = client.call("GET", f"intents/{task}/work-trace")
    ctx.expect(reply.status == 200, "read Director work trace", reply)
    trace = reply.json["work_trace"]
    ctx.expect(trace["origin"] == origin, "originating turn", trace)
    first, second = trace["jobs"]
    ctx.expect(first["finished"] and second["finished"], "both jobs finished", trace)
    ctx.expect(second["reason"] == {"kind": "retry"} and second["run_ids"] == [run_id],
               "retry job and run", second)
    ctx.expect([d["kind"] for d in second["decisions"]] == ["land"], "retry land decision", second)
    if intervene:
        ctx.expect(any(m["message_id"] == turn["turn_id"] and m["mark"] == "intervention"
                       for m in trace["owner_messages"]), "mark the follow-up intervention", trace)
    return trace


def send(ctx, node, envelope, row_line):
    response = node.call("POST", "envelopes", body={"envelope": envelope, "row_line": row_line})
    ctx.expect((response.json or {}).get("status") == "acked", envelope["event_id"], response)
