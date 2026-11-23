"""Project decision log API and real stdio lead tools against disposable Directors."""
import json
from pathlib import Path
import subprocess
import sys

from cases.budgets import end_job, project
from cases.client import device, lead, owner, refused
from cases.conversations import created
from harness.client import Client, new_key
from harness.run import case
from harness.targets import RustTarget

SPEC = Path(__file__).resolve().parents[1]
REPO = SPEC.parent.parent
AREA = "decision-log"
CONTENT = {
    "decision": "Use the Director",
    "why": "A shared record prevents drift.",
    "alternatives": [{"option": "Client state", "why_not": "Offline copies drift."}],
    "reopen_when": ["Offline operation is required."],
    "sources": ["docs/adr/0012.md", "2026-11-15", "https://example.com/adr"],
}


def post(client, path, body, key=None):
    return client.call("POST", path, body=body, key=key or new_key())


def log_path(ctx, client):
    project_id = project(ctx, client, "Decision log")["project_id"]
    return project_id, "projects/" + project_id + "/decision_log"


@case("client", AREA, "decision-log-lifecycle")
def _(ctx):
    o = owner(ctx)
    _, path = log_path(ctx, o)
    key = new_key()
    a = post(o, path, CONTENT, key)
    ctx.expect(a.status == 201, "create entry", a)
    first = a.json["entry"]
    eid = first["entry_id"]
    route = path + "/" + eid
    ctx.expect(eid.startswith("dle_") and first["number"] == "D1" and first["status"] == "active"
               and first["created_by"] == first["updated_by"] and first["created_at"] == first["updated_at"]
               and all(first[k] == v for k, v in CONTENT.items()), "stored fields and authorship", a)
    replay = post(o, path, CONTENT, key)
    ctx.expect(replay.status == 200 and replay.json["entry"] == first, "idempotency replay", replay)
    refused(ctx, post(o, path, {**CONTENT, "why": "another"}, key), 409, "idempotency_conflict", "key reuse")
    refused(ctx, post(o, path, {**CONTENT, "decision": "two\nlines"}), 422, "invalid_request", "one-line choice")
    refused(ctx, post(o, path, {**CONTENT, "alternatives": [{"option": "partial"}]}),
            422, "invalid_request", "alternatives require reasons")
    refused(ctx, o.call("POST", path, body=CONTENT), 428, "idempotency_key_required", "key required")

    paired = device(ctx)
    read = paired.call("GET", route)
    ctx.expect(read.status == 200 and read.json["entry"] == first and read.json["revisions"] == [],
               "paired device reads shared ownership record", read)
    rev = post(paired, route + "/revise", {"revision": first["revision"], "why": "Revise the wording."})
    current = rev.json["entry"]
    ctx.expect(rev.status == 200 and rev.json["revisions"] == [first] and current["why"] == "Revise the wording."
               and current["alternatives"] == first["alternatives"] and current["updated_by"] != first["created_by"],
               "partial revision preserves original fields and previous version", rev)
    refused(ctx, post(o, route + "/revise", {"revision": first["revision"], "why": "stale"}),
            409, "conflict", "stale revisions cannot overwrite changes")
    found = o.call("GET", path + "?status=active&q=OFFLINE+COPIES")
    ctx.expect(found.json["entries"] == [current] and "Client state — Offline copies drift." in found.json["markdown"],
               "search includes rejected alternatives", found)

    replacement = post(o, path, {**CONTENT, "decision": "Replace the shared record"}).json["entry"]
    ctx.expect(replacement["number"] == "D2", "project allocates the next number")
    replacement_id = replacement["entry_id"]
    _, other_path = log_path(ctx, o)
    foreign = post(o, other_path, CONTENT).json["entry"]
    ctx.expect(foreign["number"] == "D1", "separate project numbering")
    refused(ctx, o.call("GET", other_path + "/" + eid), 404, "not_found", "entry id belongs to its project")
    refused(ctx, post(o, route + "/supersede", {"revision": current["revision"], "superseded_by": foreign["entry_id"]}),
            404, "not_found", "replacement belongs to the same project")
    sup = post(o, route + "/supersede", {"revision": current["revision"], "superseded_by": replacement_id})
    ctx.expect(sup.status == 200 and sup.json["entry"]["superseded_by"] == replacement_id
               and sup.json["entry"]["status"] == "superseded" and sup.json["revisions"] == [first, current],
               "supersession retains history and links the replacement", sup)
    retired = post(o, path + "/" + replacement_id + "/retire", {"revision": replacement["revision"]})
    ctx.expect(retired.status == 200 and retired.json["entry"]["status"] == "retired", "retire an obsolete entry", retired)
    for status, num in (("superseded", "D1"), ("retired", "D2")):
        filtered = o.call("GET", path + "?status=" + status)
        ctx.expect([e["number"] for e in filtered.json["entries"]] == [num], "filter by " + status, filtered)
    refused(ctx, o.call("DELETE", route), 404, "not_found", "no hard delete")
    ctx.expect(o.call("GET", route).json == sup.json, "terminal entry is still readable")


