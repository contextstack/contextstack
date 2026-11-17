"""The stand-in Director's protocol 2 (NODE-PROTOCOL.md §9), mixed into FakeDirector: control messages, the held-open
GET work, run events, status and schedules, in memory. Its administrative acts stand in for a client's:
  cancel(node, job_id) -> control_id                  a cancel control message (§9.1)
  schedule_change(node, schedule_id, expect, set)     a schedule_change control message (§9.5)
  requeue(control_id)                                 put a settled message back to requested (a Director that lost it)
What nodes sent is kept: control (by id), events ({run_id: {job, node, events: {seq: event}}}), status ({node:
[reports]}), schedules ({node: [reports]}), seen_version ({node: last protocol})."""
import hashlib
import json
import re
import secrets
import time
import urllib.parse

KIND = re.compile(r"[a-z][a-z0-9_.]{0,63}")
PLAIN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
FIRING = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:@-]{0,199}")
OUTCOMES = {("cancel", "applied"): {"stopped", "already_finished"}, ("cancel", "refused"): {"refused"},
            ("schedule_change", "applied"): {"applied"},
            ("schedule_change", "refused"): {"revision_mismatch", "unknown_schedule", "refused"}}
FIRING_STATES = {"recorded", "started", "done", "error", "missed", "uncertain"}
PUBLIC = ("control_id", "kind", "state", "issued_at", "body", "outcome", "detail", "result", "received_at", "settled_at")


def now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def bad(detail):
    return 422, {"error": "Unprocessable Content", "code": "invalid_request", "detail": detail}


def missing():
    return 404, {"error": "Not Found", "code": "not_found", "detail": "stand-in"}


