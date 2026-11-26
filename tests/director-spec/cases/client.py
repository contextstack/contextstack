"""Client side: a Director's Client API against CLIENT-API.md, over HTTP only. Rust target only."""
import json

from unittest.mock import patch

from harness import wire
from harness.client import Client, bundle_files, new_key
from harness.fake_gateway import BY_NAME
from harness.run import case
from harness.wire import run_fixture, sha

C = "client"


@case(C, "catalog", "models-listed")
def models_list(ctx):
    """Usable executor templates from the gateway, labelled, with a default."""
    c = owner(ctx)
    a = c.call("GET", "models")
    ctx.expect(a.status == 200, "GET models is 200", a)
    names = {m["name"] for m in a.json.get("models") or []}
    ctx.expect({"codex", "local-coder"}.issubset(names), "subscription and local templates appear", a.json)
    labels = {m["name"]: m["label"] for m in a.json.get("models") or []}
    ctx.expect(labels.get("codex") == "Codex CLI, uses its default model", "codex label", labels)
    ctx.expect(labels.get("local-coder") == "Local coder (vLLM, configured model)", "local label", labels)
    ctx.expect(a.json.get("default_model") == "codex", "default is the first subscription model", a.json)
    by_name = {m["name"]: m for m in a.json.get("models") or []}
    ctx.expect(by_name.get("codex", {}).get("provider") == "openai", "codex provider", by_name.get("codex"))
    ctx.expect(by_name.get("codex", {}).get("billing") == "subscription", "codex billing", by_name.get("codex"))
    off = by_name.get("claude-code") or {}
    ctx.expect(off.get("available") is False and "claude" in (off.get("unavailable_reason") or ""), "unavailable cli reason", off)


@case(C, "catalog", "harness-readiness")
def harness_readiness(ctx):
    limited, ready = ctx.node2(), ctx.node2()
    limited.status([], agent_capabilities={"templates": ["codex", "cursor-sol"]},
                   provider_limits=[{"template": "codex"}, {"template": "cursor-sol"}])
    ready.status([], agent_capabilities={"templates": ["cursor-sol"]})
    try:
        catalog = owner(ctx).call("GET", "models").json["catalog"]
    finally:
        for node in (limited, ready):
            node.status([], agent_capabilities={}, provider_limits=[])
    harnesses = {row["id"]: row for row in catalog["harnesses"]}
    for name in ("antigravity", "opencode"):
        ctx.expect(harnesses[name]["unavailable_reason"] == "no machine reports this harness",
                   "no reporting machine", harnesses[name])
    ctx.expect(harnesses["codex"]["unavailable_reason"] == "reported executor templates are provider-limited"
               and harnesses["codex"]["nodes"] == [], "all reporting machines limited", harnesses["codex"])
    ctx.expect(harnesses["cursor-agent"]["unavailable_reason"] is None
               and harnesses["cursor-agent"]["nodes"] == [ready.name], "one ready machine is sufficient",
               harnesses["cursor-agent"])


@case(C, "catalog", "models-picker-shape")
def models_picker_shape(ctx):
    """Picker metadata and fleet capabilities come from the server."""
    first, second, unknown = ctx.node2(), ctx.node2(), ctx.node2()
    for node, caps in [
        (first, {"templates": ["codex"], "harness": True, "harness_models": ["local-coder"]}),
        (second, {"templates": ["codex", "antigravity-hydrogen"], "harness": False,
                  "harness_models": ["local-coder"]}),
        (unknown, {"templates": [], "harness": True}),
    ]:
        report = node.call("POST", "status", body={"agent_capabilities": caps})
        ctx.expect(report.status == 200, "capability report accepted", report)
    answer = owner(ctx).call("GET", "models")
    ctx.expect(answer.status == 200, "GET models is 200", answer)
    rows = {row["name"]: row for row in answer.json["models"]}
    for row in rows.values():
        ctx.expect(all(isinstance(row[key], str) and row[key]
                       for key in ("provider", "provider_label", "label"))
                   and isinstance(row["internal"], bool)
                   and all(isinstance(row[key], list) and all(isinstance(v, str) for v in row[key])
                           for key in ("efforts", "nodes")),
                   "all picker fields have the documented types", row)
    codex = rows["codex"]
    ctx.expect(codex["nodes"] == sorted([first.name, second.name]), "both CLI nodes", codex)
    ctx.expect(codex["provider_label"] == "OpenAI" and not codex["internal"]
               and codex["efforts"] == ["low", "medium", "high", "xhigh"], "Codex choices", codex)
    local = rows["local-coder"]
    ctx.expect(local["nodes"] == [first.name] and local["efforts"] == [], "reported harness catalog", local)
    planner = rows["claude-code-sonnet-planner"]
    ctx.expect(planner["internal"] and planner["nodes"] == [], "internal role remains listed", planner)
    reasoner = rows["local-reasoner"]
    ctx.expect(reasoner["provider"] == "vllm"
               and reasoner["label"] == "Local reasoner (vLLM, configured model)",
               "flat picker metadata comes from the provider overlay", reasoner)
    hydrogen = rows["antigravity-hydrogen"]
    ctx.expect(hydrogen["provider"] == "antigravity" and hydrogen["provider_label"] == "Antigravity"
               and hydrogen["label"] == "Gemini 4 Hydrogen High (Antigravity)"
               and hydrogen["nodes"] == [second.name] and not hydrogen["available"],
               "picker provider differs from routing provider and node support is independent", hydrogen)


@case(C, "catalog", "models-harnesses-and-providers")
def models_harnesses_and_providers(ctx):
    node = ctx.node2()
    node.status([], agent_capabilities={
        "templates": ["cursor-sol", "codex"], "harness": True, "harness_models": ["local-coder"],
    })
    answer = owner(ctx).call("GET", "models")
    ctx.expect(answer.status == 200, "catalog loaded", answer)
    catalog = answer.json["catalog"]
    providers = {row["id"]: row["label"] for row in catalog["providers"]}
    harnesses = {row["id"]: row for row in catalog["harnesses"]}
    models = {row["id"]: row for row in catalog["models"]}
    ctx.expect("legacy_models" not in answer.json, "flat choices use the original models key", answer)
    ctx.expect(catalog["default"] == {"harness": "codex", "model": "codex", "value": "codex"},
               "structured default resolves to the flat default", catalog)
    for family in ("codex", "goose", "claude-code"):
        choice = models[family]
        ctx.expect(family in harnesses and choice["version"] is None
                   and choice["harnesses"] == [{"harness": family, "value": family}],
                   "each CLI has an executable default", choice)
    ctx.expect(all(not row["internal"] for row in models.values()),
               "structured choices are public", catalog)
    ctx.expect(harnesses["cursor-agent"]["nodes"] == [node.name]
               and node.name in harnesses["contextstack"]["nodes"],
               "harness nodes reflect reported capabilities", harnesses)
    ctx.expect(harnesses["pi"]["billing"] == "provider"
               and harnesses["pi"]["provider_ids"] == ["openai"],
               "Pi takes its billing from the model provider", harnesses["pi"])
    ctx.expect(models["gpt-5.6-sol-high"]["version"] == "gpt-5.6-sol-high"
               and models["gpt-5.6-sol-high"]["provider"] == "openai"
               and models["gpt-5.6-sol-high"]["harnesses"] == [{"harness": "cursor-agent", "value": "cursor-sol"}],
               "pinned model and exact executable value", models)
    ctx.expect(models["opencode-go/kimi-k3"]["harnesses"] == [{"harness": "opencode", "value": "opencode-go"}],
               "model read from the CLI profile", models)
    ctx.expect(providers.get("vllm") == "vLLM" and "vllm-spark2" not in providers
               and models["local-coder"]["provider"] == models["local-reasoner"]["provider"] == "vllm",
               "vLLM deployments share a picker provider", catalog)
    cloud = models["minimax-m2.1:cloud"]
    ctx.expect(cloud["billing"] == "hosted" and "hosted" in cloud["label"],
               "Ollama cloud models are labelled hosted", cloud)


@case(C, "catalog", "cli-model-content")
def cli_model_content(ctx):
    gateway = ctx.target.gateway
    extra = [{**BY_NAME[name], "available": True} for name in (
        "claude-opus-5", "claude-sonnet-4-5-20250929", "gpt-4o", "gemini-2.5-pro",
        "llama-3.3-70b-versatile", "meta-llama/Llama-3.3-70B-Instruct-Turbo", "z-ai/glm-5.3-flash",
    )]
    with patch.object(gateway, "models", [*gateway.models, *extra]):
        catalog = owner(ctx).call("GET", "models").json["catalog"]
    models = {row["id"]: row for row in catalog["models"]}
    pairs = [pair for row in catalog["models"] for pair in row["harnesses"]]
    values = [pair["value"] for pair in pairs]
    codex_models = {
        "gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-terra", "gpt-6.1-sol", "gpt-6-astra",
        "gpt-6-sol", "gpt-6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna",
    }
    ctx.expect({pair["value"] for pair in pairs if pair["harness"] == "codex"}
               == {"codex", *("codex:" + model for model in codex_models)}, "all Codex selections", pairs)
    ctx.expect(len(values) == len(set(values)) and not {"pi", "opencode-go"} & models.keys(),
               "pinned templates have unique values and no CLI default", values)
    sol = models["gpt-6.1-sol"]
    ctx.expect(sol["label"] == "GPT-6.1 Sol" and sol["harnesses"] == [
        {"harness": "codex", "value": "codex:gpt-6.1-sol"}, {"harness": "pi", "value": "pi"},
    ], "explicit Pi alias joins Codex", sol)
    for model, label in {
        "gpt-5.6-sol-high": "GPT-5.6 Sol High",
        "gemini-4-hydrogen-high": "Gemini 4 Hydrogen High",
        "opencode-go/kimi-k3": "Kimi K3",
        "claude-opus-5": "Claude Opus 5",
        "claude-sonnet-4-5-20250929": "Claude Sonnet 4.5 (2025-09-29)",
        "gpt-4o": "GPT-4o",
        "gemini-2.5-pro": "Gemini 2.5 Pro",
    }.items():
        ctx.expect(models[model]["label"] == label, "readable model label", models[model])
    ctx.expect(models["gpt-5.6-sol-high"]["harnesses"] == [{"harness": "cursor-agent", "value": "cursor-sol"}]
               and models["gpt-5.6-sol"]["harnesses"] == [{"harness": "codex", "value": "codex:gpt-5.6-sol"}],
               "similar names without an alias stay separate", models)
    providers = {row["id"]: row["label"] for row in catalog["providers"]}
    ctx.expect(providers == {
        "anthropic": "Anthropic", "google": "Google", "groq": "Groq", "ollama": "Ollama",
        "openai": "OpenAI", "opencode": "OpenCode", "openrouter": "OpenRouter", "together": "Together AI",
        "various": "Various providers", "vllm": "vLLM",
    }, "provider labels", providers)
    ctx.expect(models["goose"]["provider"] == "various" and models["codex"]["provider"] == "openai",
               "CLI defaults carry their configured or fallback provider", models)


@case(C, "catalog", "repos-listed")
def repos_list(ctx):
    """Machine inventories are shared through the Director, without its own repos file."""
    n = ctx.node2()
    c = owner(ctx)
    rows = [{"name": "contextstack", "path": "/tmp/main", "branch": "main"}]
    a = n.call("POST", "status", body={"repos": rows})
    ctx.expect(a.status == 200, "repo report accepted", a)
    a = c.call("GET", "repos")
    ctx.expect(a.status == 200, "GET repos is 200", a)
    expected = {**rows[0], "node": n.name}
    ctx.expect(expected in a.json.get("repos", []), "reported repo and machine appear", a.json)
    other = ctx.node2()
    other_row = {"name": "contextstack", "path": "/another/checkout", "branch": "dev"}
    other.call("POST", "status", body={"repos": [other_row]})
    both = c.call("GET", "repos").json["repos"]
    ctx.expect(expected in both and {**other_row, "node": other.name} in both,
               "same repo name on different machines remains distinct", both)
    node = c.call("GET", "nodes/" + n.name)
    ctx.expect(node.json["status"]["repos"] == rows, "machine exposes inventory", node)
    # The hosted lead picks a repo itself (cs mcp list_repos): its credential may read repos.
    from cases.conversations import created
    l = lead(ctx, c, created(ctx, c, n))
    seen = l.call("GET", "repos")
    ctx.expect(seen.status == 200 and expected in seen.json.get("repos", []), "a lead reads repos", seen)
    for bad in ("bad", [{}], [{**rows[0], "branch": ""}], rows * 2):
        a = n.call("POST", "status", body={"repos": bad})
        ctx.expect(a.status == 422, "invalid inventory refused", a)
    n.call("POST", "status", body={"repos": []})
    a = c.call("GET", "repos")
    ctx.expect(expected not in a.json["repos"], "empty inventory removes repo", a)



