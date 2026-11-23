"""Node capabilities and executor fit (NODE-PROTOCOL.md status, job routing)."""
import json

from harness.client import Client, bundle_files, new_key
from harness.run import case
from harness import wire

C = "client"
RUST = ("rust",)


def owner(ctx):
    from cases.client import owner as _owner
    return _owner(ctx)


def spec_with_model(model, usd="0.25"):
    text = wire.SPEC.replace("model: fake", "model: %s" % model).replace("usd: 0.25", "usd: %s" % usd)
    return text.encode()


def upload_spec(client, model):
    files = dict(wire.JOB_FILES)
    files["spec.yaml"] = (spec_with_model(model), files["spec.yaml"][1])
    for path, (data, _) in files.items():
        client.call("PUT", "objects/" + wire.sha(data), raw=data)
    return [{"path": p, "sha256": wire.sha(d), "mode": m} for p, (d, m) in sorted(files.items())]


@case(C, "work", "node-capabilities-reported", RUST)
def node_capabilities_reported(ctx):
    """A status report carries release, templates and harness."""
    n, c = ctx.node2(), owner(ctx)
    caps = {"templates": ["codex", "claude-code"], "harness": True, "preferred": "claude-code"}
    body = {"active": [], "agent_capabilities": caps, "release": {"id": "20261116-abc", "protocol": 2}}
    ctx.expect(n.call("POST", "status", body=body).status == 200, "status accepted")
    node = c.call("GET", "nodes/" + n.name).json
    st = node["status"]
    ctx.expect(st["release"] == body["release"], "release stored", st)
    ctx.expect(st["agent_capabilities"]["templates"] == caps["templates"], "templates", st)
    ctx.expect(st["agent_capabilities"]["harness"] is True, "harness flag", st)


@case(C, "work", "job-routed-to-capable-node", RUST)
def job_routed_to_capable_node(ctx):
    """A job is offered only after the node reports the executor template."""
    n, c = ctx.node2(), owner(ctx)
    caps = {"templates": ["codex"], "harness": True}
    n.status([], agent_capabilities=caps)
    client = Client(ctx.target, ctx.target.local_token())
    flist = upload_spec(client, "codex")
    key = new_key()
    answer = client.call(
        "POST",
        "intents",
        body={"node": n.name, "spec": "spec.yaml", "task": "tasks/t", "files": flist, "title": "cap"},
        key=key,
    )
    ctx.expect(answer.status == 201, "intent created", answer)
    job = answer.json["jobs"][0]
    ctx.expect(job["state"] == "offered", "job offered when template fits", job)
    ctx.expect(len(n.call("GET", "jobs").json["jobs"]) == 1, "node sees the job")
    other = ctx.node2("b")
    other_caps = {"templates": ["claude-code"], "harness": True}
    other.status([], agent_capabilities=other_caps)
    other_jobs = other.call("GET", "jobs").json["jobs"]
    ctx.expect(job["job_id"] not in [j["job_id"] for j in other_jobs], "only the assigned node sees it")


@case(C, "work", "refuse-named-node-missing-template", RUST)
def refuse_named_node_missing_template(ctx):
    """Submitting to a node that reports no matching template is refused."""
    n, c = ctx.node2(), owner(ctx)
    n.status([], agent_capabilities={"templates": ["claude-code"], "harness": True})
    client = Client(ctx.target, ctx.target.local_token())
    flist = upload_spec(client, "codex")
    answer = client.call(
        "POST",
        "intents",
        body={"node": n.name, "spec": "spec.yaml", "task": "tasks/t", "files": flist, "title": "nope"},
        key=new_key(),
    )
    ctx.expect(answer.status == 422, "refused", answer)
    ctx.expect("cannot run executor template codex" in answer.body.decode(), "names template", answer)
    ctx.expect(n.name in answer.body.decode(), "names node", answer)


