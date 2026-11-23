"""Director-owned chief-of-staff lifecycle and authorization."""
from harness.client import Client, new_key
from harness.run import case
from cases.client import owner, device, refused


@case("client", "chief-of-staff", "lifecycle")
def _(ctx):
    o = owner(ctx)
    initial = o.call("GET", "chief-of-staff")
    ctx.expect(initial.status == 200 and initial.json["chief_of_staff"] == {
        "enabled": False, "name": "Chief of staff", "conversation_id": None, "revision": 0}, "default slot", initial)
    home = ctx.node2("chief-home")
    o.call("PUT", "lead-node", body={"node": home.name})
    s = o.stream()
    try:
        s.until(lambda name, _: name == "hello")
        key = new_key()
        paired = device(ctx)
        a = paired.call("PUT", "chief-of-staff", body={"enabled": True, "name": " Chief "}, key=key)
        ctx.expect(a.status == 200, "enable", a)
        slot = a.json["chief_of_staff"]
        cid = slot["conversation_id"]
        path = "conversations/" + cid
        conv = o.call("GET", path).json["conversation"]
        ctx.expect(conv["node"] == home.name and conv["title"] == "Chief" and conv["lead"] is True
                   and conv["model"] == "claude-code-sonnet", "hosted lead on effective node", conv)
        event, _ = s.until(lambda name, d: name == "change" and d["kind"] == "chief_of_staff")
        ctx.expect(event and event[1]["record"] == slot and event[1]["id"] == "chief_of_staff"
                   and event[1]["revision"] == slot["revision"], "slot change event", event)
        for k in (key, new_key()):
            again = o.call("PUT", "chief-of-staff", body={"enabled": True, "name": " Chief "}, key=k)
            ctx.expect(again.json == a.json, "enable is idempotent", again)
        ctx.expect(device(ctx).call("GET", "chief-of-staff").json == a.json, "device reads slot")
        for method, suffix in (("POST", "/archive"), ("DELETE", "")):
            refused(ctx, o.call(method, path + suffix, body={}, key=new_key()), 409, "chief_of_staff", "protected chief")
        token = home.call("POST", path + "/lead-credential").json["token"]
        lead = Client(ctx.target, token)
        for method in ("GET", "PUT"):
            refused(ctx, lead.call(method, "chief-of-staff", body={} if method == "PUT" else None, key=new_key()), 403, "owner_only", "lead slot access")
        refused(ctx, lead.call("PATCH", path, body={"title": "Other"}, key=new_key()), 403, "owner_only", "lead rename")
        refused(ctx, o.call("PATCH", path, body={"title": "x" * 201}, key=new_key()), 422, "invalid_request", "long title")
        refused(ctx, o.call("PATCH", path, body={"title": "  "}, key=new_key()), 422, "invalid_request", "empty title")
        renamed = paired.call("PATCH", path, body={"title": " Renamed "}, key=new_key())
        ctx.expect(renamed.status == 200 and renamed.json["conversation"]["title"] == "Renamed"
                   and renamed.json["conversation"]["revision"] > conv["revision"], "rename bumps revision", renamed)
        ctx.expect(o.call("GET", "chief-of-staff").json["chief_of_staff"]["name"] == "Renamed", "slot follows rename")
        o.call("PUT", "chief-of-staff", body={"enabled": False}, key=new_key())
        ctx.expect(o.call("GET", path).json["conversation"]["archived_at"] is not None, "disable archives")
        back = o.call("PUT", "chief-of-staff", body={"enabled": True}, key=new_key())
        ctx.expect(back.json["chief_of_staff"]["conversation_id"] == cid and o.call("GET", path).json["conversation"]["archived_at"] is None, "reenable reuses and unarchives")
        o.call("PUT", "chief-of-staff", body={"enabled": False}, key=new_key())
        o.call("DELETE", path, body={}, key=new_key())
    finally:
        s.close()


@case("client", "chief-of-staff", "provider-limits")
def _(ctx):
    o = owner(ctx)
    home = ctx.node2("chief-limits")
    o.call("PUT", "lead-node", body={"node": home.name})
    for all_limited in (False, True):
        models = ["claude-code-sonnet", "claude-code", "claude-code-opus"] if all_limited else ["claude-code-sonnet"]
        status = home.call("POST", "status", body={"active": [], "provider_limits": [
            {"template": m, "resets_at": "2099-01-01T00:00:00Z"} for m in models]})
        ctx.expect(status.status == 200, "provider limits status accepted", status)
        key = new_key()
        a = o.call("PUT", "chief-of-staff", body={"enabled": True}, key=key)
        ctx.expect(a.status == 200, "enable with limits", a)
        cid = a.json["chief_of_staff"]["conversation_id"]
        conv = o.call("GET", "conversations/" + cid).json["conversation"]
        ctx.expect(conv["model"] == ("claude-code-sonnet" if all_limited else "claude-code"), "model selection respects limits", conv)
        if all_limited:
            ctx.expect(bool(a.json.get("model_note")), "all limited fallback explains model", a)
            ctx.expect(o.call("PUT", "chief-of-staff", body={"enabled": True}, key=key).json == a.json, "fallback replay retains note")
        o.call("PUT", "chief-of-staff", body={"enabled": False}, key=new_key())
        deleted = o.call("DELETE", "conversations/" + cid, body={}, key=new_key())
        ctx.expect(deleted.status == 200, "disabled chief may be deleted", deleted)


@case("client", "chief-of-staff", "adopt-existing-lead")
def _(ctx):
    """PUT may adopt an existing hosted-lead conversation instead of creating one; a plain chat is refused."""
    from cases.conversations import created
    o = owner(ctx)
    home = ctx.node2("adopt-home")
    o.call("PUT", "lead-node", body={"node": home.name})
    existing = o.call("POST", "conversations", body={"spec": "hosted-lead", "model": "claude-code-sonnet"},
                      key=new_key()).json["conversation"]
    plain = created(ctx, o, home)
    refused(ctx, o.call("PUT", "chief-of-staff", body={"enabled": True, "conversation_id": plain["conversation_id"]},
                        key=new_key()), 422, "invalid_request", "plain chat cannot be adopted")
    refused(ctx, o.call("PUT", "chief-of-staff", body={"enabled": True, "conversation_id": "con_missing"},
                        key=new_key()), 422, "invalid_request", "missing conversation cannot be adopted")
    a = o.call("PUT", "chief-of-staff", body={"enabled": True, "conversation_id": existing["conversation_id"]},
               key=new_key())
    ctx.expect(a.status == 200 and a.json["chief_of_staff"]["conversation_id"] == existing["conversation_id"],
               "existing lead adopted", a)
    refused(ctx, o.call("POST", "conversations/%s/archive" % existing["conversation_id"], body={}, key=new_key()),
            409, "chief_of_staff", "adopted conversation is protected")