@case("client", AREA, "decision-log-import")
def _(ctx):
    o = owner(ctx)
    _, path = log_path(ctx, o)
    markdown = (SPEC / "fixtures/decision-log.md").read_text()
    # Duplicate the last row inside a fresh import. The whole transaction must roll back.
    refused(ctx, post(o, path + "/import", {"markdown": markdown + markdown.splitlines()[-1] + "\n"}),
            409, "conflict", "duplicate number aborts import")
    ctx.expect(o.call("GET", path).json["entries"] == [], "no partial import")
    key = new_key()
    imported = post(o, path + "/import", {"markdown": markdown}, key)
    ctx.expect(imported.status == 201, "import fixture", imported)
    entries = imported.json["entries"]
    ctx.expect([e["number"] for e in entries] == ["D1", "D2", "D3", "D5", "D12"]
               and all(e["alternatives"] == [] and e["status"] == "active" for e in entries),
               "gaps and empty alternatives preserved", imported)
    ctx.expect(entries[0]["sources"] == ["memory 2026-11-15", "ADR 0004"]
               and "`contextstack/contextstack`" in entries[0]["decision"]
               and entries[3]["reopen_when"] == [] and entries[4]["reopen_when"][0].endswith("due now)"),
               "preserve Markdown, dates, sources and reopen conditions", imported)
    ctx.expect(post(o, path + "/import", {"markdown": markdown}, key).json == imported.json, "import replay")
    ctx.expect(post(o, path, CONTENT).json["entry"]["number"] == "D13", "numbering continues after import")
    refused(ctx, post(o, path + "/import", {"markdown": "not a table"}), 422, "invalid_request", "invalid table")


@case("client", AREA, "decision-log-ownership")
def _(ctx):
    o, node = owner(ctx), ctx.node2()
    pid, path = log_path(ctx, o)
    other_pid, other_path = log_path(ctx, o)
    first = post(o, path, CONTENT).json["entry"]
    route = path + "/" + first["entry_id"]
    bound = lead(ctx, o, created(ctx, o, node, project_id=pid))
    unbound = lead(ctx, o, created(ctx, o, node))
    different = lead(ctx, o, created(ctx, o, node, project_id=other_pid))
    ctx.expect(bound.call("GET", route).status == 200 and post(bound, path, CONTENT).status == 201,
               "lead can read and record on its own conversation's project")
    for outsider in (unbound, different):
        for method, target, body in (
            ("GET", path, None), ("GET", route, None), ("POST", path, CONTENT),
            ("POST", route + "/revise", {"revision": first["revision"], "why": "intrusion"}),
            ("POST", route + "/supersede", {"revision": first["revision"], "superseded_by": first["entry_id"]}),
            ("POST", route + "/retire", {"revision": first["revision"]}),
            ("POST", path + "/import", {"markdown": ""}),
        ):
            refused(ctx, outsider.call(method, target, body=body, key=new_key()), 403, "owner_only", "lead scope")
    attach = {"node": node.name, "project_id": other_pid,
              "lane": {"title": "wrong project", "goal": "test", "context": ["decision_log"]}}
    refused(ctx, post(bound, "intents", attach), 403, "owner_only", "attachment honors lead scope")
    ctx.expect(o.call("GET", other_path).json["entries"] == [], "other project unchanged")

    # The current auth model gives each installation its own owner and credentials.
    separate = RustTarget(ctx.target.tmp / "other-owner", ctx.target.binary)
    try:
        foreign = Client(separate, separate.local_token())
        refused(ctx, foreign.call("GET", route), 404, "not_found", "other installation has no project")
        refused(ctx, foreign.call("POST", path, body=CONTENT, key=new_key()), 404, "not_found", "no foreign write")
        outsider = Client(ctx.target, foreign.token)
        refused(ctx, outsider.call("GET", path), 401, "unauthenticated", "other owner's token cannot read")
        refused(ctx, post(outsider, path, CONTENT), 401, "unauthenticated", "other owner's token cannot write")
    finally:
        separate.close()


