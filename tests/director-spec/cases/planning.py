"""Planner/executor dispatch with fake nodes and the real cs mcp entry point."""

from cases.budgets import end_job, report_run
from cases.client import owner, refused
from cases.decision_log import mcp, post
from cases.work_trace import run_check_trace
from harness import wire
from harness.client import new_key
from harness.run import case


def setup(ctx, *, coder_templates=()):
    planner, coder = ctx.node2("planner"), ctx.node2("dgx")
    repos = [{"name": "demo-" + ctx.id, "path": "/fixture/demo", "branch": "main"}]
    ready = planner.status([], platform="Darwin", repos=repos, agent_capabilities={"templates": ["codex"]})
    ctx.expect(ready.status == 200, "planner node reports inventory", ready)
    ready = coder.status([], platform="Linux", repos=repos, agent_capabilities={
        "templates": list(coder_templates), "harness": True, "harness_models": ["local-coder"]})
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
    ctx.expect(b"read the shebang or the file itself" in goal
               and b"## Files\n- path/to/file.rs (rust)\n- cs/ops/bin/finalize-run (python)" in goal,
               "planner is prompted for the final file and language list", goal)
    ctx.expect(coder.call("GET", "jobs").json["jobs"] == [], "coding job is deferred")
    plan = b"# Plan\nChange a and b, then run the frozen check.\n\n## Files\n- a (rust)\n- b (rust)\n"
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


def finish_plan(ctx, planner, planner_id, plan):
    run_id = ctx.serial_tag() + "-" + planner_id[-6:].lower()
    run = wire.run_fixture(run_id, {"plan.md": plan})
    run["envelope"]["job_id"] = planner_id
    for data in run["files"].values():
        ctx.expect(planner.call("PUT", "objects/" + wire.sha(data), raw=data).status == 200, "upload planner file")
    answer = planner.call("POST", "envelopes", body={k: run[k] for k in ("envelope", "row_line")})
    ctx.expect(answer.json["status"] == "acked", "planner run acknowledged", answer)
    end_job(ctx, planner, planner_id, run_id)


