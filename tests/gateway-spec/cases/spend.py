"""Pay-as-you-go source budgets (Director.Plan step 7 item 3, replaces the former
single daily total), WIRE Request budget. A source is the catalog endpoint id serving an
api-class model; the suite's fake-paid is served by `vllm`, so its source is "vllm". A source
is off until ~/.contextstack/config/gateway/limits.json gives it a budget
({"sources": {id: {"usd", "period": day|month}}}, "*" shared by all; the legacy
api_usd_per_day is "*" per day). Admission is before any connection: the call's estimate must
fit the request's budget.max_usd (a ceiling that only lowers) and the room left in every budget
that applies, counting estimates held by calls in flight. A refusal is fatal_session /
gateway.policy_denied at $0 with error.budget.reason off | exhausted | ceiling | invalid. Spend
is kept per source per day and month in ~/.contextstack/gateway/spend.json and survives a
restart. GET /v1/budgets reports each source's state. free_local and subscription calls are
never counted or refused."""
import concurrent.futures
import os
import datetime
import json

from harness.gateway import model_call
from harness.run import case, eq, no_egress, sub
from harness import upstreams
from cases.anthropic import NEEDS, messages_calls
from cases.streaming import CHUNK_V2
from cases.transport import sock_dir

DENIED = {"status": "error", "cost": {"currency": "USD", "amount": "0.000000"},
          "error": {"class": "fatal_session", "code": "gateway.policy_denied", "boundary": "gateway", "recoverable": False}}
POOL = concurrent.futures.ThreadPoolExecutor(max_workers=4)
SRC = "vllm"  # fake-paid's source


def denied(b, what, reason=None, **budget):
    sub(b, DENIED, what)
    if reason:
        sub(b["error"].get("budget"), dict(reason=reason, **budget), what + " budget")


def limits(usd, period="day", source=SRC, **more):
    return {"sources": dict({source: {"usd": usd, "period": period}}, **more)}


def now():
    return datetime.datetime.now().astimezone()


def next_midnight():
    return (now().date() + datetime.timedelta(days=1)).isoformat() + "T00:00:00"


def next_month():
    d = now().date()
    return (datetime.date(d.year + 1, 1, 1) if d.month == 12 else datetime.date(d.year, d.month + 1, 1)).isoformat() + "T00:00:00"


def spend(gw):
    try:
        return json.loads(gw.spend_path.read_text())
    except FileNotFoundError:
        return None


def spent(gw, source=SRC):
    s = (spend(gw) or {}).get("sources", {}).get(source, {})
    return round(s.get("day_usd", 0), 6), round(s.get("month_usd", 0), 6)


def paid(t, gw, usage, budget=None, request_id="spec-req-1"):
    t.vllm.chat["fake-paid"] = {"json": upstreams.chat_completion(usage=usage)}
    return gw.call(model_call("fake-paid", budget=budget or {}, request_id=request_id)).json


def budgets(gw):
    r = gw.get("/v1/budgets")
    eq(r.status, 200, "GET /v1/budgets")
    b = r.json
    eq(b["schema_version"], "csgateway.budgets.v1", "budgets schema")
    return b, {s["source"]: s for s in b["sources"]}


@case("spend", gw=None)
def api_refused_at_zero(t):
    """No budget for the source (no file, $0 legacy, empty, no sources, the source at $0, only another source budgeted): every api call is refused 'off' before any connection, unary and stream; $0, nothing counted."""
    for name, doc in (("no-file", None), ("zero", {"api_usd_per_day": 0}), ("empty", {}), ("no-sources", {"sources": {}}),
                      ("source-zero", limits(0)), ("other-source", limits(5, source="openrouter"))):
        gw = t.gateway("spend-" + name, limits=doc)
        denied(gw.call(model_call("fake-paid")).json, name + " unary", "off", source=SRC)
        frames = gw.call(model_call("fake-paid", stream=True)).sse()
        eq(len(frames), 1, name + " stream frames")
        sub(frames[0], {"schema_version": CHUNK_V2, "kind": "error", "cost": DENIED["cost"], "error": dict(DENIED["error"], budget={"reason": "off"})}, name + " stream")
        eq(t.run.calls(t.vllm), [], name + " upstream requests")
        eq(spend(gw), None, name + " nothing counted")
        gw.cleanup()