def mcp(ctx, node, name, *, credential_file=None, **arguments):
    # Use the same stdio entry point as cs mcp, with the harness's disposable install and credential.
    command = [sys.executable, "-B", str(REPO / "cs/scripts/cs"), "mcp",
               "--director", ctx.target.url, "--credential-file", str(credential_file or ctx.target.data / "local-credential"),
               "--node", node.name]
    method = "tools/call" if name else "tools/list"
    request = {"jsonrpc": "2.0", "id": 1, "method": method}
    if name:
        request["params"] = {"name": name, "arguments": arguments}
    result = subprocess.run(command, input=json.dumps(request) + "\n", text=True, capture_output=True,
                            cwd=ctx.target.tmp, env=ctx.target.env, timeout=15)
    ctx.expect(result.returncode == 0, "MCP process exits normally")
    reply = json.loads(result.stdout)["result"]
    if not name:
        return reply["tools"]
    ctx.expect(not reply["isError"], "MCP tool " + name + " succeeds", reply)
    return json.loads(reply["content"][0]["text"])


@case("client", AREA, "decision-log-tools-and-job-context")
def _(ctx):
    o, node = owner(ctx), ctx.node2()
    pid, path = log_path(ctx, o)

    def tool(name, **fields):
        return mcp(ctx, node, name, project_id=pid, **fields)

    schemas = {t["name"] for t in mcp(ctx, node, None)}
    ctx.expect({"decision_log", "record_decision", "start_task"} <= schemas, "published tools")
    first = tool("record_decision", action="create", **CONTENT)["entry"]
    current = tool("record_decision", action="revise", entry_id=first["entry_id"],
                   revision=first["revision"], why="New rationale.")["entry"]
    replacement = tool("record_decision", action="create", **{**CONTENT, "decision": "Keep one shared record"})["entry"]
    tool("record_decision", action="supersede", entry_id=current["entry_id"],
         revision=current["revision"], superseded_by=replacement["entry_id"])
    listing = tool("decision_log", status="active", search="client state")
    ctx.expect(listing["entries"] == [replacement] and "Offline copies drift." in listing["markdown"],
               "filtered tool answers with rendered alternatives")
    args = {"title": "Use project context", "goal": "Do the work.", "model": "codex",
            "context": ["decision_log"], "dispatch_id": new_key()}
    start = tool("start_task", **args)
    bundle_path = "jobs/" + start["job_id"] + "/files/" + start["task"]
    markdown = o.call("GET", bundle_path + "/context/decision-log.md")
    goal = o.call("GET", bundle_path + "/goal.md")
    ctx.expect(markdown.status == 200 and markdown.body.decode() == listing["markdown"]
               and goal.body.decode().count("`context/decision-log.md`") == 1, "goal points to the same rendered active context")
    off = tool("start_task", title="Plain job", goal="No context.", model="codex")
    ctx.expect(o.call("GET", "jobs/" + off["job_id"] + "/files/" + off["task"] + "/goal.md").body == b"No context.",
               "omitted option leaves the goal alone")
    post(o, path, {**CONTENT, "decision": "After submission"})
    again = tool("start_task", **args)
    ctx.expect(again["job_id"] == start["job_id"] and o.call("GET", bundle_path + "/context/decision-log.md").body == markdown.body,
               "dispatch replay keeps the original snapshot")
    end_job(ctx, node, start["job_id"], state="error")
    retry = mcp(ctx, node, "retry_task", job_id=start["job_id"])
    retry_bundle = "jobs/" + retry["job_id"] + "/files/" + retry["task"]
    ctx.expect(o.call("GET", retry_bundle + "/context/decision-log.md").body == markdown.body
               and o.call("GET", retry_bundle + "/goal.md").body.endswith(goal.body),
               "retry carries both the original context and original goal")
    refused(ctx, post(o, "intents", {"node": node.name, "lane": {"title": "x", "goal": "g", "context": ["decision_log"]}}),
            422, "invalid_request", "attachment requires a project")