@case("client", "planning", "auto-model-routing")
def _(ctx):
    client, planner, coder = setup(ctx, coder_templates=("codex",))
    for plan, model, languages, reason in (
        (b"## Files\n- harness/src/trace.rs (rust)\n- cs/ops/bin/finalize-run (python)\n",
         "codex", ["python", "rust"], "plan files list python+rust"),
        (b"Use tests/check.py to verify the change.\n\n## Files\n- director/src/jobs.rs (rust)\n",
         "harness:local-coder", ["rust"], "plan files list rust"),
        (b"## Files\n- assets/app.ts (typescript)\n- assets/app.js (javascript)\n- scripts/check (bash)\n",
         "codex", ["js", "shell"], "plan files list js+shell"),
        (b"See examples/sample.rs and examples/sample.py.\n\n## Files\n- README.md (markdown)\n"
         b"- config.json (json)\n- config.yml (yaml)\n- Cargo.toml (toml)\n- data (text)\n",
         "harness:local-coder", [], "plan files list no recognized coding languages"),
        (b"Edit `director/src/jobs.rs`.\n\n## Files\n",
         "harness:local-coder", ["rust"], "plan names rust (extensions)"),
        (b"Edit `director/src/jobs.rs` and [the MCP entry point](cs/scripts/cs_mcp.py).",
         "codex", ["python", "rust"], "plan names python+rust (extensions)"),
        (b"Edit `src/main.rs`, `README.md`, `config.json`, `config.yml`, `Cargo.toml`, and `Cargo.lock`.",
         "harness:local-coder", ["rust"], "plan names rust (extensions)"),
        (b"Update the documentation.", "harness:local-coder", [],
         "plan names no files with recognized coding extensions"),
        (None, "harness:local-coder", [], "planning job failed"),
    ):
        args = dict(form(ctx), model="auto", dispatch_id=new_key())
        del args["plan_with"]
        started = mcp(ctx, coder, "start_task", **args)
        planner_id, intent_id = started["planning_job_id"], started["intent_id"]
        ctx.expect(started["model"] == "auto" and planner.job(planner_id)["meta"]["model"] == "codex",
                   "auto defers executor selection and defaults to a Codex planner", started)
        if plan is None:
            end_job(ctx, planner, planner_id, state="error")
        else:
            finish_plan(ctx, planner, planner_id, plan)
        bundle = client.call("GET", "intents/" + intent_id).json
        coding, previous = bundle["jobs"]
        route = coding["meta"]["route"]
        target = coder  # Routing picks the model; the coding job and the intent stay on the requested node.
        ctx.expect(bundle["intent"]["node"] == coder.name, "the intent keeps its node", bundle["intent"])
        ctx.expect(previous["job_id"] == planner_id and coding["node"] == target.name
                   and coding["state"] == "offered" and coding["meta"]["model"] == model,
                   "coding offer goes to the selected executor", coding)
        ctx.expect(route["model"] == model and route["languages"] == languages and route["reason"] == f"{reason} → {model}",
                   "job metadata explains the route", coding)
        spec = client.call("GET", "jobs/" + coding["job_id"] + "/files/spec.yaml").body
        expected = b"model: codex\n" if model == "codex" else b'model: "local-coder"\n'
        ctx.expect(expected in spec and (b"tools: []" in spec) == (model == "codex")
                   and ("intent: " + intent_id).encode() in spec, "frozen coding spec matches the route", spec)
        status = mcp(ctx, coder, "task_status", job_id=planner_id)
        ctx.expect(status["jobs"][-1]["route"] == route, "task_status exposes the coding route", status)
        trace = client.call("GET", "intents/" + intent_id + "/work-trace").json["work_trace"]
        if plan is not None:
            ctx.expect(trace["jobs"][1]["reason"] == {"kind": "planned", "planner_job_id": planner_id},
                       "work trace retains the existing planned reason", trace)
        replay = mcp(ctx, coder, "start_task", **args)
        ctx.expect(replay["job_id"] == coding["job_id"] and replay["node"] == target.name and replay["model"] == model,
                   "idempotent replay exposes the routed coding job", replay)


@case("client", "planning", "auto-model-unavailable-on-intent-node")
def _(ctx):
    client, planner, coder = setup(ctx)
    started = mcp(ctx, coder, "start_task", **dict(form(ctx), model="auto"))
    planner_id = started["planning_job_id"]
    finish_plan(ctx, planner, planner_id, b"## Files\n- src/main.rs (rust)\n- scripts/check.py (python)\n")

    bundle = client.call("GET", "intents/" + started["intent_id"]).json
    coding, previous = bundle["jobs"]
    ctx.expect(previous["job_id"] == planner_id and coding["meta"]["model"] == "codex",
               "the plan routes coding to Codex", coding)
    ctx.expect(bundle["intent"]["node"] == coder.name and coding["node"] == coder.name,
               "the coding job stays on the intent node", bundle)
    shown = client.call("GET", "jobs/" + coding["job_id"]).json
    ctx.expect(shown["state"] == "waiting" and shown["budget"]["reason"] == "executor_unavailable",
               "the coding job waits for executor capability", shown)
    for node in (coder, planner):
        offers = node.call("GET", "jobs").json["jobs"]
        ctx.expect(all(job["job_id"] != coding["job_id"] for job in offers),
                   "the waiting coding job is not offered", offers)


@case("client", "planning", "explicit-model-bypasses-auto")
def _(ctx):
    client, _, coder = setup(ctx)
    args = form(ctx)
    del args["plan_with"]
    started = mcp(ctx, coder, "start_task", **args)
    jobs = client.call("GET", "intents/" + started["intent_id"]).json["jobs"]
    ctx.expect(len(jobs) == 1 and jobs[0]["meta"]["model"] == args["model"]
               and "route" not in jobs[0]["meta"] and "planning_job_id" not in started,
               "explicit model is offered directly", started)