class Protocol2:
    def init2(self):
        self.control, self.events, self.status, self.schedules, self.seen_version = {}, {}, {}, {}, {}
        self.serial = 0

    # ---- administration ----
    def issue(self, node, kind, body):
        self.serial += 1
        cid = "ctl_" + "".join(secrets.choice("abcdefghijklmnopqrstuvwxyz234567") for _ in range(26))
        self.control[cid] = {"control_id": cid, "kind": kind, "state": "requested", "issued_at": now(), "body": body,
                             "node": node, "serial": self.serial}
        return cid

    def cancel(self, node, job_id):
        with self.lock:
            return self.issue(node, "cancel", {"job_id": job_id})

    def schedule_change(self, node, schedule_id, expect, change):
        with self.lock:
            return self.issue(node, "schedule_change", {"schedule_id": schedule_id, "expect_revision": expect, "set": change})

    def requeue(self, cid):
        with self.lock:
            c = self.control[cid]
            c["_was_settled"] = c.get("state") in ("applied", "refused")
            for k in ("outcome", "detail", "result", "received_at", "settled_at"):
                c.pop(k, None)
            c["state"] = "requested"

    def settle_by_terminal(self, job_id):
        for c in self.control.values():
            if c["kind"] == "cancel" and c["body"].get("job_id") == job_id and c["state"] == "requested" \
                    and not c.get("_was_settled"):
                c.update(state="applied", outcome="already_finished", detail="the job ended first", settled_at=now())

    # ---- what a node sees ----
    def listed_control(self, node):
        return [{k: c[k] for k in PUBLIC if k in c} for c in sorted(self.control.values(), key=lambda c: c["serial"])
                if c["node"] == node and c["state"] in ("requested", "received")]

    def work_body(self, node, changed):
        jobs = self.route(node, "GET", "jobs", b"")[1]["jobs"]
        control = self.listed_control(node)
        cursor = hashlib.sha256(json.dumps([jobs, control], sort_keys=True).encode()).hexdigest()[:24]
        return {"jobs": jobs, "control": control, "cursor": cursor, "changed": changed}

    def work_again(self, held, deadline):
        node, after = held
        body = self.work_body(node, True)
        if body["cursor"] != after:
            return 200, body, 2
        if time.time() >= deadline:
            return 200, dict(body, changed=False), 2
        return "hold", (deadline, held), 2

    def route2(self, node, method, path, raw):
        """A protocol-2 answer (status, body), ("hold", (deadline, held)), or None for the protocol-1 routes."""
        self.seen_version[node] = 2
        bare, _, query = path.partition("?")
        if method == "GET" and bare == "work":
            q = dict(urllib.parse.parse_qsl(query))
            wait = q.get("wait", "30")
            if not re.fullmatch(r"[0-9]+", wait):
                return bad("wait is an integer from 0 to 30")
            body = self.work_body(node, True)
            if q.get("after") != body["cursor"]:
                return 200, body
            return "hold", (time.time() + min(int(wait), 30), (node, q["after"]))
        if method == "GET" and bare == "control":
            return 200, {"control": self.listed_control(node)}
        try:
            data = json.loads(raw or b"null")
        except ValueError:
            data = None
        if method == "POST" and bare.startswith("control/"):
            return self.report(node, bare.split("/", 1)[1], data)
        if method == "POST" and bare.startswith("runs/") and bare.endswith("/events") and bare.count("/") == 2:
            return self.take_events(node, bare.split("/")[1], data)
        if method == "POST" and bare == "status":
            active = data.get("active") if isinstance(data, dict) else None
            if not isinstance(active, list) or len(active) > 1000 or not all(
                    isinstance(a, dict) and isinstance(a.get("job_id"), str) and a.get("state") in ("queued", "running")
                    for a in active):
                return bad("active: a list of {job_id, state queued|running}")
            self.status.setdefault(node, []).append(dict(data, recorded_at=now()))
            return 200, {"recorded_at": now()}
        if method == "POST" and bare == "schedules":
            return self.take_schedules(node, data)
        return None

    def report(self, node, cid, data):
        c = self.control.get(cid)
        if c is None or c["node"] != node:
            return missing()
        if not isinstance(data, dict) or data.get("state") not in ("received", "applied", "refused") \
                or not isinstance(data.get("detail", ""), str) or len(data.get("detail") or "") > 4000 \
                or not isinstance(data.get("result", {}), dict):
            return bad("state received | applied | refused; detail text; result an object")
        public = lambda: {"control": {k: c[k] for k in PUBLIC if k in c}}  # noqa: E731
        if data["state"] == "received":
            if c["state"] == "requested":
                c.update(state="received", received_at=now())
            return 200, public()
        allowed = OUTCOMES.get((c["kind"], data["state"]), {"refused"} if data["state"] == "refused" else ())
        if data.get("outcome") not in allowed:
            return bad("outcome not allowed for %s %s" % (c["kind"], data["state"]))
        if c["state"] in ("applied", "refused"):
            if (c["state"], c.get("outcome")) == (data["state"], data["outcome"]):
                return 200, public()
            return 409, {"error": "Conflict", "code": "conflict", "detail": "settled", **public()}
        c.update(state=data["state"], outcome=data["outcome"], detail=data.get("detail"), result=data.get("result"),
                 received_at=c.get("received_at") or now(), settled_at=now())
        return 200, public()

    def take_events(self, node, run_id, data):
        if not isinstance(data, dict) or not isinstance(data.get("events"), list) or len(data["events"]) > 500:
            return bad("{job_id, events: at most 500}")
        job = self.jobs.get(data.get("job_id"))
        if job is None or job["node"] != node:
            return missing()
        seqs = []
        for e in data["events"]:
            if not (isinstance(e, dict) and isinstance(e.get("seq"), int) and e["seq"] >= 1 and isinstance(e.get("kind"), str)
                    and KIND.fullmatch(e["kind"]) and isinstance(e.get("partial"), bool) and isinstance(e.get("payload"), dict)
                    and len(json.dumps(e["payload"])) <= 64 * 1024):
                return bad("event: {seq >= 1, kind, partial, payload}")
            seqs.append(e["seq"])
        if seqs != sorted(seqs):
            return bad("events sorted by seq")
        run = self.events.setdefault(run_id, {"job": job["job_id"], "node": node, "events": {}})
        if run["job"] != job["job_id"] or run["node"] != node:
            return 409, {"error": "Conflict", "code": "conflict", "detail": "run belongs to another job"}
        held, accepted, duplicates, gap = run["events"], 0, 0, None
        confirmed = len(held)
        for e in data["events"]:
            if e["seq"] <= confirmed:
                duplicates += 1
            elif e["seq"] == confirmed + 1:
                held[e["seq"]], confirmed, accepted = e, confirmed + 1, accepted + 1
            else:
                gap = {"expected": confirmed + 1}
                break
        return 200, {"run_id": run_id, "confirmed": confirmed, "accepted": accepted, "duplicates": duplicates, "gap": gap}

    def take_schedules(self, node, data):
        if not isinstance(data, dict) or not isinstance(data.get("schedules", []), list) \
                or not isinstance(data.get("firings", []), list):
            return bad("{schedules, firings}")
        for s in data.get("schedules", []):
            if not (isinstance(s, dict) and PLAIN.fullmatch(str(s.get("schedule_id") or "")) and isinstance(s.get("revision"), int)
                    and s["revision"] >= 1 and isinstance(s.get("enabled"), bool) and s.get("missed", "skip") in ("skip", "catch_up")
                    and isinstance(s.get("definition", {}), dict)):
                return bad("schedule: {schedule_id, revision >= 1, enabled, missed?, definition?}")
        for f in data.get("firings", []):
            if not (isinstance(f, dict) and FIRING.fullmatch(str(f.get("firing_id") or "")) and isinstance(f.get("slot"), str)
                    and f.get("state") in FIRING_STATES and isinstance(f.get("recorded_at"), str)):
                return bad("firing: {firing_id, schedule_id, slot, state, recorded_at}")
        self.schedules.setdefault(node, []).append(dict(data, recorded_at=now()))
        return 200, {"schedules": len(data.get("schedules", [])), "firings": len(data.get("firings", [])), "recorded_at": now()}
