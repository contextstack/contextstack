"""Embedded, loopback-only UI: one-time login links, browser sessions and the Client API seam."""
import subprocess
from http.cookies import SimpleCookie
from urllib.parse import parse_qs, urlsplit

from harness import wire
from harness.client import new_key
from harness.run import case
from cases.client import owner
from cases.conversations import created, reply

C = "client"


def link(ctx):
    target = ctx.target
    issued = subprocess.run([str(target.binary), "ui-link", "--data-dir", str(target.data),
                             "--listen", "127.0.0.1:%d" % target.port],
                            env=target.env, capture_output=True, text=True, check=True)
    url = issued.stdout.strip()
    parsed = urlsplit(url)
    ctx.expect(parsed.scheme == "http" and parsed.netloc == "127.0.0.1:%d" % target.port
               and parsed.path == "/ui/" and not parsed.query, "loopback URL with fragment code")
    code = parse_qs(parsed.fragment).get("code", [""])[0]
    ctx.expect(bool(code), "login URL contains a code")
    return code


def call(ctx, method, path, cookie=None, csrf=None, body=None, key=None, headers=None):
    send = {"Origin": ctx.target.url, "Sec-Fetch-Site": "same-origin"}
    if cookie:
        send["Cookie"] = cookie
    if csrf is not None:
        send["X-CSRF-Token"] = csrf
    if key:
        send["Idempotency-Key"] = key
    send.update(headers or {})
    return wire.request(ctx.target.url, method, "/ui" + path, body=body, headers=send)


def paired(ctx):
    code = link(ctx)
    answer = call(ctx, "POST", "/pair", body={"code": code})
    ctx.expect(answer.status == 200, "code exchange succeeds", answer)
    cookie = answer.header("set-cookie")
    ctx.expect(bool(cookie), "code exchange sets a cookie")
    parsed = SimpleCookie(cookie)
    ctx.expect("cs_ui" in parsed, "session cookie name")
    session = parsed["cs_ui"]
    ctx.expect(session["httponly"] and session["samesite"].lower() == "strict"
               and session["path"] == "/ui" and session["max-age"] == "28800"
               and not session["domain"], "host-only, scoped, HttpOnly Strict cookie")
    ctx.expect(isinstance(answer.json, dict) and bool(answer.json.get("csrf")), "CSRF token returned")
    ctx.expect(not any(k in answer.json for k in ("token", "credential", "owner_credential")),
               "owner credential is absent")
    return "%s=%s" % (session.key, session.value), answer.json["csrf"], code


def csp(ctx, answer):
    policy = answer.header("content-security-policy") or ""
    directives = dict((part.strip().split(None, 1) + [""])[:2] for part in policy.split(";") if part.strip())
    ctx.expect(directives.get("default-src") == "'none'"
               and directives.get("script-src") == "'self'"
               and directives.get("style-src") == "'self'"
               and directives.get("frame-ancestors") == "'none'"
               and directives.get("base-uri") == "'none'"
               and "'unsafe-inline'" not in policy and "'unsafe-eval'" not in policy,
               "strict CSP", policy)
    ctx.expect(answer.header("cache-control") == "no-store", "UI responses are not cached")


@case(C, "ui", "ui-login-code-and-session")
def _(ctx):
    """A local login code exchanges once into a scoped cookie; the API never accepts that cookie."""
    denied = call(ctx, "GET", "/session")
    ctx.expect(denied.status == 401, "session required", denied)
    csp(ctx, denied)
    foreign_code = link(ctx)
    foreign = call(ctx, "POST", "/pair", body={"code": foreign_code},
                   headers={"Origin": "https://evil.example", "Sec-Fetch-Site": "cross-site"})
    ctx.expect(foreign.status == 403, "foreign origin cannot exchange a code", foreign)
    still_live = call(ctx, "POST", "/pair", body={"code": foreign_code})
    ctx.expect(still_live.status == 200, "origin refusal did not consume the code", still_live)
    cookie, csrf, code = paired(ctx)
    ctx.expect(call(ctx, "POST", "/pair", body={"code": code}).status == 403, "used code is refused")
    ctx.expect(call(ctx, "POST", "/pair", body={"code": "not-a-code"}).status == 403, "unknown code is refused")
    current = call(ctx, "GET", "/session", cookie=cookie)
    ctx.expect(current.status == 200 and current.json["csrf"] == csrf, "session returns its CSRF token", current)
    csp(ctx, current)
    direct = wire.request(ctx.target.url, "GET", "/api/v1/client/nodes", headers={"Cookie": cookie})
    ctx.expect(direct.status == 401, "cookie is not an API credential", direct)
    invalid = call(ctx, "GET", "/session", cookie="cs_ui=invalid")
    ctx.expect(invalid.status == 401, "unknown session is refused", invalid)


