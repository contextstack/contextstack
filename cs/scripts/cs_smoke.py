"""Real installed-release checks, sharing init's detection and first-job machinery."""
import json
import os
from datetime import datetime, timedelta, timezone
import re
import tempfile
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from types import SimpleNamespace
from urllib.parse import quote


SERVICE_WAIT_SECONDS = 60
PAUSE_REASONS = {
    "provider_unavailable": "provider unavailable",
    "provider_limit": "provider limit reached",
    "rate_limit": "rate limited",
}


class Skipped(RuntimeError):
    pass


def service_wait(init, seconds=SERVICE_WAIT_SECONDS):
    """None when the Director and the gateway endpoint the harness posts CLI work orders to both answer; otherwise
    a failure reason. Right after a release switch the services restart, and a trial job started before then
    fails with 'connection refused' although the release is fine."""
    from urllib.parse import urlparse
    parse_node_env = getattr(init.cs, "parse_node_env", lambda _: {})
    settings = parse_node_env(init.cs.CONFIG / "node.env")
    endpoint = urlparse(os.environ.get("CSGATEWAY_CLI_ENDPOINT") or settings.get("CSGATEWAY_CLI_ENDPOINT")
                        or "http://127.0.0.1:9200/v1/agent/cli.run")
    health = "%s://%s/v1/health" % (endpoint.scheme, endpoint.netloc)
    end, director, gateway = time.monotonic() + seconds, False, False
    while time.monotonic() < end and not (director and gateway):
        director = director or init.cs.curl(init.url + "/api/v1/client/jobs")[0] in (200, 401)
        gateway = gateway or init.cs.curl(health)[0] == 200
        if not (director and gateway):
            time.sleep(0.5)
    if director and gateway:
        return None
    missing = [name for name, up in (("the Director", director), ("the gateway", gateway)) if not up]
    return " and ".join(missing) + " never came up within %ds" % seconds


def sign_in_command(init, agent):
    module = init.cs.load_script("cs_init", init.cs.KIT / "scripts/cs_init.py")
    return module.sign_in_command(agent["name"])


def report_hold(init, agent, job, hold):
    reason, detail = hold
    init.cancel_job(job, "cs smoke trial held: " + (reason or "waiting"))
    if reason in (None, "executor_unavailable", "auth_expired", *PAUSE_REASONS):
        from cs_agents import _limits_rows

        limit = next((row for row in _limits_rows(state_root=init.cs.state_root())
                      if row["template"] == agent["template"]), None)
        if reason == "auth_expired" or (limit and limit.get("via") == "login"):
            raise Skipped("sign-in expired; run `%s` to sign in again" % sign_in_command(init, agent))
        if limit:
            raise Skipped("provider limit reached" + (": " + limit["message"] if limit.get("message") else ""))
        if reason == "executor_unavailable":
            raise Skipped(detail or "executor unavailable for " + agent["template"])
        if reason in PAUSE_REASONS:
            raise Skipped(detail or PAUSE_REASONS[reason])
    raise RuntimeError("held: " + (detail or reason or "waiting"))


