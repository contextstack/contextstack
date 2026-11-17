"""Conversation attention (needs-you): owner and lead set/clear; owner turn clears; events and GET."""
from harness.client import new_key
from harness.run import case

from cases.client import lead, owner, refused
from cases.conversations import created, post

C = "client"


@case(C, "attention", "owner-set-clear-and-get")
def _(ctx):
    """Owner sets attention on a conversation, reads it back, clears it, and sees null again."""
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    cid = conv["conversation_id"]
    a = o.call("PUT", "conversations/%s/attention" % cid, body={"question": "Ready to land lane/fix?", "job_id": "job_test"})
    ctx.expect(a.status == 200, "owner sets attention", a)
    att = a.json["conversation"]["attention"]
    owner_id = o.call("GET", "me").json["credential_id"]
    ctx.expect(att["question"] == "Ready to land lane/fix?" and att["job_id"] == "job_test"
               and att["set_by"] == owner_id and att["set_at"], "attention shape", att)
    listed = o.call("GET", "conversations?node=" + n.name).json["conversations"]
    ctx.expect(any(c["conversation_id"] == cid and c["attention"]["question"] == att["question"] for c in listed),
               "list includes attention", listed)
    got = o.call("GET", "conversations/" + cid).json["conversation"]
    ctx.expect(got["attention"]["question"] == att["question"], "GET by id", got)
    a = o.call("DELETE", "conversations/%s/attention" % cid)
    ctx.expect(a.status == 200 and a.json["conversation"]["attention"] is None, "owner clears", a)


@case(C, "attention", "lead-set-clear-bound")
def _(ctx):
    """A lead sets and clears attention on its bound conversation only."""
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    cid = conv["conversation_id"]
    l = lead(ctx, o, conv)
    a = l.call("PUT", "conversations/%s/attention" % cid, body={"question": "Blocked on credentials."})
    ctx.expect(a.status == 200 and a.json["conversation"]["attention"]["set_by"] == l.credential_id,
               "lead sets attention", a)
    ctx.expect(a.json["conversation"]["attention"]["source_kind"] == "lead", "Director attributes lead attention", a)
    a = l.call("DELETE", "conversations/%s/attention" % cid)
    ctx.expect(a.status == 200 and a.json["conversation"]["attention"] is None, "lead clears", a)


@case(C, "attention", "lead-refused-other-conversation")
def _(ctx):
    """A lead cannot set or clear attention on a conversation it is not bound to."""
    n, o = ctx.node2(), owner(ctx)
    bound = created(ctx, o, n)
    other = created(ctx, o, n)
    l = lead(ctx, o, bound)
    oid = other["conversation_id"]
    for method, path in (("PUT", "conversations/%s/attention" % oid), ("DELETE", "conversations/%s/attention" % oid)):
        body = {"question": "escape"} if method == "PUT" else None
        refused(ctx, l.call(method, path, body=body), 403, "owner_only", method + " other conversation")


@case(C, "attention", "owner-turn-clears-attention")
def _(ctx):
    """Posting an owner turn clears attention and records a conversation change."""
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    cid = conv["conversation_id"]
    rev = o.call("PUT", "conversations/%s/attention" % cid, body={"question": "Approve merge?"}).json["conversation"]["revision"]
    post(ctx, o, conv, "Yes — land it.", key=new_key("turn"))
    after = o.call("GET", "conversations/" + cid).json["conversation"]
    ctx.expect(after["attention"] is None and after["revision"] > rev, "turn cleared attention", after)


@case(C, "attention", "attention-change-on-event-stream")
def _(ctx):
    """Setting attention emits a conversation change on the global event stream."""
    n, o = ctx.node2(), owner(ctx)
    conv = created(ctx, o, n)
    cid = conv["conversation_id"]
    s = o.stream()
    try:
        s.until(lambda name, _d: name == "hello")
        a = o.call("PUT", "conversations/%s/attention" % cid, body={"question": "Need a decision."})
        ctx.expect(a.status == 200, "set attention", a)
        want = a.json["conversation"]["revision"]
        event, _ = s.until(lambda name, d: name == "change" and d.get("kind") == "conversation" and d.get("id") == cid
                           and d.get("revision") == want)
        ctx.expect(event is not None, "conversation change event", event)
        ctx.expect(event[1]["record"]["attention"]["question"] == "Need a decision.", "event carries attention",
                   event[1])
    finally:
        s.close()