@case("spend", gw=None, needs=NEEDS)
def api_refused_at_zero_cloud(t):
    """A cloud api source (Anthropic, unary and stream) with no budget is refused 'off' before the gateway opens a connection to it."""
    store = t.run.credential_store("spend-cloud", anthropic="sk-ant-spec-FAKE")
    gw = t.gateway("spend-cloud", env={"CSGATEWAY_CREDENTIALS": str(store)}, limits=None)
    denied(gw.call(model_call("claude-spec")).json, "unary", "off", source="anthropic")
    frames = gw.call(model_call("claude-spec", stream=True)).sse()
    sub(frames[0], {"kind": "error", "error": dict(DENIED["error"], budget={"reason": "off", "source": "anthropic"})}, "stream")
    eq(messages_calls(t), [], "no Messages request")
    no_egress(t)


@case("spend", gw=None)
def source_budget_active_then_exhausted(t):
    """A source budget admits calls until its spend reaches it, then refuses 'exhausted' (unary and stream) with the period and its reset at the next local midnight; local stays open."""
    gw = t.gateway("spend-active", limits=limits(0.005))
    sub(paid(t, gw, (400, 100)), {"status": "ok", "cost": {"class": "api", "charged_usd": 0.005}}, "within the budget")
    eq(spent(gw), (0.005, 0.005), "counted for the day and the month")
    b = paid(t, gw, (400, 100), request_id="second")
    denied(b, "used up", "exhausted", source=SRC, limit=SRC, period="day", usd=0.005, spent_usd=0.005, held_usd=0)
    eq(b["error"]["budget"]["resets_at"].startswith(next_midnight()), True, "resets_at is the next local midnight: %s" % b["error"]["budget"]["resets_at"])
    frames = gw.call(model_call("fake-paid", stream=True)).sse()
    sub(frames[0], {"kind": "error", "error": {"code": "gateway.policy_denied", "budget": {"reason": "exhausted"}}}, "stream")
    eq(len(t.run.calls(t.vllm)), 1, "upstream requests")
    sub(gw.call(model_call("local-coder")).json, {"status": "ok"}, "free_local unaffected")


@case("spend", gw=None)
def ceiling_lowers_only(t):
    """budget.max_usd is a ceiling: it never turns a source on or raises a used-up budget, and a call whose estimate is over it is refused 'ceiling' with nothing spent."""
    gw = t.gateway("spend-ceiling", limits=limits(0.005))
    sub(paid(t, gw, (400, 100), {"max_usd": "1"}), {"status": "ok"}, "under the ceiling and the budget")
    denied(paid(t, gw, (400, 100), {"max_usd": "100"}, "raise"), "a large max_usd does not raise the budget", "exhausted")
    gw.cleanup()
    gw = t.gateway("spend-ceiling-off", limits=None)
    denied(paid(t, gw, (400, 100), {"max_usd": "100"}), "a max_usd does not turn a source on", "off")
    gw.cleanup()
    gw = t.gateway("spend-ceiling-low", limits=limits(10))
    before = len(t.run.calls(t.vllm))
    b = gw.call(model_call("fake-paid", budget={"max_usd": "0.5", "max_output_tokens": 100000})).json
    denied(b, "estimate over the ceiling", "ceiling", source=SRC, max_usd=0.5)
    eq(b["error"]["budget"]["estimate_usd"] >= 1, True, "estimate counts the output ceiling")
    denied(gw.call(model_call("fake-paid", budget={"max_usd": "0"})).json, "a $0 ceiling", "ceiling")
    eq(len(t.run.calls(t.vllm)), before, "no upstream request")
    eq(spend(gw), None, "nothing counted")
    sub(paid(t, gw, (400, 100), {"max_usd": "0.5"}), {"status": "ok"}, "a ceiling above the estimate admits")


