"""Planner/executor dispatch with fake nodes and the real cs mcp entry point."""

from cases.budgets import end_job, report_run
from cases.client import owner, refused
from cases.decision_log import mcp, post
from cases.work_trace import run_check_trace
from harness import wire
from harness.client import new_key
from harness.run import case


def setup(ctx):
    planner, coder = ctx.node2("planner"), ctx.node2("dgx")
    repos = [{"name": "demo-" + ctx.id, "path": "/fixture/demo", "branch": "main"}]
    ready = planner.status([], platform="Darwin", repos=repos, agent_capabilities={"templates": ["codex"]})
    ctx.expect(ready.status == 200, "planner node reports inventory", ready)
    ready = coder.status([], platform="Linux", repos=repos, agent_capabilities={
        "templates": [], "harness": True, "harness_models": ["local-coder"]})
    ctx.expect(ready.status == 200, "coding node reports inventory", ready)
    return owner(ctx), planner, coder


def form(ctx):
    return dict(title="Cross-file fix", goal="Update both files.", repo="demo-" + ctx.id, lane="fix",
                agent="coder", model="harness:local-coder", plan_with="codex")


@case("client", "planning", "planner-then-coder")
def _(ctx):
    client, planner, coder = setup(ctx)
    tools = {tool["name"]: tool for tool in mcp(ctx, coder, None)}
    ctx.expect("plan_with" in tools["start_task"]["inputSchema"]["properties"], "MCP exposes plan_with")
    args = dict(form(ctx), dispatch_id=new_key())
    started = mcp(ctx, coder, "start_task", **args)
    planner_id, intent = started["planning_job_id"], started["intent_id"]
    ctx.expect(started["job_id"] == planner_id and started["model"] == "harness:local-coder",
               "MCP exposes the planning job and intended coding model", started)
    job = planner.job(planner_id)
    ctx.expect(job["meta"]["agent"] == "researcher" and job["meta"]["model"] == "codex"
               and job["meta"]["repo_read_only"] == "true", "planner uses the read-only profile", job)
    prefix = "jobs/" + planner_id + "/files/"
    spec = client.call("GET", prefix + "spec.yaml").body
    goal = client.call("GET", prefix + job["task"] + "/goal.md").body
    ctx.expect(all(field in spec for field in (b"planning_lane: fix", b"seconds: 600", b"turns: 15"))
               and b"Update both files." in goal and b"plan.md" in goal, "planner has its lane base, goal and budget")
    ctx.expect(coder.call("GET", "jobs").json["jobs"] == [], "coding job is deferred")
    plan = b"# Plan\nChange a and b, then run the frozen check.\n"
    run_id = ctx.serial_tag()
    run = wire.run_fixture(run_id, {"plan.md": plan})
    run["envelope"]["job_id"] = planner_id
    run["envelope"]["usage"] = wire.usage(charged_usd=0, est_usd=0.03,
                                         source={"class": "subscription", "model": "codex"})
    end_job(ctx, planner, planner_id, run_id)  # Terminal can reach the Director before the run objects.
    ctx.expect(coder.call("GET", "jobs").json["jobs"] == [], "wait for the planner's run envelope")
    for data in run["files"].values():
        ctx.expect(planner.call("PUT", "objects/" + wire.sha(data), raw=data).status == 200, "upload planner file")
    for _ in range(2):
        answer = planner.call("POST", "envelopes", body={k: run[k] for k in ("envelope", "row_line")})
        ctx.expect(answer.json["status"] == "acked", "planner run acknowledged", answer)
    end_job(ctx, planner, planner_id, run_id)
    coding, = coder.call("GET", "jobs").json["jobs"]
    coding_id = coding["job_id"]
    prefix = "jobs/" + coding_id + "/files/" + coding["task"]
    ctx.expect(client.call("GET", prefix + "/context/plan.md").body == plan, "plan frozen as a bundle file")
    goal = client.call("GET", prefix + "/goal.md").body
    ctx.expect(b"Update both files." in goal and b"context/plan.md" in goal and goal.endswith(plan),
               "original goal and frozen plan are visible to the harness")
    status = mcp(ctx, coder, "task_status", job_id=planner_id)
    ctx.expect(status["intent_id"] == intent and {j["phase"] for j in status["jobs"]} == {"planning", "coding"},
               "both phases share an intent", status)
    waited = mcp(ctx, coder, "wait_tasks", job_ids=[planner_id], timeout_seconds=5)
    ctx.expect(waited["coding_jobs"][planner_id]["job_id"] == coding_id, "wait exposes coding successor", waited)
    trace = client.call("GET", "intents/" + intent + "/work-trace").json["work_trace"]
    ctx.expect([j["job_id"] for j in trace["jobs"]] == [planner_id, coding_id], "one ordered work trace", trace)
    run_check_trace(ctx, trace, {"result": "complete", "missing": []})
    report_run(ctx, coder, ctx.serial_tag() + "-coding", cls="free_local", model="local-coder", job_id=coding_id)
    end_job(ctx, coder, coding_id, ctx.serial_tag() + "-coding")
    bundle = client.call("GET", "intents/" + intent).json
    ctx.expect(len(bundle["jobs"]) == 2 and len(bundle["runs"]) == 2, "both runs are recorded separately", bundle)
    costs = {run["usage"]["source"]["class"]: run["usage"]["charged_usd"] for run in bundle["runs"]}
    ctx.expect(costs == {"subscription": 0, "free_local": 0}, "subscription and DGX costs remain separate", costs)
    replay = mcp(ctx, coder, "start_task", **args)
    ctx.expect(replay["job_id"] == coding_id and replay["intent_id"] == intent, "replay reports the current job", replay)


@case("client", "planning", "planner-fallback")
def _(ctx):
    client, planner, coder = setup(ctx)
    refused(ctx, post(client, "intents", {"node": coder.name, "lane": dict(form(ctx), plan_with=True)}),
            422, "invalid_request", "plan_with must be a model")
    for state in ("error", "done"):
        submitted = post(client, "intents", {"node": coder.name, "lane": form(ctx)})
        ctx.expect(submitted.status == 201, "planning intent created", submitted)
        intent = submitted.json["intent"]["intent_id"]
        planner_id = submitted.json["jobs"][0]["job_id"]
        end_job(ctx, planner, planner_id, state=state)
        coding, previous = client.call("GET", "intents/" + intent).json["jobs"]
        ctx.expect(previous["job_id"] == planner_id and coding["state"] == "offered"
                   and coding["intent_id"] == intent, "coding follows a failed or empty planner")
        prefix = "jobs/" + coding["job_id"] + "/files/" + coding["task"]
        ctx.expect(client.call("GET", prefix + "/goal.md").body == b"Update both files."
                   and client.call("GET", prefix + "/context/plan.md").status == 404, "original goal without plan")
        trace = client.call("GET", "intents/" + intent + "/work-trace").json["work_trace"]
        reason = trace["jobs"][1]["reason"]
        ctx.expect(reason["kind"] == "planning_fallback" and
                   ("failed" if state == "error" else "plan.md") in reason["detail"], "trace explains fallback", trace)
        run_check_trace(ctx, trace, {"result": "complete", "missing": []})
