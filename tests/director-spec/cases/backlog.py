"""Project backlog and hosted heartbeat against disposable Directors and stand-in nodes."""
import os
import time

from cases.budgets import end_job, project, submit, until
from cases.client import device, lead, owner, refused
from cases.conversations import created, reply, thread
from cases.decision_log import mcp, post
from cases.lead_conversation import pin_lead_node
from harness.client import Client, new_key
from harness.run import case
from harness.targets import RustTarget

AREA = "backlog"


def routes(ctx, client):
    pid = project(ctx, client, "Backlog")["project_id"]
    base = "projects/" + pid
    return pid, base + "/backlog", base + "/heartbeat"


def add(ctx, client, path, title, **fields):
    result = post(client, path, {"title": title, **fields})
    ctx.expect(result.status == 201, "backlog item created", result)
    return result.json["item"]


def change(ctx, client, path, item, **fields):
    result = post(client, path + "/" + item["item_id"] + "/update", {"revision": item["revision"], **fields})
    ctx.expect(result.status == 200, "backlog item updated", result)
    return result.json["item"]


def read(client, path, suffix="", field="items"):
    response = client.call("GET", path + suffix)
    assert response.status == 200, response
    return response.json[field]


@case("client", AREA, "backlog-routes-history-and-events")
def _(ctx):
    o, paired = owner(ctx), device(ctx)
    _, path, _ = routes(ctx, o)
    key = new_key()
    data = {"title": "Implement", "body": "## Markdown", "intent_ids": ["int_link"],
            "job_ids": ["job_link"], "decision_log_entry_ids": ["dle_link"]}
    stream = o.stream()
    try:
        stream.until(lambda name, _data: name == "hello")
        response = post(paired, path, data, key)
        ctx.expect(response.status == 201, "paired client can create", response)
        first = response.json["item"]
        item_path = path + "/" + first["item_id"]
        ctx.expect(all(first[field] == value for field, value in data.items())
                   and first["status"] == "next" and first["created_by"] == first["updated_by"]
                   and first["created_at"] == first["updated_at"], "fields and author", first)
        ctx.expect(post(paired, path, data, key).json["item"] == first, "create replay")
        refused(ctx, post(paired, path, {**data, "title": "other"}, key), 409, "idempotency_conflict", "key reuse")
        refused(ctx, o.call("POST", path, body=data), 428, "idempotency_key_required", "missing key")
        refused(ctx, o.call("GET", path + "?status=unknown"), 422, "invalid_request", "invalid filter")
        refused(ctx, post(o, path, {"title": "Bad", "blocked_on": [{"kind": "owner"}]}),
                422, "invalid_request", "blocker reference required")
        def expect_event(item):
            event, _ = stream.until(lambda name, d: name == "change" and d.get("kind") == "backlog_item"
                                    and d.get("id") == item["item_id"] and d.get("revision") == item["revision"])
            ctx.expect(event is not None and event[1]["record"] == item, "backlog event carries the record", event)

        expect_event(first)
        now = change(ctx, o, path, first, status="now")
        expect_event(now)
        ctx.expect(read(o, path, "?status=now") == [now], "filter by status")
        refused(ctx, post(o, item_path + "/update", {"revision": first["revision"], "title": "stale"}),
                409, "conflict", "stale update")
        dropped = change(ctx, o, path, now, status="dropped")
        refused(ctx, o.call("DELETE", item_path), 404, "not_found", "no hard delete")
        ctx.expect(o.call("GET", item_path).json == {"item": dropped, "revisions": [first, now]},
                   "dropped item retains history")
    finally:
        stream.close()