@case("spend", gw=None)
def concurrent_last_dollar(t):
    """Two calls at once with room for one: the first holds its estimate while in flight, so the second is refused 'exhausted' before any connection; once the first settles, the hold is released."""
    gw = t.gateway("spend-concurrent", limits=limits(1.5))
    big = {"max_output_tokens": 100000}  # an estimate a little over $1 each
    t.vllm.chat["fake-paid"] = {"hold": "paid", "then": {"json": upstreams.chat_completion(usage=(300, 100))}}
    first = POOL.submit(gw.call, model_call("fake-paid", budget=big, request_id="first"))
    t.vllm.events.wait(lambda ev: ev, 5)
    b = gw.call(model_call("fake-paid", budget=big, request_id="second")).json
    denied(b, "second while the first is in flight", "exhausted", spent_usd=0)
    eq(b["error"]["budget"]["held_usd"] >= 1, True, "the first call's estimate is held")
    t.vllm.release("paid")
    sub(first.result(15).json, {"status": "ok", "cost": {"charged_usd": 0.004}}, "first")
    eq(spent(gw), (0.004, 0.004), "only the charge is counted")
    sub(gw.call(model_call("fake-paid", budget=big, request_id="third")).json, {"status": "ok"}, "the hold is released after settling")
    eq(len(t.run.calls(t.vllm)), 2, "upstream requests")


@case("spend", gw=None)
def api_counter_survives_restart(t):
    """Per-source spend is persisted: after a restart a used-up budget still refuses. Another day's counter is $0 for a day budget; this month's spend still counts for a month budget."""
    gw = t.gateway("spend-restart", limits=limits(0.01))
    sub(paid(t, gw, (600, 400)), {"status": "ok", "cost": {"charged_usd": 0.01}}, "first call spends the budget")
    gw.stop()
    gw.start()
    denied(paid(t, gw, (600, 400), request_id="after-restart"), "after restart", "exhausted")
    eq(len(t.run.calls(t.vllm)), 1, "upstream requests")
    gw.stop()
    month = now().strftime("%Y-%m")
    gw.spend_path.write_text(json.dumps({"schema_version": "csgateway.spend.v2", "day": "2000-01-01", "month": month,
                                         "sources": {SRC: {"day_usd": 5, "month_usd": 5}}}))
    gw.start()
    sub(paid(t, gw, (600, 400), request_id="new-day"), {"status": "ok"}, "yesterday's spend is not today's")
    eq(spent(gw), (0.01, 5.01), "day reset, month kept")
    eq((spend(gw)["day"], spend(gw)["month"]), (now().date().isoformat(), month), "counter dated today")
    gw.limits_path.write_text(json.dumps(limits(5, "month")))
    b = paid(t, gw, (600, 400), request_id="month")
    denied(b, "a month budget counts the month", "exhausted", period="month")
    eq(b["error"]["budget"]["resets_at"].startswith(next_month()), True, "resets on the first of next month")


@case("spend", gw=None)
def legacy_daily_total(t):
    """The legacy limits.json {api_usd_per_day} keeps working as one daily budget shared by every source ("*"), and a legacy counter's spend today counts against it."""
    gw = t.gateway("spend-config", limits={"api_usd_per_day": 0.005})
    sub(paid(t, gw, (400, 100)), {"status": "ok", "cost": {"charged_usd": 0.005}}, "within the day's limit")
    denied(paid(t, gw, (400, 100), request_id="second"), "past the day's limit", "exhausted", limit="*", period="day")
    eq(len(t.run.calls(t.vllm)), 1, "upstream requests")
    _, rows = budgets(gw)
    sub(rows[SRC], {"state": "exhausted", "shared": True, "budget": None}, "the source under the shared total")
    gw.cleanup()
    gw = t.gateway("spend-legacy-counter", limits={"api_usd_per_day": 0.01}, expect_start=False)
    gw.spend_path.parent.mkdir(parents=True, exist_ok=True)
    gw.spend_path.write_text(json.dumps({"day": now().date().isoformat(), "api_usd": 0.01}))
    gw.start()
    denied(paid(t, gw, (400, 100)), "a legacy counter's spend counts", "exhausted", limit="*")
    b, _ = budgets(gw)
    sub(b["total"], {"usd": 0.01, "period": "day", "spent_usd": 0.01, "remaining_usd": 0}, "total")


