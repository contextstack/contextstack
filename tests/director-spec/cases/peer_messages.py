"""Directed peer wakes and their live authorization."""
import sqlite3
from contextlib import closing

from cases.client import lead, owner
from cases.conversations import created, job_files, thread
from cases.policy import device
from harness.client import new_key
from harness.run import case


@case("client", "peer-messages", "directed-wakes")
def directed_wakes(ctx):
    node, local = ctx.node2(), owner(ctx)
    member, other, viewer, admin = (device(ctx, role) for role in ("member", "member", "viewer", "admin"))
    source = created(ctx, member, node, title="Lead brief")
    target = created(ctx, other, node, title="Client developer")
    sender = lead(ctx, member, source)
    src, dst = source["conversation_id"], target["conversation_id"]
    allowlist = "peer-message-allowlist"
    path, reverse = (f"conversations/{cid}/peer-messages" for cid in (src, dst))
    request = {"to": dst, "text": "A brief from the lead"}
    pair = {"from_conversation": src, "to_conversation": dst}
    back = {"from_conversation": dst, "to_conversation": src}

    def send(actor=sender, route=path, body=request, key=None):
        return actor.call("POST", route, body=body, key=key or new_key())

    ctx.expect(local.call("GET", allowlist).json == {"pairs": []}, "empty by default")
    ctx.expect(sender.call("GET", path).json == {"peers": []}, "empty discovery")
    ctx.expect(send().status == 403, "empty allowlist refuses")
    ctx.expect(admin.call("PUT", allowlist, body={"pairs": [pair]}, key=new_key()).status == 403, "admin cannot set peers")
    ctx.expect(local.call("PUT", allowlist, body={"pairs": [pair]}, key=new_key()).status == 200, "owner sets peers")
    ctx.expect(sender.call("GET", path).json == {"peers": [dst]}, "lead discovers its peer")
    ctx.expect(sender.call("GET", reverse).status == 403, "lead cannot discover another conversation's peers")
    ctx.expect(send(other, reverse, {"to": src, "text": "reply"}).status == 403, "reverse requires its own entry")
    ctx.expect(send(viewer).status == 403, "viewer cannot send")
    ctx.expect(send(other).status == 403, "member cannot send from another member's conversation")
    ctx.expect(send(body={"to": dst, "text": "é" * 8193}).status == 422, "text cap counts UTF-8 bytes")

    both, allow_key = {"pairs": [pair, back]}, new_key()
    ctx.expect(local.call("PUT", allowlist, body=both).status == 428, "allowlist requires a key")
    ctx.expect(local.call("PUT", allowlist, body=both, key=allow_key).status == 200, "owner enables replies")
    replay = local.call("PUT", allowlist, body=both, key=allow_key)
    ctx.expect(replay.status == 200 and replay.header("idempotent-replayed") == "true", "allowlist write replays")
    ctx.expect(local.call("PUT", allowlist, body={"pairs": []}, key=allow_key).status == 409,
               "changed allowlist replay conflicts")
    ctx.expect(send(sender, reverse, {"to": src, "text": "spoof"}).status == 403,
               "lead cannot send from another member's conversation even with a matching pair")
    key = new_key()
    first = send(key=key)
    ctx.expect(first.status == 201 and set(first.json) == {"turn_id"}, "sender receives only a receipt", first)
    wake, = thread(other, target)["turns"]
    ctx.expect(wake["turn_id"] == first.json["turn_id"] and wake["origin"] == "wake"
               and wake["message"] == request["text"] and wake["state"] == "running"
               and wake["peer"] == {"from_conversation": src, "from_title": "Lead brief", "trust": "untrusted"},
               "target receives an attributed wake", wake)
    goal = job_files(node, wake["job_id"])["tasks/turn/goal.md"]
    ctx.expect(b"Untrusted message from a peer conversation" in goal and request["text"].encode() in goal,
               "recipient job receives the brief as untrusted data")
    ctx.expect(sender.call("GET", "turns/" + wake["turn_id"]).status == 403, "sender cannot read target turn")
    reply = send(other, reverse, {"to": src, "text": "reply"})
    ctx.expect(reply.status == 201, "allowlisted reply", reply)
    incoming, = thread(member, source)["turns"]
    ctx.expect(incoming["turn_id"] == reply.json["turn_id"] and incoming["peer"]["from_conversation"] == dst,
               "reply produces the same wake in the opposite direction", incoming)
    for _ in range(19):
        ctx.expect(send().status == 201, "within the pair's rate limit")
    replay = send(key=key)
    ctx.expect(replay.status == 200 and replay.json == first.json
               and replay.header("idempotent-replayed") == "true", "replay succeeds at the limit", replay)
    ctx.expect(send(body={**request, "text": "changed"}, key=key).status == 409, "changed replay conflicts")
    limited = send()
    ctx.expect(limited.status == 429 and limited.json["code"] == "peer_rate_limited", "pair rate limit", limited)
    ctx.expect(len(thread(other, target)["turns"]) == 20, "no duplicate or over-limit turn")
    sponsor = member.call("GET", "me").json["member"]["id"]
    owner_id = local.call("GET", "me").json["member"]["id"]
    attention = thread(local, source)["conversation"]["attention"]
    ctx.expect(attention and "20 per hour" in attention["question"]
               and attention["assignee_member_id"] == owner_id,
               "the owner receives the question about the allowlist", attention)
    ctx.expect(thread(member, source)["conversation"]["attention"] is None,
               "the member sponsor does not receive the owner's question")
    ctx.expect(send(other, reverse, {"to": src, "text": "follow-up"}).status == 201, "reverse has a separate limit")

    with closing(sqlite3.connect(ctx.target.data / "director.sqlite")) as db, db:
        db.execute("UPDATE members SET role = 'viewer' WHERE id = ?", (sponsor,))
    ctx.expect(send(key=key).status == 403, "lead replay rechecks sponsor's live start_work role")


    ctx.expect(local.call("POST", f"conversations/{dst}/archive", key=new_key()).status == 200, "archive peer")
    ctx.expect(local.call("GET", allowlist).json == {"pairs": []}, "archived pairs drop on read")
    saved = local.call("PUT", allowlist, body=both, key=new_key())
    ctx.expect(saved.status == 200 and saved.json == {"pairs": []}, "archived pairs ignored on write", saved)