@case("client", AREA, "backlog-next-and-atomic-reorder")
def _(ctx):
    o, node = owner(ctx), ctx.node2()
    pid, path, _ = routes(ctx, o)
    ctx.expect(read(o, path, "/next_unblocked", "item") is None, "empty queue")
    dependency = add(ctx, o, path, "Dependency", status="now")
    job = submit(ctx, o, node, pid)
    waiting = add(ctx, o, path, "Waiting", priority=-10, blocked_on=[
        {"kind": "item", "ref": dependency["item_id"]}, {"kind": "job", "ref": job["job_id"]}])
    for kind in ("owner", "external", "job", "item"):
        add(ctx, o, path, kind, priority=-20, blocked_on=[{"kind": kind, "ref": "missing"}])
    earlier = add(ctx, o, path, "Earlier", priority=0)
    later = add(ctx, o, path, "Later", priority=2)

    def next_item():
        return read(o, path, "/next_unblocked", "item")

    ctx.expect(next_item() == earlier, "priority among eligible next items")
    patch = [{"item_id": later["item_id"], "revision": later["revision"], "priority": -1},
             {"item_id": earlier["item_id"], "revision": 0, "priority": 1}]
    refused(ctx, post(o, path + "/reorder", {"items": patch}), 409, "conflict", "batch rolls back on stale revision")
    ctx.expect(next_item() == earlier, "failed reorder leaves priorities unchanged")
    patch[1]["revision"] = earlier["revision"]
    key = new_key()
    ordered = post(o, path + "/reorder", {"items": patch}, key)
    ctx.expect(ordered.status == 200 and next_item()["item_id"] == later["item_id"], "batch changes order", ordered)
    ctx.expect(post(o, path + "/reorder", {"items": patch}, key).json == ordered.json, "reorder replay")
    change(ctx, o, path, dependency, status="done")
    ctx.expect(next_item()["item_id"] == later["item_id"], "one outstanding blocker prevents selection")
    node.accept(job["job_id"])
    end_job(ctx, node, job["job_id"])
    ctx.expect(next_item() == waiting, "all blockers clear once dependency and job end")


@case("client", AREA, "backlog-installation-and-lead-scope")
def _(ctx):
    o, node = owner(ctx), ctx.node2()
    pid, path, _ = routes(ctx, o)
    other, other_path, _ = routes(ctx, o)
    first = add(ctx, o, path, "Private")
    item_path = path + "/" + first["item_id"]
    bound = lead(ctx, o, created(ctx, o, node, project_id=pid))
    outsider = lead(ctx, o, created(ctx, o, node, project_id=other))
    ctx.expect(read(bound, path) == [first], "bound lead can list")
    change(ctx, bound, path, first, body="Updated by a lead")
    refused(ctx, o.call("GET", other_path + "/" + first["item_id"]), 404, "not_found", "item belongs to project")
    for method, route, data in (
        ("GET", path, None), ("GET", item_path, None), ("GET", path + "/next_unblocked", None),
        ("GET", path + "/needs_you", None), ("POST", path, {"title": "No"}),
        ("POST", item_path + "/update", {"revision": first["revision"]}),
        ("POST", path + "/reorder", {"items": []}),
    ):
        refused(ctx, outsider.call(method, route, body=data, key=new_key()), 403, "owner_only", "lead project scope")
    stream = outsider.stream()
    try:
        stream.until(lambda name, _data: name == "hello")
        hidden = add(ctx, o, path, "Hidden event")
        visible = add(ctx, o, other_path, "Visible event")
        event, seen = stream.until(lambda name, d: name == "change" and d.get("kind") == "backlog_item"
                                   and d.get("id") == visible["item_id"])
        ctx.expect(event is not None and all(data.get("id") != hidden["item_id"] for _, data, _ in seen),
                   "lead change stream only exposes items in its project", seen)
    finally:
        stream.close()

    separate = RustTarget(ctx.target.tmp / "other-backlog-owner", ctx.target.binary)
    try:
        foreign = Client(separate, separate.local_token())
        refused(ctx, foreign.call("GET", item_path), 404, "not_found", "another installation has no record")
        refused(ctx, post(foreign, path, {"title": "No"}), 404, "not_found", "another installation cannot write")
        refused(ctx, Client(ctx.target, foreign.token).call("GET", path), 401, "unauthenticated",
                "another installation's token cannot read")
    finally:
        separate.close()


@case("client", AREA, "backlog-lead-tools")
def _(ctx):
    o, node = owner(ctx), ctx.node2()
    pid, path, _ = routes(ctx, o)
    bound = lead(ctx, o, created(ctx, o, node, project_id=pid))
    credential = ctx.target.tmp / "backlog-lead-credential"
    with open(credential, "x", opener=lambda path, flags: os.open(path, flags, 0o600)) as file:
        file.write(bound.token + "\n")

    def tool(name, **fields):
        return mcp(ctx, node, name, credential_file=credential, project_id=pid, **fields)

    schemas = {tool["name"] for tool in mcp(ctx, node, None, credential_file=credential)}
    ctx.expect({"backlog", "backlog_update"} <= schemas, "tools advertised to hosted lead")
    first = tool("backlog_update", title="Use the backlog")["item"]
    ctx.expect(tool("backlog", action="next")["item"] == first, "next tool returns eligible item")
    updated = tool("backlog_update", item_id=first["item_id"], revision=first["revision"], status="now")["item"]
    ctx.expect(tool("backlog", status="now")["items"] == read(o, path) == [updated], "list tool filters Director records")
    blockers = [{"kind": "owner", "ref": "rollout"}]
    question = {"options": [{"id": "small", "label": "Small rollout"}]}
    asked = tool("backlog_update", item_id=updated["item_id"], revision=updated["revision"], status="blocked",
                 blocked_on=blockers, needs_you=question)["item"]
    ctx.expect(asked["blocked_on"] == blockers and asked["needs_you"] == question
               and read(o, path, "/needs_you") == [{**asked, "actions": {"answer": True}}],
               "lead asks through backlog_update and owner can answer")