@case("spend", gw=None)
def budgets_report(t):
    """GET /v1/budgets (bearer auth like /v1/models) lists every pay-as-you-go source: off with no budget, active with what remains, exhausted with its reset; never a key."""
    store = t.run.credential_store("spend-report", anthropic="sk-ant-spec-FAKE", openai="sk-spec-FAKE")
    gw = t.gateway("spend-report", env={"CSGATEWAY_CREDENTIALS": str(store)}, auth_token="spec-token",
                   limits=limits(0.005, openrouter={"usd": 20, "period": "month"}))
    eq(gw.get("/v1/budgets", auth=False).status, 401, "auth required")
    b, rows = budgets(gw)
    eq("FAKE" in json.dumps(b), False, "no key in the report")
    eq(b["total"], None, "no shared budget")
    for off in ("anthropic", "openai", "gemini"):
        sub(rows[off], {"state": "off", "budget": None}, off)
    sub(rows[SRC], {"state": "active", "remaining_usd": 0.005, "shared": False,
                    "budget": {"usd": 0.005, "period": "day", "spent_usd": 0, "held_usd": 0, "remaining_usd": 0.005}}, SRC)
    eq(rows[SRC]["budget"]["resets_at"].startswith(next_midnight()), True, "day resets_at")
    sub(rows["openrouter"], {"state": "active", "budget": {"usd": 20, "period": "month"}}, "openrouter")
    eq(rows["openrouter"]["budget"]["resets_at"].startswith(next_month()), True, "month resets_at")
    sub(paid(t, gw, (400, 100)), {"status": "ok"}, "spend it")
    _, rows = budgets(gw)
    sub(rows[SRC], {"state": "exhausted", "remaining_usd": 0, "spent": {"day_usd": 0.005, "month_usd": 0.005}}, "used up")
    eq(rows[SRC]["resets_at"].startswith(next_midnight()), True, "exhausted names its reset")
    gw.limits_path.write_text("{not json")
    b, rows = budgets(gw)
    sub(rows[SRC], {"state": "invalid"}, "unreadable limit file")
    eq(bool(b.get("error")), True, "the report says why")


@case("spend", gw=None)
def local_models_not_counted(t):
    """With no budget at all, free_local (vLLM, Ollama, even with a provider-reported charge) and subscription calls dispatch and are never counted."""
    gw = t.gateway("spend-local", limits=None)
    t.vllm.chat["vllm-big"] = {"json": upstreams.chat_completion(usage=(100, 100), cost=0.25)}
    for name in ("local-coder", "vllm-big", "ollama-small", "fake-sub", ""):
        sub(gw.call(model_call(name)).json, {"status": "ok"}, name or "empty hint")
    eq(spend(gw), None, "nothing counted")


@case("spend", gw=None)
def invalid_limits_fail_closed(t):
    """An unreadable limit file, budget.max_usd or counter refuses api calls 'invalid' ($0, no connection) and leaves local calls alone."""
    for name, doc in (("garbage", "{not json"), ("negative", {"api_usd_per_day": -1}),
                      ("string", {"api_usd_per_day": "5"}), ("unknown-key", {"api_usd_per_day": 5, "usd": 5}),
                      ("no-period", {"sources": {SRC: {"usd": 1}}}), ("bad-period", limits(1, "week")),
                      ("negative-source", limits(-1)), ("array", "[]"), ("both-totals", {"api_usd_per_day": 1, "sources": {"*": {"usd": 1, "period": "day"}}})):
        gw = t.gateway("spend-" + name, limits=doc)
        denied(gw.call(model_call("fake-paid")).json, name, "invalid")
        sub(gw.call(model_call("local-coder")).json, {"status": "ok"}, name + ": local")
        gw.cleanup()
    gw = t.gateway("spend-bad-budget", limits=limits(1))
    for raw in ("abc", "-1", "NaN"):
        denied(gw.call(model_call("fake-paid", budget={"max_usd": raw})).json, "max_usd " + raw, "invalid")
    gw.spend_path.parent.mkdir(parents=True, exist_ok=True)
    gw.spend_path.write_text("{not json")
    denied(gw.call(model_call("fake-paid", budget={"max_usd": "1"})).json, "unreadable counter", "invalid")
    eq([c for c in t.run.calls(t.vllm) if c["json"]["model"] == "fake-paid"], [], "no paid upstream request")


