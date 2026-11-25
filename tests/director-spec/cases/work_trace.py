"""Work trace (tracing CONTRACT 5w): scenarios R, S, T via GET work-trace and tracing/check-trace."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

from harness.client import new_key
from harness.run import case

from cases.client import accept, device, finish, owner
from cases.conversations import created, post
from harness.node2 import land_body

REPO = Path(__file__).resolve().parents[3]
CHECK_TRACE = REPO / "tracing" / "check-trace"

C = "client"


def run_check_trace(ctx, work_trace, expect):
    payload = {"spans": [], "work_traces": [work_trace]}
    with tempfile.NamedTemporaryFile("w", suffix=".json", dir=str(REPO)) as tmp:
        json.dump(payload, tmp)
        tmp.flush()
        proc = subprocess.run(
            [sys.executable, str(CHECK_TRACE), tmp.name],
            capture_output=True,
            cwd=str(REPO),
        )
    ctx.expect(proc.returncode in (0, 1), "check-trace ran", proc.stderr.decode() or proc.stdout.decode())
    result = json.loads(proc.stdout.decode())
    ctx.expect(result["result"] == expect["result"], "check-trace result", result)
    if "missing" in expect:
        ctx.expect(result.get("missing") == expect["missing"], "check-trace missing", result)
    return result


def mark_owner(ctx, o, intent_id, turn_id, mark):
    m = o.call(
        "PUT",
        "intents/%s/owner-messages/%s/mark" % (intent_id, turn_id),
        body={"mark": mark},
    )
    ctx.expect(m.status == 200, "owner message mark", m)


def scenario_r_lane(ctx, land_second=True, mark_owner_messages=True):
    """Conversation turn starts a task; job1 finishes stuck; optional land on retry job2."""
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    turn = post(ctx, o, conv, "Fix the regression in the lane.")
    o.upload_bundle()
    body = o.intent(n.name)
    body["origin"] = {
        "kind": "conversation_turn",
        "conversation_id": conv["conversation_id"],
        "turn_id": turn["turn_id"],
    }
    a = o.call("POST", "intents", body=body, key=new_key("wt-intent"))
    ctx.expect(a.status == 201, "task from conversation turn", a)
    iid = a.json["intent"]["intent_id"]
    job1 = a.json["jobs"][0]["job_id"]
    if mark_owner_messages:
        mark_owner(ctx, o, iid, turn["turn_id"], "instruction")

    accept(ctx, n, job1)
    finish(ctx, n, job1, run_id="%s-j1" % ctx.serial_tag())

    att = o.call(
        "PUT",
        "conversations/%s/attention" % conv["conversation_id"],
        body={"question": "First attempt is stuck — retry?", "job_id": job1},
    )
    ctx.expect(att.status == 200, "raise needs-you", att)
    retry_turn = post(ctx, o, conv, "Retry with a fresh attempt.", key=new_key("wt-clear"))
    if mark_owner_messages:
        mark_owner(ctx, o, iid, retry_turn["turn_id"], "instruction")

    path = "intents/%s/jobs" % iid
    body2 = o.intent(n.name)
    body2["meta"] = {"retry_of": job1}
    j2 = o.call("POST", path, body=body2, key=new_key("wt-retry"))
    ctx.expect(j2.status == 201, "retry job", j2)
    job2 = j2.json["job_id"]

    accept(ctx, n, job2)
    run2 = finish(ctx, n, job2, run_id="%s-j2" % ctx.serial_tag())["envelope"]["run_id"]
    if land_second:
        la = n.land(run2, land_body("landed", commit="cafebabe"), new_key("wt-land"))
        ctx.expect(la.status == 201, "land second job", la)

    wt = o.call("GET", "intents/%s/work-trace" % iid)
    ctx.expect(wt.status == 200 and "work_trace" in wt.json, "GET work-trace", wt)
    return wt.json["work_trace"]


@case(C, "work-trace", "scenario-r-complete")
def scenario_r(ctx):
    """R: turn starts a task; first job stuck; retry passes and lands — work trace complete."""
    trace = scenario_r_lane(ctx, land_second=True)
    ctx.expect(trace["origin"]["kind"] == "conversation_turn", "origin kind", trace["origin"])
    ctx.expect(len(trace["jobs"]) == 2, "two jobs", trace["jobs"])
    ctx.expect(trace["jobs"][0].get("decision_status") == "none_yet", "job1 none_yet", trace["jobs"][0])
    ctx.expect(trace["jobs"][1].get("reason") == {"kind": "retry"}, "job2 retry", trace["jobs"][1])
    land = [d for d in trace["jobs"][1].get("decisions", []) if d["kind"] == "land"]
    ctx.expect(len(land) == 1, "land decision", trace["jobs"][1])
    ctx.expect(trace.get("raised_needs_you_ids"), "needs-you inventory", trace)
    ctx.expect(any(n.get("status") == "cleared" for n in trace.get("needs_you", [])), "needs-you cleared",
               trace.get("needs_you"))
    run_check_trace(ctx, trace, {"result": "complete", "missing": []})


@case(C, "work-trace", "scenario-s-incomplete")
def scenario_s(ctx):
    """S: like R but the second finished job has no decision — check-trace incomplete names that job."""
    trace = scenario_r_lane(ctx, land_second=False)
    j2 = trace["jobs"][1]
    ctx.expect("decision_status" not in j2 and not j2.get("decisions"), "job2 lacks decision", j2)
    label = "5w: task %s job %s decision" % (trace["task_id"], trace["jobs"][1]["job_id"])
    run_check_trace(ctx, trace, {"result": "incomplete", "missing": [label]})


@case(C, "work-trace", "scenario-t-intervention")
def scenario_t(ctx):
    """T: owner asks after the result — message marked intervention; work trace complete."""
    trace = scenario_r_lane(ctx, land_second=True)
    o = owner(ctx)
    conv_id = trace["origin"]["conversation_id"]
    turn = post(ctx, o, {"conversation_id": conv_id}, "Did it finish?", key=new_key("wt-intervene"))
    iid = trace["task_id"]
    path = "intents/%s/owner-messages/%s/mark" % (iid, turn["turn_id"])
    mark = o.call("PUT", path, body={"mark": "answer"})
    ctx.expect(mark.status == 200, "set an initial mark with the local credential", mark)
    mark = device(ctx).call("PUT", path, body={"mark": "intervention"})
    ctx.expect(mark.status == 200, "a paired owner can override the local mark", mark)
    trace = o.call("GET", "intents/%s/work-trace" % iid).json["work_trace"]
    ctx.expect(turn["turn_id"] in trace.get("owner_message_ids", []), "message inventoried", trace)
    ctx.expect(any(m.get("mark") == "intervention" for m in trace.get("owner_messages", [])),
               "intervention on trace", trace.get("owner_messages"))
    run_check_trace(ctx, trace, {"result": "complete", "missing": []})

@case(C, "work-trace", "unmarked-owner-message-incomplete")
def unmarked_owner_message(ctx):
    """An unmarked owner message on the task thread makes the work trace incomplete."""
    trace = scenario_r_lane(ctx, land_second=True, mark_owner_messages=False)
    unmarked = trace.get("owner_message_ids", [])
    ctx.expect(unmarked, "owner messages inventoried", trace)
    missing = ["5w: task %s owner message %s mark" % (trace["task_id"], mid) for mid in unmarked]
    run_check_trace(ctx, trace, {"result": "incomplete", "missing": missing})


@case(C, "work-trace", "unknown-retry-of-refused")
def _(ctx):
    """A follow-on job whose meta.retry_of names no job is refused as invalid, not a server error."""
    from cases.client import refused, submitted
    n, o = ctx.node2(), owner(ctx)
    first = submitted(ctx, o, n)
    body = o.intent(n.name)
    body["meta"] = {"retry_of": "job-that-does-not-exist"}
    answer = o.call("POST", "intents/%s/jobs" % first["intent"]["intent_id"], body=body, key=new_key("wt-retry"))
    refused(ctx, answer, 422, "invalid_request", "unknown retry_of")
    ctx.expect("retry_of" in (answer.json or {}).get("detail", ""), "the refusal names retry_of", answer)