@case(C, "work", "queued-job-capability-reason", RUST)
def queued_job_capability_reason(ctx):
    """When the node reports it lacks the template, the job waits with a client-visible reason."""
    n, c = ctx.node2(), owner(ctx)
    n.status([], agent_capabilities={"templates": [], "harness": True})
    bundle = ctx.target.tmp / "codex-bundle"
    bundle.mkdir(exist_ok=True)
    for path, (data, mode) in wire.JOB_FILES.items():
        dest = bundle / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        if path == "spec.yaml":
            dest.write_bytes(spec_with_model("codex"))
        else:
            dest.write_bytes(data)
        dest.chmod(mode)
    created = ctx.target.admin("job", "create", "--node", n.name, "--bundle", str(bundle))
    job_id = created["job_id"]
    shown = c.call("GET", "jobs/" + job_id).json
    ctx.expect(shown["state"] == "waiting", "held for fit", shown)
    budget = shown["budget"]
    ctx.expect(budget["reason"] == "executor_unavailable", "reason code", budget)
    ctx.expect("codex" in (budget.get("detail") or ""), "detail names template", budget)
    n.status([], agent_capabilities={"templates": ["codex"], "harness": True})
    shown = c.call("GET", "jobs/" + job_id).json
    ctx.expect(shown["state"] == "offered", "offered after the node reports the template", shown)


@case(C, "work", "harness-model-lane-offer", RUST)
def harness_model_lane_offer(ctx):
    """A gateway model is frozen into a native harness bundle for a capable node."""
    capable, cli_only = ctx.node2(), ctx.node2("b")
    client = owner(ctx)
    caps = {"templates": ["codex"], "harness_models": ["local-coder"]}
    capable.status([], agent_capabilities={**caps, "harness": True})
    cli_only.status([], agent_capabilities={**caps, "harness": False})
    lane = {"title": "Native coding", "goal": "Make the change", "agent": "coder",
            "repo": "sample", "lane": "native", "model": "harness:local-coder"}

    def submit(node):
        return client.call("POST", "intents", body={"node": node.name, "lane": lane}, key=new_key())

    refused = submit(cli_only)
    ctx.expect(refused.status == 422 and b"cannot run the native harness" in refused.body,
               "CLI-only node refused", refused)
    accepted = submit(capable)
    ctx.expect(accepted.status == 201, "harness lane accepted", accepted)
    job = accepted.json["jobs"][0]
    ctx.expect(job["state"] == "offered", "harness lane offered", job)
    rows = capable.call("GET", "jobs").json["jobs"]
    ctx.expect(len(rows) == 1 and rows[0]["job_id"] == job["job_id"], "capable node sees offer", rows)
    ctx.expect(cli_only.call("GET", "jobs").json["jobs"] == [], "CLI-only node sees no offers")
    spec = next(f for f in rows[0]["files"] if f["path"] == "spec.yaml")
    body = capable.call("GET", "objects/" + spec["sha256"]).body
    ctx.expect(b'\nmodel: "local-coder"\n' in body and b"fs_write" in body and b"\nrepo: sample\n" in body,
               "native bundle contains the gateway model, tools and repo", body)
    capable.status([], agent_capabilities={**caps, "harness": False})
    ctx.expect(capable.call("GET", "jobs").json["jobs"] == [], "stale native offer is hidden")
    capable.status([], agent_capabilities={**caps, "harness": True})
    ctx.expect(len(capable.call("GET", "jobs").json["jobs"]) == 1, "native offer returns with capability")



@case(C, "work", "unknown-harness-model-refused", RUST)
def unknown_harness_model_refused(ctx):
    node = ctx.node2()
    node.status([], agent_capabilities={"templates": [], "harness": True, "harness_models": ["local-coder"]})
    refused = owner(ctx).call(
        "POST", "intents",
        body={"node": node.name, "lane": {"title": "Unknown", "goal": "Run", "model": "harness:absent"}},
        key=new_key(),
    )
    ctx.expect(refused.status == 422 and b"unknown harness model absent" in refused.body,
               "unknown gateway model refused", refused)