def hosted(ctx, o, node, **fields):
    o.call("PUT", "lead-node", body={"node": node.name})
    result = post(o, "conversations", {"spec": "hosted-lead", "model": "claude-code-sonnet", **fields})
    ctx.expect(result.status == 201, "hosted conversation created", result)
    return result.json["conversation"]


@case("client", AREA, "hosted-turn-project-snapshot")
def _(ctx):
    from cases.conversations import job_files, post as send, reply, turn

    o, node = owner(ctx), ctx.node2()
    pid, path = log_path(ctx, o)
    _, foreign = log_path(ctx, o)
    post(o, foreign, {**CONTENT, "decision": "Foreign project choice"})
    first = post(o, path, CONTENT).json["entry"]
    conv = hosted(ctx, o, node, project_id=pid)
    initial = send(ctx, o, conv, "What did we decide?")
    entry_path = "conversation/decision-log.md"
    frozen = job_files(node, initial["job_id"])[entry_path].decode()
    ctx.expect(frozen == o.call("GET", path + "?status=active").json["markdown"],
               "turn bundle contains exactly the project's rendered log", frozen)
    trace = turn(o, initial["turn_id"])["context"]["decision_log"]
    ctx.expect(trace == {"revision": first["revision"], "entry_count": 1}, "trace records included snapshot", trace)

    queued = send(ctx, o, conv, "What changed?")
    changed = post(o, path, {**CONTENT, "decision": "Updated project choice"}).json["entry"]
    reply(ctx, node, initial["job_id"], "An answer.")
    followup = turn(o, queued["turn_id"])
    rendered = job_files(node, followup["job_id"])[entry_path].decode()
    ctx.expect(rendered == o.call("GET", path + "?status=active").json["markdown"]
               and followup["context"]["decision_log"] == {"revision": changed["revision"], "entry_count": 2},
               "queued turn receives the latest snapshot at dispatch", followup)
    saved = o.call("GET", "jobs/" + initial["job_id"] + "/files/" + entry_path)
    ctx.expect(saved.status == 200 and saved.body.decode() == frozen, "previous turn remains frozen", saved)


@case("client", AREA, "hosted-turn-without-active-project-log")
def _(ctx):
    from cases.conversations import job_files, post as send

    o, node = owner(ctx), ctx.node2()
    pid, path = log_path(ctx, o)
    other_pid, _ = log_path(ctx, o)
    post(o, path, CONTENT)
    for conv in (hosted(ctx, o, node), hosted(ctx, o, node, project_id=other_pid),
                 created(ctx, o, node, project_id=pid)):
        sent = send(ctx, o, conv, "Hello")
        ctx.expect("conversation/decision-log.md" not in job_files(node, sent["job_id"])
                   and "decision_log" not in sent["context"], "turn has no decision log context", sent)


@case("client", AREA, "hosted-turn-bounded-project-log")
def _(ctx):
    from cases.conversations import job_files, post as send

    o, node = owner(ctx), ctx.node2()
    pid, path = log_path(ctx, o)
    first, _, _ = [
        post(o, path, {"decision": choice, "why": "é" * 5500}).json["entry"]
        for choice in ("Revised choice", "Omitted choice", "Recent choice")
    ]
    revised = post(o, path + "/" + first["entry_id"] + "/revise",
                   {"revision": first["revision"], "decision": "Newest revision"}).json["entry"]
    sent = send(ctx, o, hosted(ctx, o, node, project_id=pid), "What did we decide?")
    markdown = job_files(node, sent["job_id"])["conversation/decision-log.md"].decode()
    ctx.expect(len(markdown) <= 12000 and "Omitted choice" not in markdown
               and 0 <= markdown.find("Newest revision") < markdown.find("Recent choice")
               and "Omitted entries from the active decision log: 1. Use `decision_log` to fetch them all." in markdown,
               "bounded snapshot keeps the two most recently updated entries and a fetch note")
    ctx.expect(sent["context"]["decision_log"] == {"revision": revised["revision"], "entry_count": 2},
               "trace counts the entries sent", sent)