@case(C, "ui", "ui-assets-and-security-headers")
def _(ctx):
    """The embedded page and local assets need no external resources and carry the UI policy."""
    for path, kind in (("/", "text/html"), ("/app.js", "javascript"), ("/style.css", "text/css")):
        asset = call(ctx, "GET", path)
        ctx.expect(asset.status == 200 and kind in (asset.header("content-type") or ""), "embedded asset " + path, asset)
        csp(ctx, asset)
        ctx.expect(ctx.target.local_token().encode() not in asset.body, "no owner credential in assets")
    html = call(ctx, "GET", "/").body.decode()
    ctx.expect("/ui/app.js" in html and "/ui/style.css" in html, "page loads local script and style")
    ctx.expect("onclick=" not in html.lower() and "onsubmit=" not in html.lower(), "no inline handlers")
    cookie, csrf, _ = paired(ctx)
    foreign = call(ctx, "GET", "/api/nodes", cookie=cookie,
                   headers={"Origin": "https://evil.example", "Sec-Fetch-Site": "cross-site"})
    ctx.expect(foreign.status == 403, "foreign browser origin refused", foreign)
    # The UI exposes only its small read/write surface, even with a valid browser session.
    expanded = call(ctx, "POST", "/api/pairings", cookie=cookie, csrf=csrf, body={"name": "escape"})
    ctx.expect(expanded.status in (403, 404, 405), "UI cannot mint credentials", expanded)


@case(C, "ui", "ui-lists-and-conversation-thread")
def _(ctx):
    """Status and conversation reads expose nodes, recent jobs, attention, and complete turns."""
    node, client = ctx.node2(), owner(ctx)
    conv = created(ctx, client, node, title="A newcomer conversation")
    cid = conv["conversation_id"]
    question = "May I land this change? <script>unsafe()</script>"
    attention = client.call("PUT", "conversations/%s/attention" % cid, body={"question": question})
    ctx.expect(attention.status == 200, "attention fixture", attention)
    cookie, _, _ = paired(ctx)
    nodes = call(ctx, "GET", "/api/nodes", cookie=cookie)
    ctx.expect(nodes.status == 200 and any(n["name"] == node.name for n in nodes.json["nodes"]), "node listed", nodes)
    listed = call(ctx, "GET", "/api/conversations", cookie=cookie)
    ctx.expect(listed.status == 200, "conversations list", listed)
    found = next(c for c in listed.json["conversations"] if c["conversation_id"] == cid)
    ctx.expect(found["title"] == "A newcomer conversation" and found["state"] == "idle"
               and found["attention"]["question"] == question, "title, state and needs-you question", found)
    thread = call(ctx, "GET", "/api/conversations/%s/thread" % cid, cookie=cookie)
    ctx.expect(thread.status == 200 and thread.json["conversation"]["conversation_id"] == cid
               and thread.json["turns"] == [], "conversation thread", thread)
    job = node.offer()
    jobs = call(ctx, "GET", "/api/jobs", cookie=cookie)
    ctx.expect(jobs.status == 200 and any(j["job_id"] == job for j in jobs.json["jobs"]), "recent jobs", jobs)


@case(C, "ui", "ui-post-turn-csrf-and-replay")
def _(ctx):
    """Posting needs both the session and its CSRF token and uses Client API turn idempotency."""
    node, client = ctx.node2(), owner(ctx)
    conv = created(ctx, client, node)
    cid = conv["conversation_id"]
    path = "/api/conversations/%s/turns" % cid
    cookie, csrf, _ = paired(ctx)
    body, key = {"message": "Hello from the embedded UI."}, new_key("ui-turn")
    _, another_csrf, _ = paired(ctx)
    for supplied in (None, "wrong", another_csrf):
        refused = call(ctx, "POST", path, cookie=cookie, csrf=supplied, body=body, key=key)
        ctx.expect(refused.status == 403, "missing or invalid CSRF is refused", refused)
        csp(ctx, refused)
    absent = call(ctx, "POST", path, csrf=csrf, body=body, key=key)
    ctx.expect(absent.status == 401, "CSRF alone grants no session", absent)
    posted = call(ctx, "POST", path, cookie=cookie, csrf=csrf, body=body, key=key)
    ctx.expect(posted.status == 201 and posted.json["turn"]["message"] == body["message"], "message posted", posted)
    repeated = call(ctx, "POST", path, cookie=cookie, csrf=csrf, body=body, key=key)
    ctx.expect(repeated.status == 200 and repeated.json["turn"]["turn_id"] == posted.json["turn"]["turn_id"],
               "same key creates only one turn", repeated)
    reply(ctx, node, posted.json["turn"]["job_id"], "Hello, newcomer.")
    thread = call(ctx, "GET", "/api/conversations/%s/thread" % cid, cookie=cookie)
    ctx.expect(thread.status == 200 and len(thread.json["turns"]) == 1
               and thread.json["turns"][0]["reply"] == "Hello, newcomer.", "posted message and reply appear", thread)


