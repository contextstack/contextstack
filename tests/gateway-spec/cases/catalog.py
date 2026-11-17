"""Provider evolution, catalog side (V2.1 phase 5): layered catalog
(embedded, host overlay dir), merge rules and rejects, strict decoding,
revisions, atomic reload, per-call revision binding, the model cache and
the DGX overlay ids. Overlay files live in a temp dir named by
CSGATEWAY_PROVIDERS_DIR; nothing here reaches a real DGX or provider."""
import json
import re
import signal
import threading
import time

from harness.gateway import SPEC, model_call
from harness.run import case, eq, no_egress, sub
from harness import upstreams

OVERLAY = "csgateway.providers.v1"
EMBEDDED = ["anthropic", "gemini", "groq", "lambda", "mock", "ollama", "openai", "openrouter", "together", "typesafe", "vllm"]
REV = re.compile(r"^r-[0-9a-f]{12}$")
BOX_CAPS = {"images": False, "tools": True, "streaming": False}


def overlay_dir(t, name, files):
    """A fresh overlay directory holding files {name: obj or raw text}."""
    d = t.run.tmp / ("providers-" + name)
    d.mkdir()
    write(d, files)
    return d


def write(d, files):
    for f, body in files.items():
        p = d / f
        if body is None:
            p.unlink()
        else:
            p.write_text(body if isinstance(body, str) else json.dumps(body))


def ov(providers=(), **top):
    doc = {"schema": OVERLAY, "providers": list(providers)}
    doc.update(top)
    return doc


def box(url, name="spec-box", **kw):
    e = {"name": name, "protocol": "openai_chat_completions", "endpoint": url + "/v1", "host": "box-1", "capabilities": dict(BOX_CAPS)}
    e.update(kw)
    return e


BOX_MODEL = {"name": "box-model", "provider": "spec-box", "tier": 2, "capability": "general"}


def catalog(gw):
    return gw.get("/v1/catalog").json


def endpoints(gw):
    return {e["id"]: e for e in catalog(gw)["endpoints"]}


def revision(gw):
    return gw.get("/v1/health").json["catalog_revision"]


def held_call(gw, body):
    """Start a call on a thread; returns (thread, result dict)."""
    out = {}
    th = threading.Thread(target=lambda: out.setdefault("r", gw.call(body)), daemon=True)
    th.start()
    return th, out


@case("catalog")
def catalog_status_shape(t):
    """GET /v1/catalog without overlays: the embedded layer only (vllm-spark2 is no longer compiled in), revision r-<12 hex> shared with /v1/health, seq 1, no tenant layer, last reload ok."""
    c = catalog(t.gw)
    sub(c, {"schema": "csgateway.catalog_status.v1", "seq": 1, "overlay_files": [], "policy": {"state": "none", "local_layers": 0},
            "last_reload": {"ok": True, "revision": c["revision"]}}, "catalog")
    eq(bool(REV.match(c["revision"])), True, "revision format " + c["revision"])
    eq(revision(t.gw), c["revision"], "/v1/health catalog_revision")
    e = {x["id"]: x for x in c["endpoints"]}
    eq(sorted(e), EMBEDDED, "endpoint ids")
    eq({k: v["protocol"] for k, v in e.items() if k in ("anthropic", "openai", "vllm", "mock")},
       {"anthropic": "anthropic_messages", "openai": "openai_chat_completions", "vllm": "openai_chat_completions", "mock": "mock"}, "protocols")
    eq({k: v["source"] for k, v in e.items() if k in ("anthropic", "openai", "groq")}, {"anthropic": "embedded", "openai": "embedded", "groq": "embedded"}, "sources")
    eq(e["anthropic"]["capabilities"], {"images": True, "tools": True, "streaming": True}, "anthropic protocol constant")
    eq(e["openai"]["capabilities"], {"images": False, "tools": True, "streaming": True}, "openai_chat_completions protocol constant")
    eq(e["mock"]["capabilities"], {"images": False, "tools": True, "streaming": True}, "mock protocol constant")
    eq({v["egress"] for v in e.values()}, {"allowed"}, "egress without policy")
    eq(sorted(x["source"] for x in c["model_cache"]), ["ollama", "vllm"], "model cache observed the discovered endpoints")


