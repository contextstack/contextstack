"""Hosted-lead home node and the node lead-credential route."""
from harness.client import Client, new_key
from harness.run import case

from cases.client import device, owner, refused

C = "client"


@case(C, "lead-node", "default-and-owner-set")
def _(ctx):
    o = owner(ctx)
    home = ctx.node2("home")
    ctx.node2("away")
    got = o.call("GET", "lead-node")
    ctx.expect(got.status == 200 and isinstance(got.json.get("node"), str) and got.json.get("source") in ("default", "set"),
               "GET lead-node answers node and source", got)
    set_a = o.call("PUT", "lead-node", body={"node": home.name})
    ctx.expect(set_a.status == 200 and set_a.json.get("source") == "set", "owner sets lead node", set_a)
    refused(ctx, device(ctx).call("PUT", "lead-node", body={"node": home.name}), 403, "owner_only",
            "device cannot set lead node")
    refused(ctx, o.call("PUT", "lead-node", body={"node": "no-such-node"}), 422, "unknown_node", "unknown node")


@case(C, "lead-node", "hosted-lead-create-placement")
def _(ctx):
    from cases.conversations import created
    o = owner(ctx)
    home = ctx.node2("home")
    away = ctx.node2("away")
    o.call("PUT", "lead-node", body={"node": home.name})
    key = new_key("lead-home")
    body = {"spec": "hosted-lead", "model": "claude-code-sonnet", "title": "home lead"}
    a = o.call("POST", "conversations", body=body, key=key)
    ctx.expect(a.status == 201 and a.json["conversation"]["node"] == home.name,
               "bundle-less lead create lands on the lead node", a)
    refused(ctx, o.call("POST", "conversations", body={**body, "node": away.name}, key=new_key("lead-away")),
            422, "lead_node_mismatch", "wrong node refused")
    plain = created(ctx, o, away)
    ctx.expect(plain["node"] == away.name, "ordinary conversations still use the node they name", plain)


@case(C, "lead-node", "node-lead-credential-route")
def _(ctx):
    from cases.conversations import created
    o = owner(ctx)
    home = ctx.node2("home")
    away = ctx.node2("away")
    o.call("PUT", "lead-node", body={"node": home.name})
    conv = o.call(
        "POST",
        "conversations",
        body={"spec": "hosted-lead", "model": "claude-code-sonnet"},
        key=new_key("lead-cred"),
    ).json["conversation"]
    plain = created(ctx, o, home)
    cid = conv["conversation_id"]
    a = home.call("POST", "conversations/%s/lead-credential" % cid)
    ctx.expect(a.status == 201 and isinstance(a.json.get("token"), str), "home node mints lead credential", a)
    first = Client(ctx.target, a.json["token"])
    ctx.expect(first.call("GET", "me").status == 200, "minted token authenticates")
    refused(ctx, away.call("POST", "conversations/%s/lead-credential" % cid), 403, "not_this_node", "other node")
    refused(ctx, home.call("POST", "conversations/%s/lead-credential" % plain["conversation_id"]),
            409, "not_a_lead", "non-lead conversation")
    refused(ctx, home.call("POST", "conversations/con_missing/lead-credential"), 404, "not_found", "unknown id")
    b = home.call("POST", "conversations/%s/lead-credential" % cid)
    ctx.expect(b.status == 201 and b.json.get("token") != a.json.get("token"), "second mint returns a new token", b)
    refused(ctx, first.call("GET", "me"), 401, "unauthenticated", "first token revoked")
    ctx.expect(Client(ctx.target, b.json["token"]).call("GET", "me").status == 200,
               "the new token works: it is minted before older ones are revoked")