OWNER_HEADER = "X-ContextStack-Owner-Token"


def owner_token(gw):
    """The gateway's owner token under its temp HOME (never the real ~/.contextstack)."""
    return (gw.home / ".contextstack/config/gateway/owner-token").read_text().strip()


def put_budgets(gw, doc, owner=True, headers=None, **kw):
    raw = doc if isinstance(doc, (bytes, str)) else json.dumps(doc)
    h = dict(headers or {})
    if owner is True:
        h[OWNER_HEADER] = owner_token(gw)
    elif owner:
        h[OWNER_HEADER] = owner
    return gw.request("PUT", "/v1/budgets", raw=raw.encode() if isinstance(raw, str) else raw, headers=h, **kw)


@case("spend", gw=None)
def set_budgets(t):
    """PUT /v1/budgets replaces the limit file with a whole document, written in its canonical form, and answers the new state report; the next call and GET see it. {} turns every source off."""
    gw = t.gateway("spend-set", auth_token="spec-token", limits=None)
    denied(gw.call(model_call("fake-paid")).json, "off before", "off")
    r = put_budgets(gw, {"sources": {"VLLM": {"usd": 0.005, "period": "day"}, "openrouter": {"usd": 20, "period": "month"}}})
    eq(r.status, 200, "set")
    rows = {s["source"]: s for s in r.json["sources"]}
    sub(rows[SRC], {"state": "active", "budget": {"usd": 0.005, "period": "day"}}, "the reply is the new report")
    eq(json.loads(gw.limits_path.read_text()), {"sources": {"openrouter": {"usd": 20, "period": "month"}, SRC: {"usd": 0.005, "period": "day"}}}, "canonical file")
    eq(oct(gw.limits_path.stat().st_mode & 0o777), oct(0o600), "file mode")
    sub(paid(t, gw, (400, 100)), {"status": "ok", "cost": {"charged_usd": 0.005}}, "the next call is admitted")
    _, rows = budgets(gw)
    sub(rows[SRC], {"state": "exhausted"}, "GET sees the spend")
    r = put_budgets(gw, {"api_usd_per_day": 1})
    eq(r.status, 200, "replace with the legacy shape")
    eq(json.loads(gw.limits_path.read_text()), {"sources": {"*": {"usd": 1, "period": "day"}}}, "legacy written as the shared daily budget")
    eq(r.json["total"]["usd"], 1, "the report's total")
    sub(paid(t, gw, (400, 100), request_id="shared"), {"status": "ok"}, "replaced: the shared budget admits")
    eq(put_budgets(gw, {}).status, 200, "empty document")
    _, rows = budgets(gw)
    sub(rows[SRC], {"state": "off"}, "{} turns every source off")
    denied(paid(t, gw, (400, 100), request_id="after-off"), "off after", "off")