@case("client", AREA, "heartbeat-lead-project-scope")
def _(ctx):
    import sqlite3
    from cases.policy import device

    o, node = owner(ctx), ctx.node2()
    pid, _, heartbeat = routes(ctx, o)
    other, _, other_heartbeat = routes(ctx, o)
    pin_lead_node(o, node)
    node.status([], agent_capabilities={"templates": ["claude-code-sonnet"]})
    def hosted(project_id, actor=o):
        made = post(actor, "conversations", {"spec": "hosted-lead", "model": "claude-code-sonnet",
                                         "project_id": project_id})
        ctx.expect(made.status == 201, "hosted conversation created", made)
        return made.json["conversation"]

    own, same_project, foreign = hosted(pid), hosted(pid), hosted(other)
    bound = lead(ctx, o, own)
    config = {"interval_seconds": 3600, "conversation_id": own["conversation_id"]}
    ctx.expect(bound.call("GET", heartbeat).json["interval_seconds"] is None, "lead can read own schedule")
    result = bound.call("PUT", heartbeat, body=config)
    ctx.expect(result.status == 200 and result.json["interval_seconds"] == 3600, "lead enables own schedule", result)
    refused(ctx, bound.call("PUT", heartbeat, body={**config, "interval_seconds": 60}),
            422, "invalid_request", "a lead cannot wake itself more often than every 15 minutes")
    refused(ctx, bound.call("GET", other_heartbeat), 403, "owner_only", "lead cannot read another project")
    refused(ctx, bound.call("PUT", other_heartbeat, body={"interval_seconds": None}),
            403, "owner_only", "lead cannot disable another project's schedule")
    refused(ctx, bound.call("PUT", heartbeat, body={**config, "conversation_id": foreign["conversation_id"]}),
            403, "forbidden", "lead cannot send its heartbeat to another project")
    refused(ctx, bound.call("PUT", heartbeat, body={**config, "conversation_id": same_project["conversation_id"]}),
            403, "forbidden", "lead cannot target another conversation in its project")
    disabled = bound.call("PUT", heartbeat, body={"interval_seconds": None})
    ctx.expect(disabled.status == 200 and disabled.json["interval_seconds"] is None, "lead disables own schedule", disabled)

    member, future_viewer = device(ctx, "member"), device(ctx, "member")
    admin = device(ctx, "admin")
    own = hosted(pid, member)
    member_lead = lead(ctx, member, own)
    viewer_lead = lead(ctx, future_viewer, hosted(other, future_viewer))
    admin_lead = lead(ctx, admin, hosted(other, admin))
    lead_caps = set("read_work start_work control_work decide_run edit_backlog edit_decision_log".split())
    for actor in (bound, admin_lead, member_lead):
        ctx.expect(set(actor.call("GET", "me").json["capabilities"]) == lead_caps, "full active lead capabilities")
    config = {"interval_seconds": 900, "conversation_id": own["conversation_id"]}
    refused(ctx, member.call("PUT", heartbeat, body=config), 403, "forbidden", "member cannot set heartbeat")
    ctx.expect(member.call("GET", "schedules").status == 200, "member can read fleet schedules")
    ctx.expect(member_lead.call("GET", "schedules").status == 403, "lead cannot read fleet schedules")
    for actor in (member, member_lead):
        for method, path in (("POST", "schedules"), ("DELETE", f"schedules/{node.name}/unknown"),
                             ("POST", f"schedules/{node.name}/unknown/changes")):
            ctx.expect(actor.call(method, path, body={}, key=new_key()).status == 403, "schedule write denied")
    owner_config = {**config, "conversation_id": same_project["conversation_id"]}
    ctx.expect(admin.call("PUT", heartbeat, body=owner_config).status == 200, "admin can set project heartbeat")
    refused(ctx, member_lead.call("PUT", heartbeat, body={"interval_seconds": None}), 403, "forbidden",
            "lead cannot disable another conversation's heartbeat")
    ctx.expect(o.call("GET", heartbeat).json == {"project_id": pid, **owner_config}, "owner schedule is intact")
    ctx.expect(admin.call("PUT", heartbeat, body={"interval_seconds": None}).status == 200, "admin clears heartbeat")
    ctx.expect(member_lead.call("PUT", heartbeat, body=config).status == 200, "member's lead sets own heartbeat")
    refused(ctx, member_lead.call("PUT", heartbeat, body={**config, "conversation_id": same_project["conversation_id"]}),
            403, "forbidden", "member's lead cannot target an owner's conversation")
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.executemany("UPDATE members SET role = 'viewer' WHERE id = ?",
                       [("policy-" + actor.credential_id,) for actor in (member, future_viewer)])
    viewer_me = viewer_lead.call("GET", "me").json
    ctx.expect(viewer_me["role"] == "viewer" and viewer_me["capabilities"] == ["read_work"],
               "full viewer-sponsored lead capabilities", viewer_me)
    viewer_config = {**config, "conversation_id": viewer_me["conversation_id"]}
    refused(ctx, viewer_lead.call("PUT", other_heartbeat, body=viewer_config), 403, "forbidden", "viewer-sponsored lead")
    refused(ctx, member_lead.call("PUT", heartbeat, body=config), 403, "forbidden", "demoted sponsor")
    ctx.expect(o.call("GET", heartbeat).json == {"project_id": pid, **config}, "refusals preserve schedule")
    with sqlite3.connect(ctx.target.data / "director.sqlite") as db:
        db.execute("UPDATE members SET status = 'disabled' WHERE id = ?", ("policy-" + member.credential_id,))
    ctx.expect(member.call("GET", "me").status == 401, "disabled member cannot authenticate")
    ctx.expect(member_lead.call("PUT", heartbeat, body=config).status == 401, "disabled sponsor invalidates lead")