def refused(ctx, answer, status, code, what):
    ctx.expect(answer.status == status and (answer.json or {}).get("code") == code
               and all(isinstance((answer.json or {}).get(k), str) for k in ("error", "detail")),
               "%s: want %d %s with {error, code, detail}" % (what, status, code), answer)


def owner(ctx):
    return Client(ctx.target, ctx.target.local_token())


def device(ctx, name="phone"):
    token, cid = ctx.target.pair("%s-%s" % (ctx.id, name))
    return Client(ctx.target, token, cid)


def submitted(ctx, client, node, **over):
    client.upload_bundle()
    a = client.submit(node.name, **over)
    ctx.expect(a.status == 201, "POST intents is 201", a)
    return a.json


def accept(ctx, node, job_id):
    digest = node.job(job_id)["bundle_sha256"]
    a = node.call("POST", "jobs/%s/accept" % job_id, body={"bundle_sha256": digest})
    ctx.expect(a.status == 200, "node accept", a)


def finish(ctx, node, job_id, run_id=None, files=None):
    """The node reports a run and the job's end, as spool-send would. Returns the run fixture."""
    run_id = run_id or "%s-run" % ctx.serial_tag()
    run = run_fixture(run_id, overrides=files)
    for data in run["files"].values():
        ctx.expect(node.call("PUT", "objects/" + sha(data), raw=data).status == 200, "PUT objects")
    a = node.call("POST", "envelopes", body={"envelope": run["envelope"], "row_line": run["row_line"]})
    ctx.expect((a.json or {}).get("status") == "acked", "run_finalized acked", a)
    row = json.dumps({"state": "done", "director_job": job_id, "run_id": run_id})
    term = {"envelope_version": 1, "event_id": "job_terminal:" + job_id, "job_id": job_id, "files": [],
            "envelope_sha256": sha(b""), "row_sha256": sha(row)}
    a = node.call("POST", "envelopes", body={"envelope": term, "row_line": row})
    ctx.expect((a.json or {}).get("status") == "acked", "job_terminal acked", a)
    return run


def stamped(original, intent_id):
    """`original` as the Director stamps it: its `intent:` line replaced, else one appended. No other byte changes."""
    lines = original.splitlines(keepends=True)
    line = "intent: %s\n" % intent_id
    at = [i for i, l in enumerate(lines) if l.startswith("intent:")]
    if at:
        lines[at[0]] = line
    else:
        lines.append(("" if not lines or lines[-1].endswith("\n") else "\n") + line)
    return "".join(lines)


def stamped_bundle(ctx, node, job, intent_id, given, original=None):
    """The job's bundle is `given` with its spec's intent: set to `intent_id`, every other byte of the spec as given
    (comments, order, quoting, the budget's style), and its bundle_sha256 covers that stamp."""
    by_path = {f["path"]: f for f in job["files"]}
    ctx.expect(set(by_path) == {f["path"] for f in given}
               and all(by_path[f["path"]]["sha256"] == f["sha256"] for f in given if f["path"] != "spec.yaml"),
               "every file but the spec is offered as given", job["files"])
    spec = node.call("GET", "objects/" + by_path["spec.yaml"]["sha256"]).body.decode()
    if original is None:  # an earlier job's spec, which the node may read
        original = node.call("GET", "objects/" + next(f["sha256"] for f in given if f["path"] == "spec.yaml")).body.decode()
    ctx.expect(spec == stamped(original, intent_id), "the offered spec is the given one plus its intent line (%s)"
               % intent_id, {"given": original, "offered": spec})
    ctx.expect(job["bundle_sha256"] == wire.bundle_sha(job["files"]), "the job's hash covers the stamped spec", job)


def revision(record):
    return record.get("revision") if isinstance(record, dict) else None


# ---- §4 work ----
@case(C, "work", "intent-submit-recorded")
def _(ctx):
    """An intent names its spec, task and node; the answer is the recorded intent and its one offered job."""
    n, c = ctx.node(), owner(ctx)
    got = submitted(ctx, c, n)
    intent, jobs = got.get("intent") or {}, got.get("jobs") or []
    ctx.expect(intent.get("node") == n.name and intent.get("spec") == "spec.yaml" and intent.get("task") == "tasks/t",
               "the intent is as asked", got)
    ctx.expect(len(jobs) == 1 and jobs[0]["state"] == "offered" and jobs[0]["intent_id"] == intent["intent_id"]
               and intent.get("job_ids") == [jobs[0]["job_id"]], "one offered job of the intent", got)
    ctx.expect(intent.get("bundle_sha256") == wire.bundle_sha(bundle_files()), "the intent keeps the listed files", got)
    seen = n.job(jobs[0]["job_id"])
    ctx.expect(seen["spec"] == "spec.yaml" and seen["task"] == "tasks/t", "the node is offered it", seen)
    stamped_bundle(ctx, n, seen, intent["intent_id"], bundle_files(), wire.SPEC)
    ctx.expect(jobs[0]["bundle_sha256"] == seen["bundle_sha256"], "the client sees the job's bundle as offered", got)
    again = c.call("GET", "intents/" + intent["intent_id"])
    ctx.expect(again.status == 200 and again.json["intent"] == intent and again.json["jobs"] == jobs,
               "GET intents/{id} is the same record", again)
    listed = c.call("GET", "intents?node=" + n.name).json
    ctx.expect([i["intent_id"] for i in listed["intents"]] == [intent["intent_id"]]
               and [j["job_id"] for j in listed["jobs"]] == [jobs[0]["job_id"]], "GET intents lists the work", listed)


@case(C, "work", "intent-objects-missing")
def _(ctx):
    """Objects come first: an intent naming unstored objects is 422 objects_missing, listing them, and records
    nothing; after the upload the same key succeeds."""
    n, c = ctx.node(), owner(ctx)
    extra = b"never uploaded %s\n" % ctx.serial_tag().encode()
    files = bundle_files() + [{"path": "prompts/extra.md", "sha256": sha(extra), "mode": 420}]
    c.upload_bundle()
    key = new_key()
    a = c.call("POST", "intents", body=c.intent(n.name, files=files), key=key)
    refused(ctx, a, 422, "objects_missing", "an unstored object")
    ctx.expect(a.json.get("missing") == [sha(extra)], "names exactly the missing object", a)
    ctx.expect(c.call("GET", "intents?node=" + n.name).json["intents"] == [], "nothing recorded")
    ctx.expect(c.call("PUT", "objects/" + sha(extra), raw=extra).status == 200, "upload")
    ctx.expect(c.call("POST", "intents", body=c.intent(n.name, files=files), key=key).status == 201,
               "a refused write did not use up its key")


@case(C, "work", "intent-invalid")
def _(ctx):
    """An unknown node, a spec or task not in the bundle, and a body without spec are each 422; nothing recorded."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    for over, what in (({"node": "no-such-node"}, "unknown node"), ({"spec": "other.yaml"}, "spec not in bundle"),
                       ({"task": "tasks/none"}, "task not in bundle"), ({"spec": None}, "no spec"),
                       ({"files": None}, "no files nor from_job")):
        body = c.intent(n.name, **over)
        refused(ctx, c.call("POST", "intents", body=body, key=new_key()), 422, "invalid_request", what)
    ctx.expect(c.call("GET", "intents?node=" + n.name).json["intents"] == [], "nothing recorded")


@case(C, "work", "intent-from-job")
def _(ctx):
    """An intent may name an earlier job's bundle (from_job) instead of listing files; the spec, stamped with the earlier
    intent, is restamped with the new one."""
    n, c = ctx.node(), owner(ctx)
    first = submitted(ctx, c, n)["jobs"][0]
    a = c.call("POST", "intents", body={"node": n.name, "spec": "spec.yaml", "task": "tasks/t",
                                        "from_job": first["job_id"]}, key=new_key())
    ctx.expect(a.status == 201 and a.json["jobs"][0]["job_id"] != first["job_id"], "a new job", a)
    if a.status == 201:
        again = n.job(a.json["jobs"][0]["job_id"])
        stamped_bundle(ctx, n, again, a.json["intent"]["intent_id"], n.job(first["job_id"])["files"])


@case(C, "work", "uploaded-missing-check")
def _(ctx):
    """Uploads default a missing check only when the spec names the task; existing checks stay exact."""
    n, c = ctx.node(), owner(ctx)
    uploaded = c.upload_bundle()
    no_check = b"#!/bin/sh\n# no check: the form's check was empty, so every run passes\nexit 0\n"
    for has_check, names_task in ((False, True), (True, True), (False, False)):
        files = [f for f in uploaded if has_check or f["path"] != "tasks/t/check"]
        if not names_task:
            spec = wire.SPEC.replace("tasks: [tasks/t]\n", "").encode()
            ctx.expect(c.call("PUT", "objects/" + sha(spec), raw=spec).status == 200, "upload taskless spec")
            files = [dict(f, sha256=sha(spec)) if f["path"] == "spec.yaml" else f for f in files]
        added = not has_check and names_task
        key = new_key()
        a = c.submit(n.name, files=files, meta={"keep": "yes"}, key=key)
        ctx.expect(a.status == 201, "uploaded intent accepted", a)
        if a.status != 201:
            continue
        expected_meta = {"keep": "yes", **({"check_added": "none"} if added else {})}
        ctx.expect(a.json["intent"]["meta"] == expected_meta, "intent carries default marker", a)
        job = c.call("GET", "jobs/" + a.json["jobs"][0]["job_id"]).json
        ctx.expect(job["meta"] == expected_meta, "job carries default marker", job)
        expected = files + ([{"path": "tasks/t/check", "sha256": sha(no_check),
                              "mode": 493}] if added else [])
        original = wire.SPEC if names_task else spec.decode()
        stamped_bundle(ctx, n, job, a.json["intent"]["intent_id"], expected, original=original)
        actual = {f["path"]: f for f in job["files"]}
        for f in expected:
            if f["path"] != "spec.yaml":
                ctx.expect(all(actual[f["path"]][k] == v for k, v in f.items()), "descriptor unchanged", job)
        if added or has_check:
            data = no_check if added else wire.JOB_FILES["tasks/t/check"][0]
            fetched = c.call("GET", "jobs/%s/files/tasks/t/check" % job["job_id"])
            ctx.expect(fetched.status == 200 and fetched.body == data, "exact check bytes", fetched)
            ctx.expect(actual["tasks/t/check"]["bytes"] == len(data), "exact check size", actual)
        replay = c.submit(n.name, files=files, meta={"keep": "yes"}, key=key)
        ctx.expect(replay.status == 200 and replay.json == a.json, "replay retains defaults", replay)


@case(C, "work", "from-job-missing-check")
def _(ctx):
    """Both from_job routes fill a missing check; existing checks and source jobs stay intact."""
    n, c = ctx.node(), owner(ctx)
    uploaded = c.upload_bundle()
    # Preserve a legacy source without a task list, so upload defaults leave it alone.
    spec = wire.SPEC.replace("tasks: [tasks/t]\n", "").encode()
    ctx.expect(c.call("PUT", "objects/" + sha(spec), raw=spec).status == 200, "upload legacy spec")
    uploaded = [dict(f, sha256=sha(spec)) if f["path"] == "spec.yaml" else f for f in uploaded]
    no_check = b"#!/bin/sh\n# no check: the form's check was empty, so every run passes\nexit 0\n"
    for has_check in (False, True):
        files = uploaded if has_check else [f for f in uploaded if f["path"] != "tasks/t/check"]
        first = c.submit(n.name, files=files, meta={"keep": "yes"}).json
        source = first["jobs"][0]
        iid = first["intent"]["intent_id"]
        original = c.call("GET", "jobs/" + source["job_id"]).json
        body = {"node": n.name, "spec": "spec.yaml", "task": "tasks/t/",
                "from_job": source["job_id"], "meta": {"keep": "yes"}}
        for route in ("intents", "intents/%s/jobs" % iid):
            key = new_key()
            a = c.call("POST", route, body=body, key=key)
            ctx.expect(a.status == 201, "from_job accepted", a)
            job = a.json["jobs"][0] if route == "intents" else a.json
            seen = c.call("GET", "jobs/" + job["job_id"]).json
            expected_meta = {"keep": "yes"} if has_check else {"keep": "yes", "check_added": "none"}
            ctx.expect(seen["meta"] == expected_meta, "only added checks get the marker", seen)
            if route == "intents":
                ctx.expect(a.json["intent"]["meta"] == expected_meta, "intent retains default marker", a)
            actual = {f["path"]: f for f in seen["files"]}
            for f in original["files"]:
                if f["path"] != "spec.yaml" or route != "intents":
                    ctx.expect(actual[f["path"]] == f, "existing file unchanged", actual)
            ctx.expect(len(actual) == len(original["files"]) + (not has_check), "only missing check added")
            if not has_check:
                check = actual["tasks/t/check"]
                ctx.expect(check["sha256"] == sha(no_check) and check["mode"] == 493
                           and check["bytes"] == len(no_check), "exact no-check descriptor", check)
                fetched = c.call("GET", "jobs/%s/files/tasks/t/check" % job["job_id"])
                ctx.expect(fetched.status == 200 and fetched.body == no_check, "exact no-check bytes", fetched)
            if has_check and route != "intents":
                ctx.expect(seen["bundle_sha256"] == original["bundle_sha256"], "existing bundle identical")
            replay = c.call("POST", route, body=body, key=key)
            ctx.expect(replay.status == 200 and replay.json == a.json, "replay retains job and marker", replay)
        ctx.expect(c.call("GET", "jobs/" + source["job_id"]).json == original, "source job unchanged")


@case(C, "work", "intent-spec-names-another")
def _(ctx):
    """The Director names the intent in the spec; a spec naming another intent (or any other value) is 422
    invalid_request with field "intent", and nothing is recorded."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    for other in ("fixture", "01M74BZ2Y1RWHBP0N1QDR3ZS62"):
        data = wire.SPEC.encode() + b"intent: " + other.encode() + b"\n"
        ctx.expect(c.call("PUT", "objects/" + sha(data), raw=data).status == 200, "PUT objects")
        files = [f if f["path"] != "spec.yaml" else dict(f, sha256=sha(data)) for f in bundle_files()]
        a = c.call("POST", "intents", body=c.intent(n.name, files=files), key=new_key())
        refused(ctx, a, 422, "invalid_request", "a spec naming " + other)
        ctx.expect((a.json or {}).get("field") == "intent" and other in (a.json or {}).get("detail", ""),
                   "the refusal names the field and the value", a)
    ctx.expect(c.call("GET", "intents?node=" + n.name).json["intents"] == [], "nothing recorded")