@case("spend", gw=None)
def set_budgets_invalid_rejected(t):
    """An invalid budgets document is 400 invalid_budgets and writes nothing: the file is byte-identical and the budgets in force are unchanged."""
    gw = t.gateway("spend-set-invalid", auth_token="spec-token", limits=limits(1))
    before = gw.limits_path.read_bytes()
    for name, doc in (("garbage", "{not json"), ("no-period", {"sources": {SRC: {"usd": 1}}}), ("bad-period", limits(1, "week")),
                      ("negative", limits(-1)), ("string", {"api_usd_per_day": "5"}), ("unknown-key", {"budgets": {}}),
                      ("both-totals", {"api_usd_per_day": 1, "sources": {"*": {"usd": 1, "period": "day"}}}), ("array", "[]")):
        r = put_budgets(gw, doc)
        eq(r.status, 400, name + " status")
        eq(r.json.get("code"), "invalid_budgets", name + " code")
        eq(gw.limits_path.read_bytes(), before, name + " file unchanged")
    _, rows = budgets(gw)
    sub(rows[SRC], {"state": "active", "budget": {"usd": 1}}, "budgets in force unchanged")
    eq(put_budgets(gw, b"x" * (65 * 1024)).status, 413, "oversized body")
    eq(gw.limits_path.read_bytes(), before, "oversized: file unchanged")


@case("spend", gw=None)
def set_budgets_owner_only(t):
    """With auth on, the shared bearer token is not enough to write: no owner token, a wrong one, or the shared token in the owner header is 403 owner_only and writes nothing; the owner token (plus the shared token every route needs) is accepted. GET stays on the shared token. /v1/budgets answers only GET/HEAD/PUT."""
    gw = t.gateway("spend-set-auth", auth_token="spec-token", limits=None)
    for name, owner in (("no owner token", None), ("wrong owner token", "0" * 64), ("the shared token", "spec-token")):
        r = put_budgets(gw, limits(5), owner=owner)
        eq((r.status, r.json.get("code")), (403, "owner_only"), name)
    eq(put_budgets(gw, limits(5), auth=False).status, 401, "owner token without the shared token: the route's auth still applies")
    eq(gw.limits_path.exists(), False, "nothing written")
    eq(put_budgets(gw, limits(5)).status, 200, "the owner token is accepted")
    eq(gw.get("/v1/budgets").status, 200, "reads stay on the shared token")
    r = gw.request("POST", "/v1/budgets", body={})
    eq((r.status, r.headers.get("allow")), (405, "GET, HEAD, PUT"), "POST not allowed")


@case("spend", gw=None)
def set_budgets_socket_needs_owner(t):
    """With auth off, the tokenless unix socket (what a sandboxed net: gateway tool reaches) and loopback TCP cannot write without the owner token; with it, both are accepted."""
    gw = t.gateway("spend-set-noauth", socket_path=os.path.join(sock_dir(t), "gw.sock"), limits=None)
    for via in (True, False):
        r = put_budgets(gw, limits(5), owner=None, via_socket=via)
        eq((r.status, r.json.get("code")), (403, "owner_only"), "no owner token, socket=%s" % via)
    eq(gw.limits_path.exists(), False, "nothing written")
    r = put_budgets(gw, limits(5), via_socket=True)
    eq(r.status, 200, "socket with the owner token")
    sub({s["source"]: s for s in r.json["sources"]}[SRC], {"state": "active"}, "set over the socket")
    eq(put_budgets(gw, limits(6)).status, 200, "loopback TCP with the owner token")
    eq(gw.get("/v1/budgets", via_socket=True).status, 200, "reads over the socket stay open")


@case("spend", gw=None)
def owner_token_file(t):
    """The owner token is created once at first start: 64 hex characters, 0600 in a 0700 directory, kept across restarts, and never logged."""
    gw = t.gateway("spend-owner-file", limits=None)
    path = gw.home / ".contextstack/config/gateway/owner-token"
    token = owner_token(gw)
    eq((len(token), all(c in "0123456789abcdef" for c in token)), (64, True), "64 hex characters")
    eq(oct(path.stat().st_mode & 0o777), oct(0o600), "file mode")
    eq(oct(path.parent.stat().st_mode & 0o777), oct(0o700), "directory mode")
    gw.stop()
    gw.start()
    eq(owner_token(gw), token, "kept across a restart")
    eq(token in gw.log_text(), False, "never logged")
    eq(gw.log_text().count("starting on"), 2, "the log holds both starts")
    eq(gw.log_text().count("owner token created"), 1, "created once")