def agent_check(init, agent):
    deadline = time.monotonic() + 120
    with tempfile.TemporaryDirectory(prefix="cs-smoke-") as tmp:
        init.bundle(Path(tmp), agent["template"], "smoke ok", "cs-smoke-" + uuid.uuid4().hex)
        result = init.run([str(init.cs.release_bin(init.cs.CURRENT, "cs-director")), "job", "create", "--node",
                           init.nr.load_record().get("node") or init.node_name(), "--bundle", tmp,
                           "--data-dir", str(init.data)], timeout=max(1, deadline - time.monotonic()))
    created = init.cs.read_body(result.stdout)
    job = created.get("job_id")
    if result.returncode or not job:
        raise RuntimeError("could not create job: " + (result.stderr or result.stdout)[-300:])
    while time.monotonic() < deadline:
        row = init.job_row(job)
        if row and row[0] in ("waiting", "held"):
            hold = init.job_hold(job)
            if hold is not None:
                report_hold(init, agent, job, hold)
        elif row and row[0] not in ("offered", "accepted", "running"):
            terminal = json.loads(row[2] or "{}")
            stop = terminal.get("stop")
            if stop == "auth_expired":
                raise Skipped("not signed in (%s)" % sign_in_command(init, agent))
            if stop in PAUSE_REASONS:
                raise Skipped(PAUSE_REASONS[stop])
            if row[0] != "done":
                raise RuntimeError("job finished " + row[0])
            verdict = terminal.get("verdict")
            if verdict is None:
                verdict = init.cs.read_json(init.cs.state_root() / "runs" / str(row[1]) / "check.json", {}).get("verdict")
            if verdict != "pass":
                raise RuntimeError("run verdict " + str(verdict or "missing"))
            return init.reply(row[1])[0][:300]
        time.sleep(min(1, max(0, deadline - time.monotonic())))
    init.cancel_job(job)
    raise RuntimeError("timed out after 120s")


def lead_check(cs, init):
    lead = cs.load_script("cs_lead", cs.KIT / "scripts/cs_lead.py")
    record = cs.read_json(cs.CONFIG / "hosted-lead.json", {})
    owner = lead.OwnerClient(record.get("director") or init.url, init.data / "local-credential")
    deadline = time.monotonic() + 180
    owner.token()  # Missing credentials fail immediately; a restarting Director gets time to listen.
    key = "smoke-" + uuid.uuid4().hex
    while True:
        try:
            status, body, _ = owner.request("POST", "conversations/%s/turns" % quote(record["conversation_id"], safe=""),
                body={"message": "Call the machines tool and reply with the node count as a number."}, key=key,
                timeout=min(5, max(0.01, deadline - time.monotonic())))
            if status not in (502, 503):
                break
        except lead.LeadError:
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError("Director unavailable within 180s")
        time.sleep(min(1, max(0, deadline - time.monotonic())))
    if status not in (200, 201):
        raise RuntimeError("could not send message (HTTP %s)" % status)
    turn = body["turn"]["turn_id"]
    while time.monotonic() < deadline:
        status, body, _ = owner.request("GET", "turns/" + quote(turn, safe=""), timeout=max(0.01, deadline - time.monotonic()))
        if status != 200:
            raise RuntimeError("could not read reply (HTTP %s)" % status)
        row = body if "state" in body else body.get("turn", {})
        if row.get("state") == "done":
            if not re.search(r"\d", row.get("reply") or ""):
                raise RuntimeError("reply contains no number")
            return
        if row.get("state") in ("error", "stopped", "failed", "cancelled"):
            raise RuntimeError("message finished " + row["state"])
        time.sleep(min(1, max(0, deadline - time.monotonic())))
    raise RuntimeError("timed out after 180s")


def measured(name, check):
    started = time.monotonic()
    try:
        reply = check()
        return {"name": name, "ok": True, "seconds": round(time.monotonic() - started, 1), "reply": reply}
    except Skipped as error:
        return {"name": name, "ok": True, "skipped": True, "seconds": round(time.monotonic() - started, 1), "reason": str(error)}
    except Exception as error:
        return {"name": name, "ok": False, "seconds": round(time.monotonic() - started, 1), "reason": str(error)}


def smoke_template(templates, agents, name):
    preferred = {"claude": "claude-code-sonnet"}.get(name)
    return preferred if preferred in templates else agents[name]


def agent_for(model, agents):
    if isinstance(model, str) and model.startswith("claude-code"):
        return "claude"
    return next((name for name, template in agents.items() if template and template == model), None)


def default_agents(record, agents):
    return {record.get("default_agent"), agent_for(record.get("default_model"), agents)}.intersection(agents)