@case(C, "work", "intent-spec-bytes-kept")
def _(ctx):
    """The stamp edits the spec in place: a spec with comments, a block-style budget and quoted prose is offered as
    its bytes plus one `intent:` line, so the node reads what the client wrote."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    text = ("# written by hand\nid: dspec-kept  # keep this\nmodel: fake\nsystem: prompts/p.md\nbudget:\n"
            "  turns: 2   # a block mapping\n  tool_calls: 0\n  usd: 0.25\n  seconds: 60\ntasks:\n- tasks/t\n"
            "trials: 1\nhypothesis: 'fake: \"quoted\" prose'\n")
    data = text.encode()
    ctx.expect(c.call("PUT", "objects/" + sha(data), raw=data).status == 200, "PUT objects")
    files = [f if f["path"] != "spec.yaml" else dict(f, sha256=sha(data)) for f in bundle_files()]
    a = c.call("POST", "intents", body=c.intent(n.name, files=files), key=new_key())
    ctx.expect(a.status == 201, "POST intents is 201", a)
    if a.status == 201:
        stamped_bundle(ctx, n, n.job(a.json["jobs"][0]["job_id"]), a.json["intent"]["intent_id"], files, text)


@case(C, "work", "intent-spec-uneditable")
def _(ctx):
    """A spec the Director cannot edit in place (here an anchored budget) is 422 invalid_request naming why; nothing
    is recorded."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    data = wire.SPEC.replace("budget: {", "budget: &b {").encode()
    ctx.expect(c.call("PUT", "objects/" + sha(data), raw=data).status == 200, "PUT objects")
    files = [f if f["path"] != "spec.yaml" else dict(f, sha256=sha(data)) for f in bundle_files()]
    a = c.call("POST", "intents", body=c.intent(n.name, files=files), key=new_key())
    refused(ctx, a, 422, "invalid_request", "an anchored budget")
    ctx.expect("anchor" in (a.json or {}).get("detail", ""), "the refusal says why", a)
    ctx.expect(c.call("GET", "intents?node=" + n.name).json["intents"] == [], "nothing recorded")


# ---- §5 write rules ----
@case(C, "writes", "idempotency-key-required")
def _(ctx):
    """Every write names an Idempotency-Key: none is 428, a malformed one 422; neither records anything."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    refused(ctx, c.call("POST", "intents", body=c.intent(n.name)), 428, "idempotency_key_required", "no key")
    for bad in ("has space", "x" * 129, "semi;colon"):
        refused(ctx, c.call("POST", "intents", body=c.intent(n.name), key=bad), 422, "idempotency_key_invalid", repr(bad))
    ctx.expect(c.call("GET", "intents?node=" + n.name).json["intents"] == [], "nothing recorded")


@case(C, "writes", "idempotency-replay")
def _(ctx):
    """Same key, same content: 200 with the same record (marked Idempotent-Replayed), and only one record."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    key = new_key()
    first = c.submit(n.name, key=key)
    again = c.submit(n.name, key=key)
    ctx.expect(first.status == 201 and again.status == 200, "201 then 200", (first, again))
    ctx.expect(again.json == first.json, "the same record", (first.json, again.json))
    ctx.expect(again.header("idempotent-replayed") == "true" and first.header("idempotent-replayed") is None,
               "the replay is marked", again)
    ctx.expect(len(c.call("GET", "intents?node=" + n.name).json["intents"]) == 1, "one intent")


@case(C, "writes", "idempotency-normalised")
def _(ctx):
    """Content is compared after defaults: trials omitted and trials 1 are the same request."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    key = new_key()
    ctx.expect(c.submit(n.name, key=key).status == 201, "first")
    ctx.expect(c.submit(n.name, key=key, trials=1, meta={}).status == 200, "the same request with defaults spelt out")


@case(C, "writes", "idempotency-conflict")
def _(ctx):
    """Same key, different content is 409 idempotency_conflict, naming the record the key made; so is the key
    reused for another kind of write. Nothing is changed."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    key = new_key()
    made = c.submit(n.name, key=key).json
    a = c.submit(n.name, key=key, title="something else")
    refused(ctx, a, 409, "idempotency_conflict", "other content")
    ctx.expect(a.json.get("record") == {"kind": "intent", "id": made["intent"]["intent_id"]}, "names the record", a)
    job = made["jobs"][0]["job_id"]
    refused(ctx, c.call("POST", "jobs/%s/cancel" % job, body={}, key=key), 409, "idempotency_conflict", "another write")
    ctx.expect(c.call("GET", "jobs/" + job).json["cancel"] is None, "no cancel was recorded")
    ctx.expect(len(c.call("GET", "intents?node=" + n.name).json["intents"]) == 1, "one intent")


@case(C, "writes", "revisions")
def _(ctx):
    """Every record has a revision, and each change to a record gives it a higher one: the job when offered,
    accepted and finished; the run when it arrives and when it is decided."""
    n, c = ctx.node(), owner(ctx)
    got = submitted(ctx, c, n)
    intent, job = got["intent"], got["jobs"][0]
    ctx.expect(all(isinstance(revision(r), int) and revision(r) > 0 for r in (intent, job)), "positive revisions", got)
    revs = [revision(job)]
    accept(ctx, n, job["job_id"])
    revs.append(revision(c.call("GET", "jobs/" + job["job_id"]).json))
    finish(ctx, n, job["job_id"])
    done = c.call("GET", "jobs/" + job["job_id"]).json
    revs.append(revision(done))
    ctx.expect(revs == sorted(set(revs)), "the job's revision rises with each change", revs)
    run = c.call("GET", "runs/" + done["run_id"]).json
    ctx.expect(isinstance(revision(run), int), "the run has a revision", run)
    c.call("POST", "runs/%s/decision" % run["run_id"], body={"decision": "approve"}, key=new_key())
    ctx.expect(revision(c.call("GET", "runs/" + run["run_id"]).json) > revision(run), "deciding changes the run")
    ctx.expect(revision(c.call("GET", "intents/" + intent["intent_id"]).json["intent"]) == revision(intent),
               "an unchanged record keeps its revision")


@case(C, "work", "job-error-reason")
def _(ctx):
    """An errored job exposes structured meaning from its stored reason; a done job has error null."""
    n, c = ctx.node(), owner(ctx)
    got = submitted(ctx, c, n)
    job_id = got["jobs"][0]["job_id"]
    accept(ctx, n, job_id)
    why = "executor: gateway refused or unavailable: invalid executor or timeout"
    row = json.dumps({"state": "error", "director_job": job_id, "reason": why,
                      "stop": "error: no_row: " + why})
    term = {"envelope_version": 1, "event_id": "job_terminal:" + job_id, "job_id": job_id, "files": [],
            "envelope_sha256": sha(b""), "row_sha256": sha(row)}
    a = n.call("POST", "envelopes", body={"envelope": term, "row_line": row})
    ctx.expect((a.json or {}).get("status") == "acked", "job_terminal acked", a)
    job = c.call("GET", "jobs/" + job_id).json
    ctx.expect(job.get("state") == "error" and job.get("error") == why, "Client API carries the reason", job)
    ctx.expect((job.get("failure") or {}).get("code"), "and a classified failure", job)

    # stop when reason is absent
    got2 = submitted(ctx, c, n)
    j2 = got2["jobs"][0]["job_id"]
    accept(ctx, n, j2)
    row2 = json.dumps({"state": "failed", "director_job": j2, "stop": "check_error: Exec format error"})
    term2 = {"envelope_version": 1, "event_id": "job_terminal:" + j2, "job_id": j2, "files": [],
             "envelope_sha256": sha(b""), "row_sha256": sha(row2)}
    n.call("POST", "envelopes", body={"envelope": term2, "row_line": row2})
    job2 = c.call("GET", "jobs/" + j2).json
    ctx.expect(job2.get("error") == "check_error: Exec format error", "stop becomes error when reason is absent", job2)
    ctx.expect(job2.get("failure") == {"code": "check_error",
               "message": "Fix the task's check command before retrying.",
               "model_switch_may_help": False, "retryable": False, "detail": "check_error: Exec format error"},
               "check execution failure is classified, with the machine's words as detail", job2)

    got_check = submitted(ctx, c, n)
    check_id = got_check["jobs"][0]["job_id"]
    accept(ctx, n, check_id)
    row_check = json.dumps({"state": "error", "director_job": check_id, "stop": "check",
                            "reason": "The frozen task check returned 1"})
    env_check = {"envelope_version": 1, "event_id": "job_terminal:" + check_id, "job_id": check_id,
                 "files": [], "envelope_sha256": sha(b""), "row_sha256": sha(row_check)}
    ctx.expect(n.call("POST", "envelopes", body={"envelope": env_check, "row_line": row_check}).json["status"] == "acked",
               "check failure acked")
    check_job = c.call("GET", "jobs/" + check_id).json
    ctx.expect(check_job["failure"] == {"code": "check_failed", "message": "Fix the task's failing check before retrying.",
               "model_switch_may_help": False, "retryable": False, "detail": "The frozen task check returned 1"},
               "task check failure cannot benefit from a model switch", check_job)

    # a done job keeps error null
    got3 = submitted(ctx, c, n)
    j3 = got3["jobs"][0]["job_id"]
    accept(ctx, n, j3)
    finish(ctx, n, j3)
    done = c.call("GET", "jobs/" + j3).json
    ctx.expect(done.get("error") is None and done.get("failure") is None, "done: error and failure are null")


# ---- §4 jobs, runs and evidence ----
@case(C, "evidence", "run-usage-and-evidence")
def _(ctx):
    """After the node reports, the intent shows its job done and its run with usage; the run's files and the
    job's bundle files are fetched through the client API, byte for byte, never as a page."""
    n, c = ctx.node(), owner(ctx)
    got = submitted(ctx, c, n)
    intent_id, job_id = got["intent"]["intent_id"], got["jobs"][0]["job_id"]
    accept(ctx, n, job_id)
    trace = b'{"kind":"finalization","parent_id":"run"}\n{"kind":"run","outcome":"finalized"}\n'
    run = finish(ctx, n, job_id, files={"trace.jsonl": trace})
    run_id = run["envelope"]["run_id"]
    view = c.call("GET", "intents/" + intent_id).json
    ctx.expect(view["jobs"][0]["state"] == "done" and view["jobs"][0]["run_ids"] == [run_id], "the job is done", view)
    ctx.expect(len(view["runs"]) == 1 and view["runs"][0]["usage"] == wire.usage()
               and view["runs"][0]["intent_id"] == intent_id and view["runs"][0]["job_id"] == job_id,
               "its run, with usage", view)
    for path, data in run["files"].items():
        a = c.call("GET", "runs/%s/files/%s" % (run_id, path))
        ctx.expect(a.status == 200 and a.body == data, "run file %s" % path, a)
        ctx.expect(a.header("x-content-type-options") == "nosniff" and "html" not in (a.header("content-type") or ""),
                   "served as data, not a page", a)
        ctx.expect(c.call("GET", "objects/" + sha(data)).body == data, "the object by its hash")
    a = c.call("GET", "jobs/%s/files/tasks/t/goal.md" % job_id)
    ctx.expect(a.status == 200 and a.body == wire.JOB_FILES["tasks/t/goal.md"][0], "a bundle file", a)
    for path in ("runs/%s/files/nope.md" % run_id, "runs/%s/files/..%%2Fetc" % run_id, "objects/" + sha(b"unlisted")):
        refused(ctx, c.call("GET", path), 404, "not_found", path)


@case(C, "evidence", "objects-content-addressed")
def _(ctx):
    """PUT objects stores bytes only under their own hash; an uploaded object no record lists cannot be read back."""
    c = owner(ctx)
    data = b"loose %s" % ctx.serial_tag().encode()
    refused(ctx, c.call("PUT", "objects/" + sha(b"other"), raw=data), 422, "invalid_request", "wrong name")
    ctx.expect(c.call("PUT", "objects/" + sha(data), raw=data).json == {"sha256": sha(data), "stored": True}, "stored")
    ctx.expect(c.call("PUT", "objects/" + sha(data), raw=data).json["stored"] is False, "a repeat stores nothing")
    refused(ctx, c.call("GET", "objects/" + sha(data)), 404, "not_found", "listed by no record")


# ---- §6 decisions ----
@case(C, "decisions", "decision-receipt")
def _(ctx):
    """Approve records a receipt about one run's result that authorises nothing further; the run shows it;
    a replay answers the same receipt; a second decision is 409 already_decided."""
    n, c = ctx.node(), owner(ctx)
    job_id = submitted(ctx, c, n)["jobs"][0]["job_id"]
    accept(ctx, n, job_id)
    run_id = finish(ctx, n, job_id)["envelope"]["run_id"]
    key = new_key()
    a = c.call("POST", "runs/%s/decision" % run_id, body={"decision": "approve", "note": "looks right"}, key=key)
    ctx.expect(a.status == 201, "201", a)
    r = a.json
    ctx.expect(r["decision"] == "approve" and r["run_id"] == run_id and r["job_id"] == job_id and r["note"] == "looks right"
               and r["scope"] == "run_result" and r["authorises"] == [] and wire.UTC_RE.fullmatch(r["decided_at"])
               and isinstance(r["decision_id"], str) and isinstance(revision(r), int), "the receipt", r)
    ctx.expect(c.call("GET", "runs/%s/decision" % run_id).json == r, "GET runs/{id}/decision")
    ctx.expect(c.call("GET", "decisions/" + r["decision_id"]).json == r, "GET decisions/{id}")
    run = c.call("GET", "runs/" + run_id).json
    ctx.expect((run.get("decision") or {}).get("decision_id") == r["decision_id"], "the run shows it", run)
    replay = c.call("POST", "runs/%s/decision" % run_id, body={"decision": "approve", "note": "looks right"}, key=key)
    ctx.expect(replay.status == 200 and replay.json == r, "a replay is the same receipt", replay)
    a = c.call("POST", "runs/%s/decision" % run_id, body={"decision": "reject"}, key=new_key())
    refused(ctx, a, 409, "already_decided", "a second decision")
    ctx.expect((a.json.get("decision") or {}).get("decision_id") == r["decision_id"], "names the receipt", a)
    ctx.expect(n.call("GET", "jobs").json["jobs"] == [], "approval offers the node nothing new")


@case(C, "decisions", "decision-refusals")
def _(ctx):
    """Reject is the other decision; an unknown run is 404; any other word is 422; an undecided run has no receipt."""
    n, c = ctx.node(), owner(ctx)
    refused(ctx, c.call("POST", "runs/no-such-run/decision", body={"decision": "approve"}, key=new_key()), 404,
            "not_found", "unknown run")
    job_id = submitted(ctx, c, n)["jobs"][0]["job_id"]
    accept(ctx, n, job_id)
    run_id = finish(ctx, n, job_id)["envelope"]["run_id"]
    refused(ctx, c.call("GET", "runs/%s/decision" % run_id), 404, "not_found", "no decision yet")
    for word in ("approved", "", None):
        refused(ctx, c.call("POST", "runs/%s/decision" % run_id, body={"decision": word}, key=new_key()), 422,
                "invalid_request", repr(word))
    a = c.call("POST", "runs/%s/decision" % run_id, body={"decision": "reject"}, key=new_key())
    ctx.expect(a.status == 201 and a.json["decision"] == "reject", "reject", a)


@case(C, "decisions", "cancel-requested")
def _(ctx):
    """A cancel is recorded as requested and says it is not delivered; the node (protocol 1 here) still has the job.
    Asking again answers the same request; a finished job is 409 job_finished."""
    n, c = ctx.node(), owner(ctx)
    job_id = submitted(ctx, c, n)["jobs"][0]["job_id"]
    a = c.call("POST", "jobs/%s/cancel" % job_id, body={"reason": "changed my mind"}, key=new_key())
    ctx.expect(a.status == 202, "202", a)
    k = a.json
    ctx.expect(k["state"] == "requested" and k["delivered"] is False and isinstance(k["detail"], str)
               and k["job_id"] == job_id and isinstance(revision(k), int), "requested, not delivered", k)
    ctx.expect(c.call("GET", "cancels/" + k["cancel_id"]).json == k, "GET cancels/{id}")
    ctx.expect((c.call("GET", "jobs/" + job_id).json.get("cancel") or {}) == {"cancel_id": k["cancel_id"],
               "state": "requested", "outcome": None}, "the job shows it")
    ctx.expect(any(j["job_id"] == job_id for j in n.call("GET", "jobs").json["jobs"]), "the node still has the job")
    again = c.call("POST", "jobs/%s/cancel" % job_id, body={}, key=new_key())
    ctx.expect(again.status == 200 and again.json["cancel_id"] == k["cancel_id"], "one request per job", again)
    refused(ctx, c.call("POST", "jobs/no-such-job/cancel", body={}, key=new_key()), 404, "not_found", "unknown job")
    other = submitted(ctx, c, n)["jobs"][0]["job_id"]
    accept(ctx, n, other)
    finish(ctx, n, other)
    refused(ctx, c.call("POST", "jobs/%s/cancel" % other, body={}, key=new_key()), 409, "job_finished", "finished job")


# ---- §7 live updates ----
@case(C, "events", "events-hello-and-changes")
def _(ctx):
    """The stream opens with hello (the newest revision, no replay), then carries each change: kind, id,
    revision and the record as it is, with the revision as the event id."""
    n, c = ctx.node(), owner(ctx)
    s = c.stream()
    try:
        ctx.expect(s.status == 200 and (s.headers.get("content-type") or "").startswith("text/event-stream"),
                   "an event stream", s.headers)
        hello, _ = s.until(lambda name, _d: name == "hello")
        ctx.expect(hello and isinstance(hello[1].get("revision"), int) and hello[1].get("replay") is False, "hello", hello)
        got = submitted(ctx, c, n)
        job, intent = got["jobs"][0], got["intent"]
        for kind, record in (("job", job), ("intent", intent)):
            ident = record[kind + "_id"]
            e, _ = s.until(lambda name, d, k=kind, i=ident: name == "change" and d["kind"] == k and d["id"] == i)
            ctx.expect(e is not None, "a %s change" % kind)
            ctx.expect(e[1]["revision"] == revision(record) == e[1]["record"]["revision"] and e[2] == str(revision(record))
                       and e[1]["revision"] > hello[1]["revision"], "%s event revision" % kind, e)
            ctx.expect(e[1]["record"] == record, "the event carries the record", e)
    finally:
        s.close()


@case(C, "events", "events-reconnect-order")
def _(ctx):
    """The reconnect order: open the stream, hold events, refetch, apply held events by revision. A change made
    between hello and the refetch arrives held with a revision the refetch already shows (skip it); one made
    after arrives with a higher revision (apply it). Every event for a record has a rising revision."""
    n, c = ctx.node(), owner(ctx)
    job_id = submitted(ctx, c, n)["jobs"][0]["job_id"]
    s = c.stream()
    try:
        hello, _ = s.until(lambda name, _d: name == "hello")
        ctx.expect(hello is not None, "hello first")
        accept(ctx, n, job_id)                              # a change while the client holds events
        fetched = c.call("GET", "jobs/" + job_id).json      # the refetch
        finish(ctx, n, job_id)                              # a change after the refetch
        held = []
        while True:
            e = s.next(3)
            if e is None:
                break
            if e[0] == "change" and e[1]["kind"] == "job" and e[1]["id"] == job_id:
                held.append(e[1])
                if e[1]["record"]["state"] == "done":
                    break
        revs = [h["revision"] for h in held]
        ctx.expect(len(held) >= 2 and revs == sorted(set(revs)), "rising revisions", revs)
        ctx.expect(held[0]["revision"] <= revision(fetched), "the held change is already in the refetch", (revs, fetched))
        state = dict(fetched)
        for h in held:
            if h["revision"] > revision(state):
                state = h["record"]
        ctx.expect(state["state"] == "done" and revision(state) == revs[-1], "applying by revision ends current", state)
        ctx.expect(state == c.call("GET", "jobs/" + job_id).json, "and equals a fresh fetch")
    finally:
        s.close()
    late = c.stream()
    try:
        hello, _ = late.until(lambda name, _d: name == "hello")
        ctx.expect(hello and hello[1]["revision"] >= revs[-1], "a new stream starts at the present", hello)
        ctx.expect(late.next(1.5) is None, "older changes are not replayed")
    finally:
        late.close()


@case(C, "events", "events-admin-changes")
def _(ctx):
    """A change made by the Director's administrative command (a hand-made job) reaches open streams too."""
    n, c = ctx.node(), owner(ctx)
    s = c.stream()
    try:
        s.until(lambda name, _d: name == "hello")
        job_id = n.offer()
        e, _ = s.until(lambda name, d: name == "change" and d["id"] == job_id, timeout=5)
        ctx.expect(e is not None and e[1]["kind"] == "job" and e[1]["record"]["intent_id"] is None, "the job's change", e)
    finally:
        s.close()


# ---- §2 access ----
@case(C, "access", "device-does-the-journey")
def _(ctx):
    """A paired device submits, cancels and decides like the owner, and sees the same records."""
    n, o, d = ctx.node(), owner(ctx), device(ctx)
    got = submitted(ctx, d, n)
    intent_id, job_id = got["intent"]["intent_id"], got["jobs"][0]["job_id"]
    ctx.expect(got["intent"]["created_by"] == d.credential_id, "the device is named as the submitter", got)
    ctx.expect(o.call("GET", "intents/" + intent_id).json == d.call("GET", "intents/" + intent_id).json,
               "both see the same record")
    accept(ctx, n, job_id)
    run_id = finish(ctx, n, job_id)["envelope"]["run_id"]
    a = d.call("POST", "runs/%s/decision" % run_id, body={"decision": "approve"}, key=new_key())
    ctx.expect(a.status == 201 and a.json["decided_by"] == d.credential_id, "the device approves", a)
    other = submitted(ctx, d, n)["jobs"][0]["job_id"]
    ctx.expect(d.call("POST", "jobs/%s/cancel" % other, body={}, key=new_key()).status == 202, "the device cancels")


@case(C, "access", "owner-only-and-credentials")
def _(ctx):
    """A paired owner can issue codes, list credentials and revoke another device."""
    n, o, actor = ctx.node(), owner(ctx), device(ctx)
    ctx.expect(actor.call("GET", "me").json["owner"] is True, "a paired device acts for the owner")
    issued = actor.call("POST", "pairings", body={"name": "second-device"})
    ctx.expect(issued.status == 200, "device issues a pairing code")
    pending = actor.call("GET", "credentials").json["pairings"]
    audit = next(p for p in pending if p["name"] == "second-device")
    ctx.expect(audit["issued_by"] == actor.credential_id and audit["issued_at"]
               and audit["credential_id"] is None and issued.json["code"] not in json.dumps(pending),
               "pending pairing is attributed without exposing its code", audit)
    paired = wire.request(ctx.target.url, "POST", "/api/v1/pair", body={"code": issued.json["code"]})
    ctx.expect(paired.status == 200, "the code works for a second device")
    d = Client(ctx.target, paired.json["token"], paired.json["credential_id"])
    self_revoke = actor.call("DELETE", "credentials/" + actor.credential_id)
    refused(ctx, self_revoke, 409, "self_revoke", "device cannot revoke itself")
    ctx.expect("Forget this device" in self_revoke.json["detail"], "self-revocation explains local removal")
    for token, what in ((None, "no token"), (n.token, "a node token")):
        for method, path in (("GET", "intents"), ("POST", "intents"), ("GET", "events")):
            refused(ctx, o.call(method, path, body={}, key=new_key(), token=token), 401, "unauthenticated",
                    "%s on %s %s" % (what, method, path))
    s = d.stream()
    try:
        s.until(lambda name, _d: name == "hello")
        ctx.expect(actor.call("DELETE", "credentials/" + d.credential_id).status == 200, "the device revokes")
        e, _ = s.until(lambda name, _d: name == "revoked", timeout=5)
        ctx.expect(e is not None, "the stream says revoked")
        ctx.expect(s.next(2) is None and s.ended, "and ends")
    finally:
        s.close()
    refused(ctx, d.call("POST", "intents", body=d.intent(n.name), key=new_key()), 401, "unauthenticated", "revoked")
    listed = actor.call("GET", "credentials").json
    row = next(c for c in listed["credentials"] if c["id"] == d.credential_id)
    ctx.expect(row["revoked_by"] == actor.credential_id and row["revoked_at"], "revocation is attributed", row)
    pairing = next(p for p in listed["pairings"] if p["credential_id"] == d.credential_id)
    ctx.expect(pairing["issued_by"] == actor.credential_id and pairing["issued_at"] == audit["issued_at"]
               and pairing["used_at"], "pairing linked to the new credential", pairing)

    cli_target = device(ctx, "cli-target")
    stream = cli_target.stream()
    try:
        stream.until(lambda name, _: name == "hello")
        ctx.target.admin("credential", "revoke", cli_target.credential_id)
        event, _ = stream.until(lambda name, _: name == "revoked", timeout=5)
        ctx.expect(event is not None, "host CLI revocation reaches the running Director")
        ctx.expect(stream.next(2) is None and stream.ended, "the CLI-revoked stream ends")
    finally:
        stream.close()
    row = next(c for c in actor.call("GET", "credentials").json["credentials"]
               if c["id"] == cli_target.credential_id)
    ctx.expect(row["revoked_by"] == o.call("GET", "me").json["credential_id"],
               "host CLI revocation is attributed to the local credential", row)


@case(C, "access", "origin-and-transport-rules")
def _(ctx):
    """A browser request from an unlisted origin is 403 origin_refused before anything is recorded, writes and
    the event stream alike."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    for headers in ({"Origin": "https://evil.example"}, {"Sec-Fetch-Site": "cross-site"}):
        refused(ctx, c.call("POST", "intents", body=c.intent(n.name), key=new_key(), headers=headers), 403,
                "origin_refused", "a write from %s" % headers)
        refused(ctx, c.call("GET", "events", headers=headers), 403, "origin_refused", "the stream from %s" % headers)
    ctx.expect(c.call("GET", "intents?node=" + n.name).json["intents"] == [], "nothing recorded")


# ---- §2 hosted lead credentials ----
def lead(ctx, c, conversation, intent_id=None):
    body = {"name": "hosted-lead", "conversation_id": conversation["conversation_id"]}
    if intent_id is not None:
        body["intent_id"] = intent_id
    answer = c.call("POST", "credentials/lead", body=body)
    ctx.expect(answer.status == 201, "owner mints a lead credential", answer)
    ctx.expect(isinstance(answer.json.get("token"), str) and answer.json.get("credential_id"),
               "mint returns the token and credential id", answer)
    return Client(ctx.target, answer.json["token"], answer.json["credential_id"])


@case(C, "lead", "lead-mint-owner-only")
def _(ctx):
    """A paired owner mints a lead with an audited conversation and intent binding."""
    from cases.conversations import created
    n, o = ctx.node2(), owner(ctx)
    intent = submitted(ctx, o, n)["intent"]["intent_id"]
    conv = created(ctx, o, n, intent_id=intent)
    actor = device(ctx)
    l = lead(ctx, actor, conv, intent)
    ctx.expect(l.call("GET", "me").json["owner"] is False, "lead is not owner")
    credentials = o.call("GET", "credentials").json["credentials"]
    record = next(x for x in credentials if x["id"] == l.credential_id)
    ctx.expect(record["kind"] == "lead" and record["conversation_id"] == conv["conversation_id"]
               and record["intent_id"] == intent and record["created_by"] == actor.credential_id
               and record["created_at"] and l.token not in json.dumps(credentials),
               "binding is stored and token is never listed", record)
    refused(ctx, l.call("POST", "credentials/lead", body={"name": "escape", "conversation_id": conv["conversation_id"]}),
            403, "owner_only", "lead mint")
    for body in ({"name": "bad", "conversation_id": "missing"},
                 {"name": "bad", "conversation_id": conv["conversation_id"], "intent_id": "missing"},
                 {"conversation_id": conv["conversation_id"]}):
        refused(ctx, o.call("POST", "credentials/lead", body=body), 422, "invalid_request", "invalid binding")
    ctx.expect(o.call("DELETE", "credentials/" + l.credential_id).status == 200, "owner revokes a lead")
    refused(ctx, l.call("GET", "me"), 401, "unauthenticated", "revoked lead")


@case(C, "lead", "lead-allowed-work-and-evidence")
def _(ctx):
    """A lead uploads/submits, reads jobs/runs/output/events, decides and cancels, and reads its conversation."""
    from cases.conversations import created
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    l = lead(ctx, o, conv)
    got = submitted(ctx, l, n)
    iid, jid = got["intent"]["intent_id"], got["jobs"][0]["job_id"]
    accept(ctx, n, jid)
    run = finish(ctx, n, jid)
    rid = run["envelope"]["run_id"]
    for path in ("me", "nodes", "nodes/" + n.name, "usage", "intents", "intents/" + iid,
                 "jobs", "jobs/" + jid, "runs", "runs/" + rid, "runs/%s/events" % rid,
                 "conversations/" + conv["conversation_id"], "conversations/%s/thread" % conv["conversation_id"]):
        a = l.call("GET", path)
        ctx.expect(a.status == 200, "lead reads " + path, a)
    a = l.call("GET", "jobs/%s/files/tasks/t/goal.md" % jid)
    ctx.expect(a.status == 200 and a.body == wire.JOB_FILES["tasks/t/goal.md"][0], "lead reads job bundle", a)
    for path, data in run["files"].items():
        ctx.expect(l.call("GET", "runs/%s/files/%s" % (rid, path)).body == data, "lead reads run evidence")
        ctx.expect(l.call("GET", "objects/" + sha(data)).body == data, "lead reads listed objects")
    a = l.call("POST", "runs/%s/decision" % rid, body={"decision": "approve"}, key=new_key())
    ctx.expect(a.status == 201 and a.json["decided_by"] == l.credential_id, "lead decides", a)
    for path in ("runs/%s/decision" % rid, "decisions/" + a.json["decision_id"]):
        ctx.expect(l.call("GET", path).status == 200, "lead reads decision")
    jid = submitted(ctx, l, n)["jobs"][0]["job_id"]
    a = l.call("POST", "jobs/%s/cancel" % jid, body={}, key=new_key())
    ctx.expect(a.status == 202, "lead cancels", a)
    ctx.expect(l.call("GET", "cancels/" + a.json["cancel_id"]).status == 200, "lead reads cancel")


@case(C, "lead", "lead-refused-administration")
def _(ctx):
    """A lead cannot administer credentials, conversations, schedules or budgets, even on its bound conversation."""
    from cases.conversations import create_body, created
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    l = lead(ctx, o, conv)
    cid = conv["conversation_id"]
    project = o.call("POST", "projects", body={"name": "lead fixture"}, key=new_key()).json["project"]["project_id"]
    calls = [("POST", "pairings", {"name": "escape"}), ("GET", "credentials", None),
             ("DELETE", "credentials/" + l.credential_id, None),
             ("POST", "credentials/lead", {"name": "escape", "conversation_id": cid}),
             ("PUT", "lead-node", {"node": n.name}),
             ("PUT", "chief-of-staff", {"enabled": False}),
             ("PATCH", "conversations/" + cid, {"title": "escape"}),
             ("POST", "action-approvals/missing/decide", {"decision": "approve"}),
             ("PUT", "intents/missing/owner-messages/missing/mark", {"mark": "answer"}),
             ("POST", "conversations", create_body(o, n)),
             ("POST", "conversations/%s/turns" % cid, {"message": "escape"}),
             ("POST", "conversations/%s/stop" % cid, {}), ("POST", "conversations/%s/resume" % cid, {}),
             ("PUT", "conversations/%s/budget" % cid, {"usd": 1}),
             ("DELETE", "conversations/%s/budget" % cid, None),
             ("PUT", "projects/%s/budget" % project, {"usd": 1}),
             ("DELETE", "projects/%s/budget" % project, None),
             ("PUT", "source-budgets", {}), ("PUT", "source-budgets/fixture", {"usd": 1, "period": "day"}),
             ("DELETE", "source-budgets/fixture", None),
             ("POST", "schedules/%s/missing/changes" % n.name, {"enabled": False})]
    for method, path, body in calls:
        refused(ctx, l.call(method, path, body=body, key=new_key()), 403, "owner_only", method + " " + path)
    code = o.call("POST", "pairings", body={"name": "escape"}).json["code"]
    refused(ctx, wire.request(ctx.target.url, "POST", "/api/v1/pair", token=l.token, body={"code": code}),
            403, "owner_only", "lead cannot exchange a live pairing code")
    ctx.expect(l.call("GET", "me").status == 200, "refused revoke did not revoke the lead")


@case(C, "lead", "lead-intent-and-node-scope")
def _(ctx):
    """A scoped lead sees its attached intent plus self-submitted intents; unscoped means its node, never another."""
    from cases.conversations import created
    n, other, o = ctx.node2(), ctx.node2("other"), owner(ctx)
    attached = submitted(ctx, o, n)
    foreign = submitted(ctx, o, n)
    away = submitted(ctx, o, other)
    conv = created(ctx, o, n, intent_id=attached["intent"]["intent_id"])
    l = lead(ctx, o, conv, attached["intent"]["intent_id"])
    own = submitted(ctx, l, n)
    visible = {attached["jobs"][0]["job_id"], own["jobs"][0]["job_id"]}
    for query in ("", "?node=" + n.name):
        jobs = l.call("GET", "jobs" + query).json["jobs"]
        ctx.expect({j["job_id"] for j in jobs} == visible, "lists only attached and self-submitted jobs", jobs)
        intents = l.call("GET", "intents" + query).json
        ctx.expect({i["intent_id"] for i in intents["intents"]} ==
                   {attached["intent"]["intent_id"], own["intent"]["intent_id"]}, "intents filtered", intents)
    for record in (foreign, away):
        iid, jid = record["intent"]["intent_id"], record["jobs"][0]["job_id"]
        for path in ("intents/" + iid, "jobs/" + jid, "jobs/%s/files/tasks/t/goal.md" % jid):
            refused(ctx, l.call("GET", path), 403, "owner_only", "foreign " + path)
        refused(ctx, l.call("POST", "jobs/%s/cancel" % jid, body={}, key=new_key()),
                403, "owner_only", "foreign cancel")
        refused(ctx, l.call("POST", "intents", body={"node": n.name, "spec": "spec.yaml", "task": "tasks/t",
                                                     "from_job": jid}, key=new_key()),
                403, "owner_only", "foreign bundle reuse")
    jid = foreign["jobs"][0]["job_id"]
    accept(ctx, n, jid)
    run = finish(ctx, n, jid)
    rid = run["envelope"]["run_id"]
    for path in ("runs/" + rid, "runs/%s/events" % rid, "runs/%s/files/terminal.json" % rid):
        refused(ctx, l.call("GET", path), 403, "owner_only", "foreign run")
    refused(ctx, l.call("POST", "runs/%s/decision" % rid, body={"decision": "approve"}, key=new_key()),
            403, "owner_only", "foreign decision")
    ctx.expect(l.call("GET", "runs").json["runs"] == [], "foreign run absent from list")
    refused(ctx, l.call("GET", "objects/" + sha(run["files"]["terminal.json"])),
            403, "owner_only", "exclusive foreign evidence object")
    ctx.expect(l.call("GET", "usage?period=all").json["total"]["runs"] == 0,
               "usage excludes foreign runs")
    foreign_key = new_key("owner-write")
    fresh = o.submit(n.name, key=foreign_key)
    ctx.expect(fresh.status == 201, "owner creates replay fixture", fresh)
    refused(ctx, l.submit(n.name, key=foreign_key), 403, "owner_only", "out-of-scope idempotency replay")
    ctx.expect(l.call("GET", "intents?dispatch_id=" + foreign_key).json == {"intents": [], "jobs": []},
               "dispatch lookup does not expose foreign work")
    unscoped = lead(ctx, o, conv)
    expected = visible | {foreign["jobs"][0]["job_id"], fresh.json["jobs"][0]["job_id"]}
    ctx.expect({j["job_id"] for j in unscoped.call("GET", "jobs").json["jobs"]} == expected,
               "without intent binding all jobs on bound node are visible")
    refused(ctx, unscoped.submit(other.name), 403, "owner_only", "submit to another node")
    elsewhere = created(ctx, o, n)
    for path in ("conversations/" + elsewhere["conversation_id"], "conversations/%s/thread" % elsewhere["conversation_id"]):
        refused(ctx, unscoped.call("GET", path), 403, "owner_only", "another conversation")
    conflict_key = new_key("owner-conflict")
    o.submit(n.name, key=conflict_key)
    clash = l.submit(n.name, task="tasks/other-goal.md", key=conflict_key)
    refused(ctx, clash, 403, "owner_only", "out-of-scope idempotency conflict")
    ctx.expect("record" not in (clash.json or {}), "idempotency conflict does not name a foreign record", clash)


@case(C, "lead", "lead-events-scope")
def _(ctx):
    """Lead event streams carry allowed work and bound conversation changes, without unrelated records."""
    from cases.conversations import created, post
    from harness.node2 import delta
    n, other, o = ctx.node2(), ctx.node2("other-node"), owner(ctx)
    attached = submitted(ctx, o, n)
    conv = created(ctx, o, n, intent_id=attached["intent"]["intent_id"])
    l = lead(ctx, o, conv, attached["intent"]["intent_id"])
    s = l.stream()
    try:
        ctx.expect(s.status == 200, "lead opens global events")
        ctx.expect(s.until(lambda name, _d: name == "hello")[0] is not None, "lead receives hello")
        foreign = submitted(ctx, o, n)
        jid = foreign["jobs"][0]["job_id"]
        accept(ctx, n, jid)
        n.events("foreign-" + ctx.serial_tag(), jid, [delta(1, "private")])
        watched = attached["jobs"][0]["job_id"]
        accept(ctx, n, watched)
        event, seen = s.until(lambda name, d: name == "change" and d["id"] == watched)
        ctx.expect(event is not None, "allowed job change arrives")
        ctx.expect(all(d.get("id") not in (jid, foreign["intent"]["intent_id"])
                       for name, d, _ in seen if name == "change"), "foreign work never streamed", seen)
        ctx.expect(all(d.get("job_id") != jid for name, d, _ in seen if name == "run_event"),
                   "foreign live output never streamed", seen)
        rid = "allowed-" + ctx.serial_tag()
        ctx.expect(n.events(rid, watched, [delta(1, "visible")]).status == 200, "node sends allowed live output")
        event, seen = s.until(lambda name, d: name == "run_event" and d["run_id"] == rid)
        ctx.expect(event is not None and event[1]["payload"]["text"] == "visible", "allowed live output arrives", event)
        ctx.expect(all(d.get("job_id") != jid for name, d, _ in seen if name == "run_event"),
                   "foreign live output remains filtered", seen)
        ctx.expect(other.status([], vitals={"load": 0.1}).status == 200, "other node posts status")
        event, seen = s.until(lambda name, d: name == "change" and d.get("kind") == "node", timeout=10)
        ctx.expect(event is None, "foreign node changes never streamed")
        ctx.expect(all(not (name == "change" and d.get("kind") == "node" and d.get("id") == other.name)
                       for name, d, _ in seen if name == "change"), "only bound node may stream node changes", seen)
    finally:
        s.close()
    s = l.stream_for(conv["conversation_id"])
    try:
        ctx.expect(s.status == 200, "lead opens bound conversation stream")
        s.until(lambda name, _d: name == "hello")
        post(ctx, o, conv, "A new owner message.")
        event, _ = s.until(lambda name, d: name == "change" and d["kind"] == "conversation")
        ctx.expect(event is not None, "bound conversation changes arrive")
    finally:
        s.close()


@case(C, "lead", "lead-bundle-object-scope")
def _(ctx):
    """Naming a foreign object's hash cannot copy its bytes into lead work; uploading the bytes proves possession."""
    from cases.conversations import created
    n, o = ctx.node2(), owner(ctx)
    attached = submitted(ctx, o, n)
    foreign = submitted(ctx, o, n)["jobs"][0]["job_id"]
    accept(ctx, n, foreign)
    run = finish(ctx, n, foreign)
    secret = run["files"]["terminal.json"]
    digest = sha(secret)
    conv = created(ctx, o, n, intent_id=attached["intent"]["intent_id"])
    l = lead(ctx, o, conv, attached["intent"]["intent_id"])
    l.upload_bundle()
    files = bundle_files() + [{"path": "tasks/foreign.md", "sha256": digest, "mode": 420}]
    key = new_key("foreign-object")
    refused(ctx, l.submit(n.name, files=files, key=key), 403, "owner_only", "foreign object by hash alone")
    a = l.call("PUT", "objects/" + digest, raw=secret)
    ctx.expect(a.status == 200 and a.json["stored"] is False,
               "upload proves possession even when object already exists", a)
    a = l.submit(n.name, files=files, key=key)
    ctx.expect(a.status == 201, "same submission succeeds once the lead uploads those bytes", a)
    jid = a.json["jobs"][0]["job_id"]
    a = l.call("GET", "jobs/%s/files/tasks/foreign.md" % jid)
    ctx.expect(a.status == 200 and a.body == secret, "uploaded object is now part of allowed work", a)


@case(C, "lead", "lead-control-intent-provenance")
def _(ctx):
    """Lead control follows intent provenance; the owner retains authority over every job."""
    from cases.conversations import created, post
    n, o = ctx.node2(), owner(ctx)
    owner_work = submitted(ctx, o, n)
    conv = created(ctx, o, n, intent_id=owner_work["intent"]["intent_id"])
    first = lead(ctx, o, conv, owner_work["intent"]["intent_id"])
    unscoped = lead(ctx, o, conv)
    other = lead(ctx, o, created(ctx, o, n))
    other_work = submitted(ctx, other, n)
    own_work = submitted(ctx, first, n)
    # A second credential in the same conversation retains authority after rotation.
    rotated = lead(ctx, o, conv)
    for work in (owner_work, other_work):
        jid = work["jobs"][0]["job_id"]
        for actor in (first, unscoped):
            a = actor.call("POST", "jobs/%s/cancel" % jid, body={}, key=new_key())
            refused(ctx, a, 403, "owner_only", "lead cannot cancel foreign intent")
            ctx.expect("own intents" in a.json["detail"], "clear scope reason", a)
        accept(ctx, n, jid)
        rid = finish(ctx, n, jid, run_id="%s-%s" % (ctx.serial_tag(), jid[-8:]))["envelope"]["run_id"]
        for actor in (first, unscoped):
            for decision in ("approve", "reject"):
                refused(ctx, actor.call("POST", "runs/%s/decision" % rid,
                                       body={"decision": decision}, key=new_key()),
                        403, "owner_only", "lead cannot decide foreign intent")
        ctx.expect(o.call("POST", "runs/%s/decision" % rid, body={"decision": "approve"},
                          key=new_key()).status == 201, "owner decides foreign-to-lead work")
    jid = own_work["jobs"][0]["job_id"]
    accept(ctx, n, jid)
    rid = finish(ctx, n, jid, run_id="%s-%s" % (ctx.serial_tag(), jid[-8:]))["envelope"]["run_id"]
    ctx.expect(rotated.call("POST", "runs/%s/decision" % rid, body={"decision": "reject"},
                            key=new_key()).status == 201, "same conversation decides own lane")
    for creator in (o, first, other):
        jid = submitted(ctx, creator, n)["jobs"][0]["job_id"]
        ctx.expect(o.call("POST", "jobs/%s/cancel" % jid, body={}, key=new_key()).status == 202,
                   "owner cancels everywhere")
    jid = submitted(ctx, first, n)["jobs"][0]["job_id"]
    ctx.expect(first.call("POST", "jobs/%s/cancel" % jid, body={}, key=new_key()).status == 202,
               "lead cancels own lane")
    turn = post(ctx, o, conv, "create descendant")
    descendant = o.call("POST", "intents", body={"node": n.name, "spec": "spec.yaml",
                        "task": "tasks/turn", "from_job": turn["job_id"]}, key=new_key())
    ctx.expect(descendant.status == 201, "owner creates intent from conversation turn", descendant)
    jid = descendant.json["jobs"][0]["job_id"]
    ctx.expect(first.call("POST", "jobs/%s/cancel" % jid, body={}, key=new_key()).status == 202,
               "lead controls intent descended from its turn")
    child = o.call("POST", "intents", body={"node": n.name, "spec": "spec.yaml",
                   "task": "tasks/turn", "from_job": jid}, key=new_key())
    ctx.expect(child.status == 201, "owner creates descendant of descendant", child)
    jid = child.json["jobs"][0]["job_id"]
    ctx.expect(first.call("POST", "jobs/%s/cancel" % jid, body={}, key=new_key()).status == 202,
               "lead controls transitive turn descendant")


@case(C, "work", "intent-add-job")
def _(ctx):
    """Another attempt belongs to the same intent; replay never adds a third job."""
    n, c = ctx.node(), owner(ctx)
    first = submitted(ctx, c, n)
    iid = first["intent"]["intent_id"]
    path = "intents/%s/jobs" % iid
    body, key = c.intent(n.name), new_key("attempt")
    body["meta"] = {"model": "codex"}
    made = device(ctx).call("POST", path, body=body, key=key)
    ctx.expect(made.status == 201 and made.json.get("intent_id") == iid
               and made.json.get("job_id") != first["jobs"][0]["job_id"], "new job, same intent", made)
    again = c.call("POST", path, body=body, key=key)
    ctx.expect(again.status == 200 and again.json == made.json
               and again.header("idempotent-replayed") == "true", "idempotent replay", again)
    seen = c.call("GET", "intents/" + iid).json
    ctx.expect(len(seen["jobs"]) == 2 and seen["intent"]["title"] == first["intent"]["title"],
               "one task with two jobs", seen)
    refused(ctx, c.call("POST", path, body=dict(body, title="changed"), key=key),
            409, "idempotency_conflict", "changed replay")
    refused(ctx, c.call("POST", "intents/unknown/jobs", body=body, key=new_key()),
            404, "not_found", "unknown intent")
    refused(ctx, c.call("POST", path, body=dict(body, node="another-node"), key=new_key()),
            422, "invalid_request", "cannot move task to another node")
    for state in ("landed", "discarded"):
        closed = submitted(ctx, c, n, meta={"state": state})["intent"]["intent_id"]
        refused(ctx, c.call("POST", "intents/%s/jobs" % closed, body=body, key=new_key()),
                409, "intent_finished", state)


@case(C, "lead", "lead-retry-own-lane")
def _(ctx):
    """A lead may POST intents/{id}/jobs on its own lane; foreign intents stay owner_only."""
    from cases.conversations import created
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    foreign = submitted(ctx, o, n)
    l = lead(ctx, o, conv)
    own = submitted(ctx, l, n)
    iid = own["intent"]["intent_id"]
    jid = own["jobs"][0]["job_id"]
    accept(ctx, n, jid)
    why = "executor: stuck"
    row = json.dumps({"state": "error", "director_job": jid, "reason": why, "stop": "error: no_row: " + why})
    term = {"envelope_version": 1, "event_id": "job_terminal:" + jid, "job_id": jid, "files": [],
            "envelope_sha256": sha(b""), "row_sha256": sha(row)}
    a = n.call("POST", "envelopes", body={"envelope": term, "row_line": row})
    ctx.expect((a.json or {}).get("status") == "acked", "failed job acked", a)
    body = l.intent(n.name)
    body["meta"] = {"retry_of": jid, "attempt": "2"}
    path = "intents/%s/jobs" % iid
    retry = l.call("POST", path, body=body, key=new_key("lead-retry"))
    ctx.expect(retry.status == 201 and retry.json.get("intent_id") == iid
               and retry.json.get("job_id") != jid, "lead retries its own failed lane", retry)
    fjid = foreign["jobs"][0]["job_id"]
    refused(ctx, l.call("POST", "jobs/%s/cancel" % fjid, body={}, key=new_key()),
            403, "owner_only", "lead cannot cancel foreign intent on retry case")
    refused(ctx, l.call("POST", path.replace(iid, foreign["intent"]["intent_id"]), body=body, key=new_key()),
            403, "owner_only", "lead cannot retry foreign intent")


@case(C, "lead", "lead-conversation-link")
def lead_conversation_link(ctx):
    """lead_conversation_id stamps lead lanes, filters list and events, and cannot be spoofed."""
    from cases.conversations import created
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    other = created(ctx, o, n)
    l = lead(ctx, o, conv)
    l2 = lead(ctx, o, other)
    owner_lane = submitted(ctx, o, n)
    ctx.expect(owner_lane["intent"].get("lead_conversation_id") is None, "owner intent unstamped")
    meta_lane = submitted(ctx, l, n, meta={"conversation_id": "forged"})
    ctx.expect(meta_lane["intent"]["lead_conversation_id"] == conv["conversation_id"],
               "stamp comes from credential not meta")
    ctx.expect(meta_lane["jobs"][0]["lead_conversation_id"] == conv["conversation_id"],
               "job inherits intent stamp")
    own = submitted(ctx, l, n)
    away = submitted(ctx, l2, n)
    listed = o.call("GET", "intents?conversation=" + conv["conversation_id"]).json["intents"]
    ids = {i["intent_id"] for i in listed}
    ctx.expect(ids == {meta_lane["intent"]["intent_id"], own["intent"]["intent_id"]},
               "conversation filter lists only that lead's lanes", listed)
    ctx.expect(owner_lane["intent"]["intent_id"] not in ids, "owner lane excluded")
    ctx.expect(away["intent"]["intent_id"] not in ids, "other conversation lane excluded")
    refused(ctx, l.call("GET", "intents?conversation=" + other["conversation_id"]),
            403, "owner_only", "lead cannot filter another conversation")
    s = o.stream_for(conv["conversation_id"])
    try:
        s.until(lambda name, _d: name == "hello")
        watched = submitted(ctx, l, n)["jobs"][0]["job_id"]
        accept(ctx, n, watched)
        event, seen = s.until(lambda name, d: name == "change" and d.get("kind") == "job" and d.get("id") == watched)
        ctx.expect(event is not None, "job change for lead lane arrives on conversation stream", event)
        foreign = submitted(ctx, o, n)["jobs"][0]["job_id"]
        accept(ctx, n, foreign)
        ctx.expect(all(not (name == "change" and d.get("id") == foreign) for name, d, _ in seen),
                   "unstamped owner lane job changes stay off the stream", seen)
    finally:
        s.close()



@case(C, "conversation", "conversations-list-filters")
def conversations_list_filters(ctx):
    """GET conversations: limit, lead, project and existing filters in SQL; 422 on bad values; default unchanged."""
    from cases.conversations import created

    n, o = ctx.node2(), owner(ctx)
    pa = o.call("POST", "projects", body={"name": "conv-list-a"}, key=new_key()).json["project"]["project_id"]
    pb = o.call("POST", "projects", body={"name": "conv-list-b"}, key=new_key()).json["project"]["project_id"]
    c_a = created(ctx, o, n, project_id=pa)
    c_b = created(ctx, o, n, project_id=pb)
    c_plain = created(ctx, o, n)
    o.call("PUT", "lead-node", body={"node": n.name})
    lead = o.call(
        "POST",
        "conversations",
        body={"spec": "hosted-lead", "model": "claude-code-sonnet", "title": "lead"},
        key=new_key("lead-list"),
    )
    ctx.expect(lead.status == 201 and lead.json["conversation"].get("lead") is True, "hosted-lead conversation", lead)
    lead_c = lead.json["conversation"]

    default = o.call("GET", "conversations").json["conversations"]
    ours = {c["conversation_id"] for c in (c_a, c_b, c_plain, lead_c)}
    ctx.expect(ours <= {c["conversation_id"] for c in default}, "default list still includes new conversations", default)
    ctx.expect(default[0]["conversation_id"] == lead_c["conversation_id"], "newest first without filters", default)

    limited = o.call("GET", "conversations?limit=2").json["conversations"]
    ctx.expect(len(limited) == 2 and limited[0]["conversation_id"] == lead_c["conversation_id"], "limit newest first", limited)

    by_project = o.call("GET", "conversations?project=" + pa).json["conversations"]
    ctx.expect({c["conversation_id"] for c in by_project} == {c_a["conversation_id"]}, "project filter", by_project)

    lead_only = o.call("GET", "conversations?lead=1").json["conversations"]
    ctx.expect(all(c.get("lead") for c in lead_only), "lead=1 only hosted-lead conversations", lead_only)
    ctx.expect(lead_c["conversation_id"] in {c["conversation_id"] for c in lead_only}, "hosted lead included", lead_only)

    combined = o.call(
        "GET",
        "conversations?project=" + pb + "&limit=1&node=" + n.name,
    ).json["conversations"]
    ctx.expect(
        len(combined) == 1 and combined[0]["conversation_id"] == c_b["conversation_id"],
        "project, node and limit combine",
        combined,
    )

    refused(ctx, o.call("GET", "conversations?limit=0"), 422, "invalid_request", "limit below range")
    refused(ctx, o.call("GET", "conversations?limit=501"), 422, "invalid_request", "limit above range")
    refused(ctx, o.call("GET", "conversations?limit=nope"), 422, "invalid_request", "limit not an integer")
    refused(ctx, o.call("GET", "conversations?lead=0"), 422, "invalid_request", "lead not 1")
    refused(ctx, o.call("GET", "conversations?project=prj_missing"), 422, "invalid_request", "unknown project")


@case(C, "work", "intents-list-project-filter")
def intents_list_project_filter(ctx):
    """GET intents: project= filters in SQL and combines with limit and lanes."""
    n, o = ctx.node2(), owner(ctx)
    pa = o.call("POST", "projects", body={"name": "intent-list-a"}, key=new_key()).json["project"]["project_id"]
    pb = o.call("POST", "projects", body={"name": "intent-list-b"}, key=new_key()).json["project"]["project_id"]
    o.upload_bundle()
    for _ in range(3):
        ctx.expect(
            o.submit(n.name, project_id=pa, key=new_key("p-a")).status == 201,
            "intent in project a",
        )
    ctx.expect(o.submit(n.name, project_id=pb, key=new_key("p-b")).status == 201, "intent in project b")
    lane = o.submit(
        n.name,
        project_id=pa,
        meta={"source": "client-web-lane", "lane": "lane/x"},
        key=new_key("lane"),
    )
    ctx.expect(lane.status == 201, "lane intent in project a", lane)

    filtered = o.call("GET", "intents?project=" + pa).json["intents"]
    ctx.expect(len(filtered) == 4, "project filter keeps only that project's intents", filtered)
    ctx.expect(all(i.get("project_id") == pa for i in filtered), "every intent names the project", filtered)

    limited = o.call("GET", "intents?project=" + pa + "&limit=2").json["intents"]
    ctx.expect(len(limited) == 2, "limit after project filter", limited)

    laned = o.call("GET", "intents?project=" + pa + "&lanes=1&limit=1").json["intents"]
    ctx.expect(len(laned) == 1 and laned[0]["intent_id"] == lane.json["intent"]["intent_id"],
               "lanes and limit combine with project", laned)

    refused(ctx, o.call("GET", "intents?project=prj_missing"), 422, "invalid_request", "unknown project")


@case(C, "work", "intents-lane-flag")
def intents_lane_flag(ctx):
    """Every intent carries derived lane; lanes=1 returns exactly the lane:true intents."""
    n, o = ctx.node2(), owner(ctx)
    o.upload_bundle()
    lane = o.submit(n.name, meta={"source": "client-web-lane"}, key=new_key("lane"))
    plain = o.submit(n.name, meta={"source": "cs-cli"}, key=new_key("plain"))
    ctx.expect(lane.status == 201 and plain.status == 201, "lane and plain intents created", (lane, plain))
    lane_id = lane.json["intent"]["intent_id"]
    plain_id = plain.json["intent"]["intent_id"]
    ctx.expect(lane.json["intent"].get("lane") is True, "POST lane intent has lane true", lane)
    ctx.expect(plain.json["intent"].get("lane") is False, "POST plain intent has lane false", plain)
    got = o.call("GET", "intents/" + lane_id).json["intent"]
    ctx.expect(got.get("lane") is True, "GET lane intent has lane true", got)
    got = o.call("GET", "intents/" + plain_id).json["intent"]
    ctx.expect(got.get("lane") is False, "GET plain intent has lane false", got)
    listed = o.call("GET", "intents").json["intents"]
    by_id = {i["intent_id"]: i for i in listed}
    ctx.expect(by_id[lane_id].get("lane") is True, "list marks lane intent", by_id[lane_id])
    ctx.expect(by_id[plain_id].get("lane") is False, "list marks plain intent", by_id[plain_id])
    laned = o.call("GET", "intents?lanes=1").json["intents"]
    ctx.expect(all(i.get("lane") is True for i in laned), "lanes=1 rows all have lane true", laned)
    ctx.expect({i["intent_id"] for i in laned} == {i["intent_id"] for i in listed if i.get("lane")},
               "lanes=1 returns exactly lane:true intents", (laned, listed))


@case(C, "conversation", "conversation-archive-delete")
def conversation_archive_delete(ctx):
    from cases.conversations import created, post, reply
    n, o = ctx.node2(), owner(ctx)
    project = o.call("POST", "projects", body={"name": "archive"}, key=new_key()).json["project"]["project_id"]
    conv = created(ctx, o, n, project_id=project)
    cid = conv["conversation_id"]
    path = "conversations/" + cid
    l = lead(ctx, o, conv)
    lane = submitted(ctx, l, n)
    t = post(ctx, o, conv, "busy")
    refused(ctx, o.call("DELETE", path, key=new_key()), 409, "busy", "busy delete")
    refused(ctx, l.call("DELETE", path, key=new_key()), 403, "owner_only", "lead delete")
    reply(ctx, n, t["job_id"], "done")
    before = o.call("GET", path).json["conversation"]
    key = new_key()
    a = o.call("POST", path + "/archive", body={}, key=key)
    archived = a.json["conversation"]
    ctx.expect(a.status == 200 and archived["archived_at"] and archived["revision"] > before["revision"], "archive bumps revision", a)
    for k in (key, new_key()):
        again = o.call("POST", path + "/archive", body={}, key=k)
        ctx.expect(again.status == 200 and again.json["conversation"] == archived, "archive idempotent", again)
    for query, includes in (("", False), ("?archived=1", True), ("?archived=all", True)):
        listed = o.call("GET", "conversations" + query).json["conversations"]
        ctx.expect((cid in {c["conversation_id"] for c in listed}) == includes, "archive list " + query, listed)
    listed = o.call("GET", "conversations?archived=1&limit=1&project=" + project + "&node=" + n.name).json["conversations"]
    ctx.expect(len(listed) == 1 and listed[0]["conversation_id"] == cid, "archive combines filters before limit", listed)
    for value in ("0", "true", "", "2"):
        refused(ctx, o.call("GET", "conversations?archived=" + value), 422, "invalid_request", "bad archived")
    ctx.expect(o.call("GET", path + "/thread").status == 200, "archived thread readable")
    refused(ctx, o.call("POST", path + "/turns", body={"message": "no"}, key=new_key()), 409, "archived", "archived turn")
    refused(ctx, l.call("POST", path + "/wake-turns", body={"nudge": "wake", "reply": "no"}, key=new_key()), 409, "archived", "archived wake")
    ctx.expect(l.call("GET", "me").status == 200, "archive preserves credential")
    key = new_key()
    a = o.call("POST", path + "/unarchive", body={}, key=key)
    unarchived = a.json["conversation"]
    ctx.expect(a.status == 200 and unarchived["archived_at"] is None and unarchived["revision"] > archived["revision"], "unarchive bumps revision", a)
    for k in (key, new_key()):
        ctx.expect(o.call("POST", path + "/unarchive", body={}, key=k).json["conversation"] == unarchived, "unarchive idempotent")
    s = o.stream_for(cid)
    try:
        s.until(lambda name, _d: name == "hello")
        key = new_key()
        a = o.call("DELETE", path, key=key)
        ctx.expect(a.status == 200 and a.json == {"deleted": cid}, "deleted", a)
        event, _seen = s.until(lambda name, d: name == "change" and d.get("id") == cid and d.get("deleted"))
        ctx.expect(event is not None and "record" not in event[1], "deleted change event", event)
    finally:
        s.close()
    for k in (key, new_key()):
        refused(ctx, o.call("DELETE", path, key=k), 404, "not_found", "second delete")
    for target in (path, path + "/thread", "turns/" + t["turn_id"]):
        ctx.expect(o.call("GET", target).status == 404, "deleted record absent " + target)
    ctx.expect(cid not in {c["conversation_id"] for c in o.call("GET", "conversations?archived=all").json["conversations"]}, "resync omits deleted id")
    intent = o.call("GET", "intents/" + lane["intent"]["intent_id"]).json["intent"]
    ctx.expect(intent.get("lead_conversation_id") is None and not intent["conversation_ids"], "intent kept without conversation", intent)
    job = o.call("GET", "jobs/" + lane["jobs"][0]["job_id"]).json
    ctx.expect(job.get("conversation_id") is None and job.get("lead_conversation_id") is None, "job retained unlinked", job)
    ctx.expect(l.call("GET", "me").status == 401, "lead revoked")


@case(C, "work", "intent-builtin-lane")
def builtin_lane(ctx):
    """The Director builds the same lane bundle for any client; profiles own defaults."""
    n, c = ctx.node(), owner(ctx)
    for agent in (None, "coder", "reviewer"):
        form = {"title": "Hello WORLD!", "goal": "Make it work.\n", "repo": "project", "lane": "hello-world"}
        if agent:
            form["agent"] = agent
        key = new_key()
        body = {"node": n.name, "lane": form}
        a = c.call("POST", "intents", body=body, key=key)
        ctx.expect(a.status == 201, "built-in intent created", a)
        if a.status != 201:
            continue
        intent, job = a.json["intent"], a.json["jobs"][0]
        prefix = "jobs/%s/files/" % job["job_id"]
        get = lambda path: c.call("GET", prefix + path).body.decode()
        spec = get("spec.yaml")
        model, turns, seconds = ("cursor-codex", 150, 1800) if agent == "reviewer" else ("cursor-sol", 300, 3600)
        ctx.expect("model: %s\n" % model in spec and "turns: %d" % turns in spec and "seconds: %d" % seconds in spec,
                   "model and budget defaults", spec)
        ctx.expect("repo: project\n" in spec and "lane: hello-world\n" in spec
                   and "tasks/hello-world" in spec, "spec workspace and task", spec)
        ctx.expect(get("tasks/hello-world/goal.md") == form["goal"], "goal preserved")
        ctx.expect(get("tasks/hello-world/check").startswith("#!/bin/sh\n")
                   and "exit 0" in get("tasks/hello-world/check"), "default executable check")
        prompt = get("prompts/system.md")
        expected_prompt = {None: "worker on one lane", "coder": "coding worker", "reviewer": "code reviewer"}[agent]
        ctx.expect(expected_prompt in prompt, "profile or default prompt", prompt)
        if agent == "reviewer":
            ctx.expect("read-only" in get("tasks/hello-world/check"), "read-only guard")
        if agent:
            ctx.expect(intent["meta"].get("agent") == agent and len(intent["meta"].get("agent_sha256", "")) == 64,
                       "profile identity and digest", intent)
        replay = c.call("POST", "intents", body=body, key=key)
        ctx.expect(replay.status == 200 and replay.json["intent"]["intent_id"] == intent["intent_id"],
                   "built-in idempotency", replay)
    for over in ({"repo": "../bad", "lane": "good"}, {"repo": "good", "lane": "Bad"},
                 {"repo": "good"}, {"repo": "good", "lane": "a" * 64}):
        form = dict(title="Bad", goal="No work", **over)
        a = c.call("POST", "intents", body={"node": n.name, "lane": form}, key=new_key())
        ctx.expect(a.status == 422, "invalid workspace slug refused", a)
    for invalid in (0, -1, 3601, True, 1.5, "30", None):
        form = dict(title="Bad window", goal="No work", check_window=invalid)
        a = c.call("POST", "intents", body={"node": n.name, "lane": form}, key=new_key())
        ctx.expect(a.status == 422, "invalid check_window refused", a)
    form = dict(title="!! Hello WORLD !!", goal="x", check="echo yes", check_window=30)
    a = c.call("POST", "intents", body={"node": n.name, "lane": form}, key=new_key())
    ctx.expect(a.status == 201 and a.json["intent"]["task"] == "tasks/hello-world", "title slug for scratch lane", a)
    if a.status == 201:
        job = a.json["jobs"][0]["job_id"]
        spec = c.call("GET", "jobs/%s/files/spec.yaml" % job).body
        ctx.expect(b"check_window: 30\n" in spec, "check_window included in job spec", spec)
        check = c.call("GET", "jobs/%s/files/tasks/hello-world/check" % job).body
        ctx.expect(check == b"#!/bin/sh\necho yes\n", "check body wrapped as executable script", check)


@case(C, "work", "agent-profiles")
def agent_profiles(ctx):
    """Profiles are readable by every client, including hosted leads."""
    from cases.conversations import created
    n, o = ctx.node(), owner(ctx)
    l = lead(ctx, o, created(ctx, o, n))
    for c in (o, device(ctx), l):
        answer = c.call("GET", "agents")
        ctx.expect(answer.status == 200, "client lists profiles", answer)
        rows = answer.json["agents"]
        ctx.expect(any(p["name"] == "coder" for p in rows), "shipped coder listed", answer)
        for row in rows:
            ctx.expect(set(row) == {"name", "description", "model", "fallback", "writes", "lane", "check", "budget", "task_type", "sha256"}, "public profile fields", row)
            ctx.expect(isinstance(row["writes"], bool) and isinstance(row["lane"], bool)
                       and len(row["sha256"]) == 64, "profile types and digest", row)
            detail = c.call("GET", "agents/" + row["name"])
            ctx.expect(detail.status == 200 and detail.json == row, "detail equals listing", detail)
        refused(ctx, c.call("GET", "agents/no-such-agent"), 404, "not_found", "unknown profile")


@case(C, "writes", "dispatch-id-recovery")
def _(ctx):
    """A lost response can be recovered or retried using the stable dispatch ID."""
    n, c = ctx.node(), owner(ctx)
    c.upload_bundle()
    dispatch = "dispatch:" + new_key("dispatch")
    body = c.intent(n.name, dispatch_id=dispatch)
    # Read only acceptance headers, then drop the socket without reading the IDs.
    import http.client
    import urllib.parse
    url = urllib.parse.urlsplit(c.target.url)
    conn = http.client.HTTPConnection(url.hostname, url.port, timeout=10)
    conn.request("POST", "/api/v1/client/intents", body=json.dumps(body),
                 headers={"Authorization": "Bearer " + c.token, "Content-Type": "application/json"})
    response = conn.getresponse()
    ctx.expect(response.status == 201, "accepted before response dropped")
    response.close()
    conn.close()
    found = c.call("GET", "intents?dispatch_id=" + urllib.parse.quote(dispatch, safe="")).json
    again = c.call("POST", "intents", body=body, key=new_key())
    ctx.expect(again.status == 200, "dispatch overrides a fresh transport key", again)
    ctx.expect(found["intents"][0]["intent_id"] == again.json["intent"]["intent_id"]
               and found["jobs"][0]["job_id"] == again.json["jobs"][0]["job_id"], "original IDs recovered")
    refused(ctx, c.call("POST", "intents", body=dict(body, title="different")),
            409, "idempotency_conflict", "different body")
    ctx.expect(len(c.call("GET", "intents?node=" + n.name).json["intents"]) == 1, "exactly one task")
    ctx.expect(c.call("GET", "intents?dispatch_id=missing").json == {"intents": [], "jobs": []}, "unknown ID")
    for bad in ("", "has space", "x" * 129, 12, None):
        ctx.expect(c.call("POST", "intents", body=dict(body, dispatch_id=bad)).status == 422, "invalid dispatch rejected")
    c.submit(n.name)
    c.submit(n.name)
    ctx.expect(len(c.call("GET", "intents?node=" + n.name).json["intents"]) == 3, "omitting dispatch creates tasks")


@case(C, "work", "lane-first-run-capabilities")
def lane_first_run_capabilities(ctx):
    """The first offer freezes an installed fallback and a non-colliding lane name."""
    n, c = ctx.node2(), owner(ctx)
    form = {"title": "Hello", "goal": "Do it", "repo": "hello", "lane": "hello", "agent": "coder"}
    caps = {"templates": ["codex", "claude-code"], "preferred": "claude-code"}
    answer = n.call("POST", "status", body={"active": [], "agent_capabilities": caps})
    ctx.expect(answer.status == 200, "node reports ready agents", answer)
    key = new_key()
    request = {"node": n.name, "lane": form}
    answer = c.call("POST", "intents", body=request, key=key)
    ctx.expect(answer.status == 201, "missing Cursor uses a ready fallback", answer)
    if answer.status != 201:
        return
    job = answer.json["jobs"][0]
    ctx.expect(job["meta"]["model"] == "claude-code" and job["meta"]["lane"] == "hello-1",
               "offer reports actual executor and renamed lane", job)
    spec = c.call("GET", "jobs/%s/files/spec.yaml" % job["job_id"]).body.decode()
    ctx.expect("model: claude-code\n" in spec and "lane: hello-1\n" in spec,
               "frozen spec uses selected executor and lane", spec)
    n.call("POST", "status", body={"active": [], "agent_capabilities": {"templates": []}})
    replay = c.call("POST", "intents", body=request, key=key)
    ctx.expect(replay.status == 200 and replay.json["jobs"][0]["job_id"] == job["job_id"],
               "replay preserves the original frozen offer", replay)
    refusal = c.call("POST", "intents", body=request, key=new_key())
    ctx.expect(refusal.status == 422 and "no installed, signed-in agent" in refusal.body.decode(),
               "no ready fallback gives an actionable refusal", refusal)