@case("catalog", gw=None)
def overlay_adds_endpoint(t):
    """A new overlay name adds a keyless endpoint: declared protocol, endpoint, host and capabilities; its models route there and only there; discovery covers it."""
    m, url = t.mock("box")
    m.models = ["box-model"]
    d = overlay_dir(t, "add", {"box.json": ov([box(url)])})
    gw = t.gateway("ov-add", t.run.config(extra_models=[BOX_MODEL]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    c = catalog(gw)
    eq(c["overlay_files"], ["box.json"], "overlay files")
    sub({x["id"]: x for x in c["endpoints"]}["spec-box"], {"protocol": "openai_chat_completions", "endpoint": url + "/v1", "host": "box-1",
                                                          "source": "overlay:box.json", "capabilities": BOX_CAPS, "egress": "allowed"}, "spec-box")
    b = gw.call(model_call("box-model")).json
    sub(b, {"status": "ok", "provider": "spec-box", "model": "box-model", "cost": {"amount": "0.000000"}}, "call")
    eq([c["json"]["model"] for c in t.run.calls(m)], ["box-model"], "box upstream")
    eq(t.run.calls(t.vllm) + t.run.calls(t.ollama), [], "no other upstream")
    h = {e["endpoint_id"]: e for e in gw.get("/v1/discovery/health").json["endpoints"]}
    sub(h["spec-box"], {"mode": "refresh", "state": "fresh", "model_count": 1}, "spec-box discovery")
    no_egress(t)


@case("catalog", gw=None)
def overlay_patches_endpoint(t):
    """An overlay entry with an embedded name patches endpoint, host and capabilities; the protocol may be restated but stays; calls go to the patched endpoint, never the compiled-in one."""
    m, url = t.mock("patched-openai")
    store = t.run.credential_store("patch", openai="sk-spec-FAKE")
    d = overlay_dir(t, "patch", {"p.json": ov([{"name": "openai", "protocol": "openai_chat_completions", "endpoint": url + "/v1",
                                                "host": "box-2", "capabilities": {"images": False, "tools": False, "streaming": False}}])})
    gw = t.gateway("ov-patch", env={"CSGATEWAY_PROVIDERS_DIR": str(d), "CSGATEWAY_CREDENTIALS": str(store)})
    sub(endpoints(gw)["openai"], {"protocol": "openai_chat_completions", "endpoint": url + "/v1", "host": "box-2",
                                  "source": "embedded+overlay:p.json", "capabilities": {"images": False, "tools": False, "streaming": False}}, "openai")
    sub(gw.call(model_call("gpt-spec")).json, {"status": "ok", "provider": "openai", "model": "gpt-spec"}, "call")
    eq([c["json"]["model"] for c in t.run.calls(m)], ["gpt-spec"], "patched upstream")
    eq(t.run.calls(t.openai), [], "compiled-in api.openai.com not called")
    no_egress(t)


@case("catalog", gw=None)
def overlay_env_override_wins(t):
    """The process-level local endpoint variable (set away from its default) beats an overlay patch of the same endpoint; the overlay's other fields still apply."""
    d = overlay_dir(t, "env", {"v.json": ov([{"name": "vllm", "endpoint": "http://127.0.0.1:9/v1", "host": "box-3"}])})
    gw = t.gateway("ov-env", env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    v = endpoints(gw)["vllm"]
    eq((v["endpoint"], v["host"], "overlay:v.json" in v["source"]), (t.run.vllm_url + "/v1", "box-3", True), "vllm endpoint, host, source")
    eq(gw.call(model_call("local-coder")).json["status"], "ok", "local-coder served by the env endpoint")
    eq(len(t.run.calls(t.vllm)), 1, "env upstream")


@case("catalog", gw=None)
def overlay_removes_endpoint(t):
    """"remove": true deletes an endpoint: gone from the catalog and discovery health; its models refuse at $0 with no upstream request and no egress, never falling through."""
    store = t.run.credential_store("remove", openai="sk-spec-FAKE")
    d = overlay_dir(t, "remove", {"r.json": ov([{"name": "openai", "remove": True}, {"name": "groq", "remove": True}])})
    gw = t.gateway("ov-remove", env={"CSGATEWAY_PROVIDERS_DIR": str(d), "CSGATEWAY_CREDENTIALS": str(store)})
    eq(sorted(endpoints(gw)), [x for x in EMBEDDED if x not in ("groq", "openai")], "endpoint ids")
    b = gw.call(model_call("gpt-spec")).json
    sub(b, {"status": "error", "cost": {"currency": "USD", "amount": "0.000000"},
              "error": {"class": "fatal_session", "code": "gateway.provider_not_configured", "recoverable": False}}, "removed endpoint's model")
    eq(bool(b["error"].get("class")) and bool(b["error"].get("code")), True, "classed error")
    eq(t.run.calls(t.openai) + t.run.calls(t.vllm) + t.run.calls(t.ollama), [], "no upstream")
    no_egress(t)


@case("catalog", gw=None)
def overlay_removed_endpoint_leaves_discovery(t):
    """A removed endpoint leaves discovery too: it is not reported in discovery health and its credential is not looked up, with or without a credential store."""
    store = t.run.credential_store("removed-disc", openai="sk-spec-FAKE")
    d = overlay_dir(t, "removed-disc", {"r.json": ov([{"name": "openai", "remove": True}, {"name": "groq", "remove": True}])})
    for name, env in (("no-store", {}), ("store", {"CSGATEWAY_CREDENTIALS": str(store)})):
        gw = t.gateway("ov-removed-" + name, env=dict(env, CSGATEWAY_PROVIDERS_DIR=str(d)))
        ids = sorted(e["endpoint_id"] for e in gw.get("/v1/discovery/health").json["endpoints"])
        eq(ids, [x for x in EMBEDDED if x not in ("groq", "openai")], name + ": discovery ids")


REJECTS = [
    # (what, files, words the startup log must contain)
    ("unknown field", {"a.json": ov([dict(box("http://127.0.0.1:9"), colour="red")])}, ["a.json", "colour"]),
    ("duplicate key, case-insensitive", {"a.json": '{"schema": "%s", "providers": [], "Schema": "x"}' % OVERLAY}, ["a.json", "Schema"]),
    ("trailing data", {"a.json": json.dumps(ov()) + " {}"}, ["a.json", "trailing"]),
    ("over 1 MiB", {"a.json": json.dumps(ov()) + " " * (1 << 20)}, ["a.json", "1048576"]),
    ("wrong schema", {"a.json": {"schema": "csgateway.providers.v0", "providers": []}}, ["a.json", "csgateway.providers.v0"]),
    ("YAML", {"a.yaml": "schema: csgateway.providers.v1\n"}, ["a.yaml"]),
    ("new endpoint without capabilities", {"a.json": ov([{k: v for k, v in box("http://127.0.0.1:9").items() if k != "capabilities"}])}, ["spec-box", "capabilities"]),
    ("requires_key on an overlay endpoint", {"a.json": ov([dict(box("http://127.0.0.1:9"), requires_key=True)])}, ["spec-box", "requires_key"]),
    ("default_cost_per_1k on an overlay endpoint", {"a.json": ov([dict(box("http://127.0.0.1:9"), default_cost_per_1k=0.1)])}, ["spec-box", "default_cost_per_1k"]),
    ("protocol change", {"a.json": ov([{"name": "openai", "protocol": "anthropic_messages"}])}, ["openai", "protocol"]),
    ("remove of an unknown id", {"a.json": ov([{"name": "nosuch", "remove": True}])}, ["nosuch", "remove"]),
    ("remove with other fields", {"a.json": ov([{"name": "groq", "remove": True, "host": "x"}])}, ["groq", "remove"]),
    ("one id in two files", {"a.json": ov([box("http://127.0.0.1:9")]), "b.json": ov([box("http://127.0.0.1:9")])}, ["spec-box", "a.json", "b.json"]),
    ("overlay model collides with shipped", {"a.json": {"schema": "csgateway.providers.v2", "providers": [], "models": [{"name": "local-coder", "provider": "vllm", "tier": 1, "capability": "coding"}]}}, ["a.json", "local-coder", "models.json"]),
    ("non-http endpoint", {"a.json": ov([box("ftp://127.0.0.1:9")])}, ["ftp://127.0.0.1:9/v1"]),
    ("credentials in the endpoint", {"a.json": ov([box("http://u:p@127.0.0.1:9")])}, ["spec-box", "endpoint"]),
    ("invalid id", {"a.json": ov([box("http://127.0.0.1:9", name="Spec Box")])}, ["Spec Box"]),
    ("unknown protocol", {"a.json": ov([box("http://127.0.0.1:9", protocol="grpc")])}, ["grpc"]),
    ("invalid host pattern", {"a.json": ov(policy={"egress": {"deny_hosts": ["Example.com"]}})}, ["Example.com"]),
]


@case("catalog", gw=None)
def overlay_rejects_fail_startup(t):
    """One strict decoder for every layer: each invalid overlay (unknown field, duplicate key, trailing data, >1 MiB, schema, YAML, merge-rule violation) refuses startup, naming the cause; no partial catalog is served."""
    failures = []
    for i, (what, files, words) in enumerate(REJECTS):
        d = overlay_dir(t, "reject-%d" % i, files)
        code, log = t.gateway("ov-reject", env={"CSGATEWAY_PROVIDERS_DIR": str(d)}, expect_start=False).start_fails()
        missing = [w for w in words if w not in log]
        if code == 0 or missing:
            failures.append("%s: exit %s, log lacks %s" % (what, code, missing))
    eq(failures, [], "rejects")
    code, log = t.gateway("ov-relative", env={"CSGATEWAY_PROVIDERS_DIR": "rel/dir"}, expect_start=False).start_fails()
    eq((code != 0, "CSGATEWAY_PROVIDERS_DIR" in log), (True, True), "relative overlay dir refused")


@case("catalog", gw=None)
def overlay_dir_default_off_and_missing(t):
    """The overlay dir defaults to ~/.contextstack/gateway/providers.d; a missing dir is an empty layer; off disables the layer; non-JSON and dot files are ignored."""
    m, url = t.mock("default-box")
    gw = t.gateway("ov-default", expect_start=False)
    d = gw.home / ".contextstack" / "gateway" / "providers.d"
    d.mkdir(parents=True)
    write(d, {"box.json": ov([box(url)]), "notes.txt": "x", ".hidden.json": "{broken"})
    gw.start()
    c = catalog(gw)
    eq((c["overlay_dir"], c["overlay_files"], "spec-box" in endpoints(gw)), (str(d), ["box.json"], True), "default dir")
    missing = t.gateway("ov-missing", env={"CSGATEWAY_PROVIDERS_DIR": str(t.run.tmp / "no-such-dir")})
    eq((catalog(missing)["overlay_files"], sorted(endpoints(missing))), ([], EMBEDDED), "missing dir")
    off = t.gateway("ov-off", expect_start=False, env={"CSGATEWAY_PROVIDERS_DIR": "off"})
    d2 = off.home / ".contextstack" / "gateway" / "providers.d"
    d2.mkdir(parents=True)
    write(d2, {"box.json": ov([box(url)])})
    off.start()
    eq((catalog(off)["overlay_files"], "spec-box" in endpoints(off)), ([], False), "off ignores the default dir")


@case("catalog", gw=None)
def revision_deterministic(t):
    """The revision is a function of the merged content: identical input gives the identical revision across processes and restarts; formatting does not matter; a content change gives a new one."""
    doc = ov([box("http://127.0.0.1:9")])
    a = overlay_dir(t, "rev-a", {"x.json": doc})
    b = overlay_dir(t, "rev-b", {"x.json": json.dumps(doc, indent=4, sort_keys=True)})
    c = overlay_dir(t, "rev-c", {"x.json": ov([box("http://127.0.0.1:9", host="box-other")])})
    env = {"CSGATEWAY_DISCOVERY_DISABLED": "spec-box"}
    ga = t.gateway("rev-a", env=dict(env, CSGATEWAY_PROVIDERS_DIR=str(a)))
    ra = revision(ga)
    ga.stop()
    ga.start()
    gb = t.gateway("rev-b", env=dict(env, CSGATEWAY_PROVIDERS_DIR=str(b)))
    gc = t.gateway("rev-c", env=dict(env, CSGATEWAY_PROVIDERS_DIR=str(c)))
    eq((revision(ga), revision(gb)), (ra, ra), "same content: restart, other process, other formatting")
    eq(revision(gc) != ra and bool(REV.match(revision(gc))), True, "changed content: a new revision")
    eq(revision(t.gateway("rev-none")) != ra, True, "no overlay differs")


@case("catalog", gw=None)
def reload_sighup_publishes(t):
    """SIGHUP publishes a new snapshot when the layers changed: new revision and seq, the new endpoint serves without a restart, last_reload ok."""
    m, url = t.mock("hup-box")
    d = overlay_dir(t, "hup", {})
    gw = t.gateway("reload-hup", t.run.config(extra_models=[BOX_MODEL]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    r1 = revision(gw)
    refused = gw.call(model_call("box-model", request_id="before")).json
    eq(refused["status"], "error", "before: no spec-box endpoint")
    write(d, {"box.json": ov([box(url)])})
    gw.send(signal.SIGHUP)
    gw.wait(lambda: revision(gw) != r1, timeout=5, what="new revision after SIGHUP")
    c = catalog(gw)
    sub(c, {"seq": 2, "overlay_files": ["box.json"], "last_reload": {"ok": True, "revision": c["revision"]}}, "catalog after reload")
    sub(gw.call(model_call("box-model", request_id="after")).json, {"status": "ok", "provider": "spec-box"}, "after")
    rows = {r["request_id"]: r["catalog_revision"] for r in gw.log_rows("catalog_binding")}
    eq((rows.get("before"), rows.get("after")), (r1, c["revision"]), "catalog_binding rows")


@case("catalog", gw=None, fast=False)
def reload_by_poll(t):
    """Without a signal, a changed overlay is picked up by the poll (5 s) and published."""
    d = overlay_dir(t, "poll", {})
    gw = t.gateway("reload-poll", env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    r1 = revision(gw)
    write(d, {"box.json": ov([box("http://127.0.0.1:9")])})
    gw.wait(lambda: revision(gw) != r1, timeout=12, what="poll reload")
    eq("spec-box" in endpoints(gw), True, "published")


@case("catalog", gw=None)
def reload_bad_file_keeps_snapshot(t):
    """A reload that fails (a bad file beside good ones) keeps the previous snapshot whole: same revision and seq, the old endpoints serve, the error names the file at GET /v1/catalog; a later valid change publishes."""
    m, url = t.mock("keep-box")
    d = overlay_dir(t, "keep", {"box.json": ov([box(url)])})
    gw = t.gateway("reload-keep", t.run.config(extra_models=[BOX_MODEL]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    r1 = revision(gw)
    write(d, {"box.json": ov([box(url, host="box-new")]), "zz.json": "{broken"})
    gw.send(signal.SIGHUP)
    c = gw.wait(lambda: (lambda c: c if not c["last_reload"]["ok"] else None)(catalog(gw)), timeout=5, what="failed reload reported")
    eq((c["revision"], c["seq"], endpoints(gw)["spec-box"]["host"]), (r1, 1, "box-1"), "previous snapshot kept whole")
    eq("zz.json" in c["last_reload"]["error"], True, "error names the file: " + c["last_reload"]["error"])
    eq(gw.call(model_call("box-model")).json["status"], "ok", "old snapshot serves")
    write(d, {"zz.json": None})
    gw.send(signal.SIGHUP)
    gw.wait(lambda: revision(gw) != r1, timeout=5, what="valid change published")
    sub(catalog(gw), {"seq": 2, "last_reload": {"ok": True}}, "after fix")
    eq(endpoints(gw)["spec-box"]["host"], "box-new", "new content")


@case("catalog", gw=None)
def reload_error_clears_when_restored(t):
    """After a failed reload, restoring the layers to the published content clears the reported error (last_reload ok); an operator never sees a failure that no longer exists."""
    d = overlay_dir(t, "restore", {"box.json": ov([box("http://127.0.0.1:9")])})
    gw = t.gateway("reload-restore", env={"CSGATEWAY_PROVIDERS_DIR": str(d), "CSGATEWAY_DISCOVERY_DISABLED": "spec-box"})
    r1 = revision(gw)
    write(d, {"zz.json": "{broken"})
    gw.send(signal.SIGHUP)
    gw.wait(lambda: not catalog(gw)["last_reload"]["ok"], timeout=5, what="failed reload reported")
    write(d, {"zz.json": None})
    gw.send(signal.SIGHUP)
    time.sleep(0.5)
    c = catalog(gw)
    eq((c["revision"], c["last_reload"].get("ok"), c["last_reload"].get("error")), (r1, True, None), "last_reload after restore")


@case("catalog", gw=None)
def revision_bound_per_call(t):
    """A call binds one snapshot for its life: a call in flight when its endpoint is removed by a reload finishes ok on its revision; a new call binds the new revision and refuses; each call logs its revision."""
    m, url = t.mock("bind-box")
    d = overlay_dir(t, "bind", {"box.json": ov([box(url)])})
    gw = t.gateway("bind", t.run.config(extra_models=[BOX_MODEL]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    r1 = revision(gw)
    m.chat["box-model"] = {"hold": "bind", "then": {"json": upstreams.chat_completion(text="finished on r1")}}
    th, out = held_call(gw, model_call("box-model", request_id="in-flight"))
    m.events.wait(lambda ev: any(e.get("held") == "bind" for e in ev))
    write(d, {"box.json": None})
    gw.send(signal.SIGHUP)
    gw.wait(lambda: revision(gw) != r1, timeout=5, what="swap")
    r2 = revision(gw)
    b = gw.call(model_call("box-model", request_id="after-swap")).json
    sub(b, {"status": "error", "cost": {"amount": "0.000000"}}, "new call on the new revision")
    m.release("bind")
    th.join(10)
    sub(out["r"].json, {"status": "ok", "provider": "spec-box", "message": {"content": "finished on r1"}}, "in-flight call")
    eq(len(t.run.calls(m)), 1, "upstream requests (the new call never reached the removed endpoint)")
    rows = {r["request_id"]: r["catalog_revision"] for r in gw.log_rows("catalog_binding")}
    eq((rows.get("in-flight"), rows.get("after-swap")), (r1, r2), "catalog_binding rows")


@case("catalog", gw=None)
def reload_keeps_admission_permits(t):
    """Admission is process-long: a permit held by an in-flight call survives a catalog swap, so a second call to the same deployment is still refused for capacity, and the permit is released when the first call ends."""
    d = overlay_dir(t, "permits", {})
    gw = t.gateway("permits", t.run.config(deployments=[t.run.vllm_deployment()]), env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    r1 = revision(gw)
    t.vllm.chat["local-coder"] = {"hold": "permit", "then": {"json": upstreams.chat_completion()}}
    th, out = held_call(gw, model_call("local-coder", request_id="holder"))
    t.vllm.events.wait(lambda ev: any(e.get("held") == "permit" for e in ev))
    write(d, {"box.json": ov([box("http://127.0.0.1:9")])})
    gw.send(signal.SIGHUP)
    gw.wait(lambda: revision(gw) != r1, timeout=5, what="swap")
    sub(gw.admission()["deployments"]["vllm-spec"], {"active": 1}, "permit held across the swap")
    sub(gw.call(model_call("vllm-big", request_id="second")).json, {"status": "error", "error": {"code": "gateway.capacity_exhausted"}}, "second call")
    t.vllm.release("permit")
    th.join(10)
    eq(out["r"].json["status"], "ok", "holder")
    gw.wait(lambda: gw.admission()["deployments"]["vllm-spec"]["active"] == 0, timeout=2, what="permit released")


def cache(gw):
    return {c["source"]: c for c in catalog(gw)["model_cache"]}


@case("catalog", gw=None)
def model_cache_fresh_stale_empty(t):
    """The model cache keeps each endpoint's last good observation (source, models, time, revision): a failed discovery keeps it as stale with the error, while availability follows the latest attempt; a valid empty list replaces it."""
    gw = t.gateway("cache")
    v = cache(gw)["vllm"]
    sub(v, {"source": "vllm", "state": "fresh", "models": sorted(t.vllm.models), "catalog_revision": revision(gw)}, "fresh")
    t.vllm.discovery = {"json": {}, "status": 500}
    eq(gw.post("/v1/inventory/refresh").status, 200, "refresh")
    s = cache(gw)["vllm"]
    sub(s, {"state": "stale", "models": v["models"], "observed_at": v["observed_at"]}, "stale keeps the observation")
    eq("500" in s.get("last_error", ""), True, "last_error " + s.get("last_error", ""))
    rows = {m["name"]: m["available"] for m in gw.get("/v1/models?available_only=false").json["models"]}
    eq(rows["local-coder"], False, "availability follows the latest attempt, not the cache")
    t.vllm.discovery = {"json": {"object": "list", "data": []}}
    gw.post("/v1/inventory/refresh")
    sub(cache(gw)["vllm"], {"state": "fresh", "models": []}, "a valid empty list replaces")


@case("catalog", gw=None, fast=False)
def model_cache_expires(t):
    """A cached observation older than stale_after is expired, whatever the last attempt."""
    gw = t.gateway("cache-exp", env={"CSAGENT_INVENTORY_REFRESH_INTERVAL": "1s", "CSGATEWAY_DISCOVERY_STALE_AFTER": "2s"})
    t.vllm.discovery = {"json": {}, "status": 500}
    gw.wait(lambda: cache(gw)["vllm"]["state"] == "stale", timeout=4, what="stale")
    gw.wait(lambda: cache(gw)["vllm"]["state"] == "expired", timeout=5, what="expired")
    eq(len(cache(gw)["vllm"]["models"]), len(t.vllm.models), "models kept while expired")


def dgx_overlay(spark1_url, spark2_url):
    """The repo's DGX overlay example with its tunnel endpoints pointed at mocks (the real ones are the DGX tunnels; never contacted)."""
    doc = json.loads((SPEC.parent.parent / "gateway" / "providers.d.example" / "dgx.json").read_text())
    want = {"vllm": "http://127.0.0.1:8000/v1", "vllm-spark2": "http://127.0.0.1:8001/v1"}
    eq({e["name"]: e["endpoint"] for e in doc["providers"]}, want, "example overlay endpoints")
    eq(doc.get("schema"), "csgateway.providers.v2", "example overlay schema")
    for e in doc["providers"]:
        e["endpoint"] = {"vllm": spark1_url, "vllm-spark2": spark2_url}[e["name"]] + "/v1"
    for dep in doc.get("deployments") or []:
        if dep.get("provider") == "vllm-spark2":
            dep["endpoint"] = spark2_url + "/v1"
    return doc


@case("catalog", gw=None)
def dgx_overlay_ids_resolve(t):
    """The DGX overlay keeps the ids: vllm (example-host-1) is patched and vllm-spark2 (example-host-2) is added by file, with models and deployments in the overlay; local-reasoner lists on /v1/models and routes; without the overlay the vllm-spark2 deployment refuses startup."""
    spark2, url2 = t.mock("spark2")
    spark2.models = ["local-reasoner"]
    d = overlay_dir(t, "dgx", {"dgx.json": dgx_overlay(t.run.vllm_url, url2)})
    cfg = t.run.config(deployments=[t.run.vllm_deployment()])
    gw = t.gateway("dgx", cfg, env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    names = [m["name"] for m in gw.get("/v1/models").json["models"]]
    eq("local-reasoner" in names, True, "overlay model on /v1/models")
    e = endpoints(gw)
    sub(e["vllm-spark2"], {"protocol": "openai_chat_completions", "endpoint": url2 + "/v1", "host": "example-host-2", "source": "overlay:dgx.json",
                           "capabilities": {"images": False, "tools": True, "streaming": False}}, "vllm-spark2")
    sub(e["vllm"], {"host": "example-host-1", "capabilities": {"images": False, "tools": True, "streaming": False}}, "vllm")
    sub(gw.call(model_call("local-reasoner")).json, {"status": "ok", "provider": "vllm-spark2", "model": "local-reasoner"}, "local-reasoner")
    eq([c["json"]["model"] for c in t.run.calls(spark2)], ["local-reasoner"], "spark2 upstream")
    sub(gw.call(model_call("local-coder")).json, {"status": "ok", "provider": "vllm"}, "local-coder")
    reasoner = {"name": "local-reasoner", "provider": "vllm-spark2", "tier": 2, "capability": "reasoning"}
    dep = {"id": "spark2", "provider": "vllm-spark2", "endpoint": url2 + "/v1", "models": ["local-reasoner"], "max_concurrent": 1, "max_queued": 0, "max_wait_ms": 0}
    cfg_bad = t.run.config(extra_models=[reasoner], deployments=[dep, t.run.vllm_deployment()])
    code, log = t.gateway("dgx-absent", cfg_bad, env={"CSGATEWAY_PROVIDERS_DIR": str(overlay_dir(t, "dgx-absent", {}))}, expect_start=False).start_fails()
    eq((code != 0, "vllm-spark2" in log), (True, True), "deployment on a missing endpoint refuses startup")
    no_egress(t)


@case("catalog", gw=None)
def overlay_adds_model(t):
    """An overlay file may add a model and deployment: it appears on /v1/models and model.call routes through the overlay provider."""
    m, url = t.mock("overlay-model")
    m.models = ["overlay-only"]
    doc = {"schema": "csgateway.providers.v2",
           "providers": [box(url, name="overlay-vllm")],
           "models": [{"name": "overlay-only", "provider": "overlay-vllm", "tier": 2, "capability": "general"}],
           "deployments": [{"id": "overlay-only-dep", "provider": "overlay-vllm", "endpoint": url + "/v1",
                            "models": ["overlay-only"], "max_concurrent": 1, "max_queued": 0, "max_wait_ms": 0}]}
    d = overlay_dir(t, "ov-model", {"overlay.json": doc})
    cfg = t.run.config()
    gw = t.gateway("ov-model", cfg, env={"CSGATEWAY_PROVIDERS_DIR": str(d)})
    names = [x["name"] for x in gw.get("/v1/models").json["models"]]
    eq("overlay-only" in names, True, "listed")
    sub(gw.call(model_call("overlay-only")).json,
        {"status": "ok", "provider": "overlay-vllm", "model": "overlay-only"}, "routed")
    no_egress(t)