@case("client", AREA, "backlog-heartbeat-off-stop-and-overlap")
def _(ctx):
    o, node = owner(ctx), ctx.node2()
    pid, path, heartbeat = routes(ctx, o)
    pin_lead_node(o, node)
    node.status([], agent_capabilities={"templates": ["claude-code-sonnet"]})
    made = post(o, "conversations", {"spec": "hosted-lead", "model": "claude-code-sonnet", "project_id": pid})
    ctx.expect(made.status == 201, "hosted conversation created", made)
    conv = made.json["conversation"]
    conv_path = "conversations/" + conv["conversation_id"]
    enabled = {"interval_seconds": 1, "conversation_id": conv["conversation_id"]}

    def turns():
        return thread(o, conv)["turns"]

    ctx.expect(o.call("GET", heartbeat).json["interval_seconds"] is None, "off by default")
    time.sleep(1.2)
    ctx.expect(turns() == [], "off does not start a turn")
    ordinary = created(ctx, o, node, project_id=pid)
    refused(ctx, o.call("PUT", heartbeat, body={**enabled, "conversation_id": ordinary["conversation_id"]}),
            422, "invalid_request", "requires hosted lead")
    blocked = add(ctx, o, path, "Owner decision", status="blocked", blocked_on=[{"kind": "owner", "ref": "owner"}],
                  blocked_reason="Approve the rollout?", stop=True)
    ctx.expect(read(o, path, "/needs_you") == [{**blocked, "actions": {"answer": True}}], "owner blocker is on needs-you")
    attention = o.call("GET", conv_path).json["conversation"]["attention"]
    ctx.expect(attention["backlog_item_id"] == blocked["item_id"] and attention["question"] == "Approve the rollout?",
               "conversation attention exposes the owner blocker", attention)
    ctx.expect(o.call("PUT", heartbeat, body=enabled).json["interval_seconds"] == 1, "enabled")
    time.sleep(2.2)
    ctx.expect(turns() == [], "owner stop skips due heartbeat")
    blocked = change(ctx, o, path, blocked, stop=False)
    started = until(turns, seconds=5)
    ctx.expect(started and len(started) == 1, "heartbeat starts with a non-stop owner blocker", started)
    first = started[0]
    ctx.expect(first["origin"] == "heartbeat" and first["state"] == "running"
               and pid in first["message"] and "backlog_update" in first["message"], "recorded hosted turn", first)
    time.sleep(2.2)
    ctx.expect(turns() == started, "multiple due ticks cannot overlap the running turn")
    reply(ctx, node, first["job_id"], "Nothing ready.")
    restarted = until(lambda: len(turns()) == 2, seconds=5)
    ctx.expect(restarted and turns()[0]["state"] == "done", "fires again only after the first turn finishes")
    second = turns()[1]
    ctx.expect(o.call("PUT", heartbeat, body={"interval_seconds": None}).json["interval_seconds"] is None, "disabled")
    reply(ctx, node, second["job_id"], "Nothing ready.")
    time.sleep(2.2)
    ctx.expect(len(turns()) == 2, "off suppresses future turns")
    change(ctx, o, path, blocked, status="done")
    ctx.expect(read(o, path, "/needs_you") == [] and o.call("GET", conv_path).json["conversation"]["attention"] is None,
               "resolved item clears needs-you and attention")