def used_agents(cs, module, record):
    selected = default_agents(record, module.AGENTS)
    now = datetime.now(timezone.utc)
    cutoff = now - timedelta(days=14)
    try:
        with (cs.state_root() / "ledger.jsonl").open() as ledger:
            for line in ledger:
                try:
                    row = json.loads(line)
                    at = datetime.fromisoformat(row["ts"].replace("Z", "+00:00"))
                    if row.get("stop") == "done" and row.get("verdict") == "pass" and cutoff <= at <= now:
                        selected.add(agent_for(row.get("model"), module.AGENTS))
                except (ValueError, KeyError, TypeError):
                    continue
    except FileNotFoundError:
        pass
    return selected.intersection(module.AGENTS)


def main(cs, args):
    module = cs.load_script("cs_init", cs.KIT / "scripts/cs_init.py")
    selected = set(args.agents.split(",")) if args.agents else None
    unknown = (selected or set()) - set(module.AGENTS)
    if unknown:
        raise cs.Stop("unknown agents: " + ", ".join(sorted(unknown)), 2)
    init = module.Init(cs, SimpleNamespace(director_listen=None, verbose=False))
    if not (init.data / "local-credential").is_file():
        # Only a machine serving its own Director has one. A first install (before cs init) and a node of another
        # machine's Director have nothing local to trial; failing here would roll back, or refuse, a release that is fine.
        if args.json:
            print(json.dumps({"ok": True, "skipped": "not set up as its own Director", "checks": []}))
        else:
            print("This machine isn't set up as its own Director (cs init); nothing to test.")
        return 0
    if selected is None:
        selected = used_agents(cs, module, init.nr.load_record())
    templates = cs.read_json(cs.KIT / "ops/bin/executor-templates.json", {})
    skipped, names = [], []
    for n, template in module.AGENTS.items():
        if not template or n not in selected:
            continue
        template = smoke_template(templates, module.AGENTS, n)
        reason = templates.get(template, {}).get("unavailable_reason")
        if isinstance(reason, str):
            # Known not to work (owner-hidden harness): report it, don't trial it.
            skipped.append({"name": template, "ok": True, "skipped": True, "seconds": 0, "reason": reason})
        else:
            names.append(n)
    path = init.agent_path() if names else None
    with ThreadPoolExecutor(max_workers=max(1, len(names))) as pool:
        agents = list(pool.map(lambda n: init.probe(n, path), names))
    for agent in agents:
        agent["template"] = smoke_template(templates, module.AGENTS, agent["name"])
    checks = [(a["template"], lambda a=a: agent_check(init, a)) for a in agents if a["signed_in"] and a["path"]]
    no_agents = not checks
    hosted_lead = (cs.CONFIG / "hosted-lead.json").is_file()
    results = []
    if checks or hosted_lead:
        started = time.monotonic()
        wait_reason = service_wait(init)
        if wait_reason:
            results.append({"name": "services", "ok": False, "seconds": round(time.monotonic() - started, 1),
                            "reason": wait_reason})
            ok = False
            if args.json:
                print(json.dumps({"ok": ok, "no_agents": no_agents, "checks": results}))
            else:
                print("FAIL services: %s" % wait_reason)
            return 0 if ok else 1
    with ThreadPoolExecutor(max_workers=max(1, len(checks))) as pool:
        results = skipped + list(pool.map(lambda pair: measured(*pair), checks))
    if hosted_lead:
        results.append(measured("hosted-lead", lambda: lead_check(cs, init)))
    ok = all(r["ok"] for r in results)
    if args.json:
        print(json.dumps({"ok": ok, "no_agents": no_agents, "checks": results}))
    else:
        if no_agents:
            print("No signed-in runnable agents." + (" Nothing to test." if not results else ""))
        for row in results:
            if row.get("skipped"):
                print("skip %s: %s" % (row["name"], row["reason"]))
            elif row["ok"]:
                reply = " ".join((row.get("reply") or "").split())
                print("ok %s %.0fs%s" % (row["name"], row["seconds"], ": " + reply if reply else ""))
            else:
                print("FAIL %s: %s" % (row["name"], row["reason"]))
    return 0 if ok else 1