@case(C, "ui", "ui-logout-revokes-session")
def _(ctx):
    """Logout requires CSRF, expires the cookie and revokes only the selected browser session."""
    cookie, csrf, _ = paired(ctx)
    other_cookie, other_csrf, _ = paired(ctx)
    for supplied in (None, "wrong", other_csrf):
        refused = call(ctx, "POST", "/logout", cookie=cookie, csrf=supplied)
        ctx.expect(refused.status == 403, "logout requires the session's CSRF token", refused)
        ctx.expect(call(ctx, "GET", "/session", cookie=cookie).status == 200,
                   "CSRF refusal leaves session live")
    ctx.expect(call(ctx, "POST", "/logout", csrf=csrf).status == 401, "logout requires a session")
    ctx.expect(call(ctx, "GET", "/logout", cookie=cookie).status == 404, "GET cannot log out")
    logged_out = call(ctx, "POST", "/logout", cookie=cookie, csrf=csrf)
    ctx.expect(logged_out.status == 200, "logout succeeds", logged_out)
    csp(ctx, logged_out)
    cleared = SimpleCookie(logged_out.header("set-cookie") or "")
    ctx.expect("cs_ui" in cleared, "logout clears session cookie")
    session = cleared["cs_ui"]
    ctx.expect(session.value == "" and session["max-age"] == "0" and session["path"] == "/ui"
               and session["httponly"] and session["samesite"].lower() == "strict",
               "cleared cookie has matching scope and security attributes")
    for path in ("/session", "/api/nodes"):
        ctx.expect(call(ctx, "GET", path, cookie=cookie).status == 401,
                   "saved cookie cannot access " + path)
    ctx.expect(call(ctx, "POST", "/logout", cookie=cookie, csrf=csrf).status == 401,
               "saved cookie cannot be reused for logout")
    ctx.expect(call(ctx, "GET", "/session", cookie=other_cookie).status == 200,
               "other browser session remains live")
    html = call(ctx, "GET", "/").body.decode()
    ctx.expect('id="logout"' in html, "page provides logout button")


@case(C, "ui", "ui-create-default-conversation")
def _(ctx):
    """A browser can create a model conversation with an optional first turn."""
    node = ctx.node2()
    cookie, csrf, _ = paired(ctx)
    options = call(ctx, "GET", "/api/models", cookie=cookie)
    ctx.expect(options.status == 200 and options.json["default_model"] in
               [m["name"] for m in options.json["models"]], "browser gets model catalog and default", options)
    body = {"node": node.name, "model": options.json["default_model"], "title": "First conversation"}
    key = new_key("ui-create")
    denied = call(ctx, "POST", "/api/conversations", cookie=cookie, body=body, key=key)
    ctx.expect(denied.status == 403, "creation requires CSRF", denied)
    created = call(ctx, "POST", "/api/conversations", cookie=cookie, csrf=csrf, body=body, key=key)
    ctx.expect(created.status == 201, "default bundle created server-side", created)
    cid = created.json["conversation"]["conversation_id"]
    replay = call(ctx, "POST", "/api/conversations", cookie=cookie, csrf=csrf, body=body, key=key)
    ctx.expect(replay.status == 200 and replay.json["conversation"]["conversation_id"] == cid,
               "create replay reuses conversation", replay)
    empty = call(ctx, "GET", "/api/conversations/%s/thread" % cid, cookie=cookie)
    ctx.expect(empty.status == 200 and empty.json["turns"] == [],
               "new conversation is ready without a first message", empty)
    posted = call(ctx, "POST", "/api/conversations/%s/turns" % cid, cookie=cookie, csrf=csrf,
                  body={"message": "Hello"}, key=new_key("first-turn"))
    ctx.expect(posted.status == 201, "generated spec starts a turn", posted)
    reply(ctx, node, posted.json["turn"]["job_id"], "Welcome")
    thread = call(ctx, "GET", "/api/conversations/%s/thread" % cid, cookie=cookie)
    ctx.expect(thread.json["turns"][0]["reply"] == "Welcome", "conversation works", thread)
    bad = call(ctx, "POST", "/api/conversations", cookie=cookie, csrf=csrf,
               body={"node": node.name, "model": "bad\nmodel: paid"}, key=new_key("bad-model"))
    ctx.expect(bad.status == 422, "unlisted model refused", bad)