@case("client", AREA, "structured-needs-you-unblocks-backlog")
def _(ctx):
    from cases.policy import device as role_device

    o, node = owner(ctx), ctx.node2()
    pid, path, _ = routes(ctx, o)
    viewer, member = role_device(ctx, "viewer"), role_device(ctx, "member")
    pin_lead_node(o, node)
    made = post(member, "conversations", {"spec": "hosted-lead", "model": "claude-code-sonnet", "project_id": pid})
    ctx.expect(made.status == 201, "member creates hosted conversation", made)
    conv = made.json["conversation"]
    conv_path = "conversations/" + conv["conversation_id"]
    plain = o.call("PUT", conv_path + "/attention", body={"question": "Plain question"})
    attention = plain.json["conversation"]["attention"]
    ctx.expect(attention["question"] == "Plain question" and "options" not in attention
               and "links" not in attention and "default" not in attention
               and attention["actions"] == {"control": True}, "plain attention can be cleared", plain)
    ctx.expect(o.call("DELETE", conv_path + "/attention").status == 200, "plain attention still clears")

    choices = {"options": [{"id": "small", "label": "Small rollout"}, {"id": "full", "label": "Full rollout"}],
               "default": "small", "links": {"lane": "lane/example"}}
    item = add(ctx, o, path, "Choose a rollout", status="blocked", blocked_reason="Pick a rollout",
               blocked_on=[{"kind": "owner", "ref": "rollout"}], needs_you=choices)
    attention = o.call("GET", conv_path).json["conversation"]["attention"]
    ctx.expect(attention["options"] == choices["options"] and attention["default"] == "small"
               and attention["links"] == {"lane": "lane/example", "backlog": item["item_id"]}
               and attention["actions"] == {"answer": True},
               "client receives options and navigation", attention)
    queue = o.call("GET", path + ".md")
    ctx.expect(queue.status == 200 and all(heading in queue.body for heading in
               (b"## Now", b"## Next", b"## Blocked", b"## Needs you", b"Choose a rollout")), "Markdown queue", queue)
    answer_path = path + "/" + item["item_id"] + "/answer"
    answer = {"revision": item["revision"], "choice": "full"}
    bound = lead(ctx, member, conv)
    item_path = answer_path.removesuffix("/answer")
    before = o.call("GET", item_path).json
    refused(ctx, post(bound, answer_path, answer), 403, "forbidden", "lead cannot answer")
    update_path = item_path + "/update"
    for edit in ({"blocked_on": []}, {"blocked_on": [{"kind": "owner", "ref": "other"}]}, {"status": "dropped"},
                 {"needs_you": None}, {"needs_you": {**choices, "default": "full"}},
                 {"needs_you_answer": {"choice": "full"}}):
        refused(ctx, post(bound, update_path, {"revision": item["revision"], **edit}),
                403, "forbidden", "lead cannot change an owner decision")
    ctx.expect(read(o, path, "/needs_you") == [{**item, "actions": {"answer": True}}], "lead refusals preserve the item")
    ctx.expect(o.call("GET", item_path).json == before, "lead refusals preserve revision history")
    ctx.expect(o.call("GET", conv_path).json["conversation"]["attention"] == attention,
               "lead refusals preserve attention")
    refused(ctx, post(viewer, answer_path, answer), 403, "forbidden", "viewer cannot answer")
    refused(ctx, post(member, answer_path, answer), 403, "forbidden", "unassigned member cannot answer")
    refused(ctx, post(member, update_path, {"revision": item["revision"], "blocked_on": []}),
            403, "forbidden", "unassigned member cannot remove the blocker")
    refused(ctx, post(o, answer_path, {**answer, "revision": -1}), 409, "conflict", "stale revision")
    result = post(o, answer_path, answer)
    after = result.json["item"]
    recorded = after["needs_you_answer"]
    ctx.expect(result.status == 200 and after["status"] == "next" and after["blocked_on"] == []
               and recorded["choice"] == "full" and recorded["label"] == "Full rollout"
               and recorded["member_id"] == o.call("GET", "me").json["member"]["id"],
               "answer unblocks and records the member", result)
    ctx.expect(o.call("GET", conv_path).json["conversation"]["attention"] is None, "resolved attention disappears")
