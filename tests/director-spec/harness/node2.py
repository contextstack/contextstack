"""The protocol-2 stand-in node (NODE-PROTOCOL.md §9): a registered node that sends ContextStack-Protocol: 2 and
makes protocol 2's calls the way a node client would. The Director-side protocol-2 cases drive it step by step (the
real node client, cs/ops/bin, is held to the node side by cases/node2.py)."""
import time

from harness import wire
from harness.run import Node

PROTOCOL = 2


class Node2(Node):
    """A node speaking protocol 2. Every call carries version 2; `call` behaves as Node.call otherwise."""

    def headers(self, **over):
        h = super().headers(**over)
        if wire.H_PROTOCOL not in {k for k in over}:
            h[wire.H_PROTOCOL] = str(PROTOCOL)
        return h

    def work(self, after=None, wait=None, timeout=45):
        """GET work (§9.2), held open. Returns (answer, seconds it took)."""
        query = "&".join("%s=%s" % kv for kv in (("after", after), ("wait", wait)) if kv[1] is not None)
        started = time.time()
        a = wire.call(self.target.url, "GET", "work" + ("?" + query if query else ""), token=self.token,
                      headers=self.headers(), timeout=timeout)
        return a, time.time() - started

    def control(self):
        return self.call("GET", "control")

    def report(self, control_id, state, outcome=None, detail=None, result=None):
        body = {"state": state}
        for k, v in (("outcome", outcome), ("detail", detail), ("result", result)):
            if v is not None:
                body[k] = v
        return self.call("POST", "control/" + control_id, body=body)

    def events(self, run_id, job_id, events):
        return self.call("POST", "runs/%s/events" % run_id, body={"job_id": job_id, "events": events})

    def status(self, active, node_version="director-spec stand-in", vitals=None, held=None):
        body = {"active": active, "node_version": node_version}
        if vitals is not None:
            body["vitals"] = vitals
        if held is not None:
            body["held"] = held
        return self.call("POST", "status", body=body)

    def schedules(self, schedules, firings=()):
        return self.call("POST", "schedules", body={"schedules": list(schedules), "firings": list(firings)})

    def accept(self, job_id):
        return self.call("POST", "jobs/%s/accept" % job_id, body={"bundle_sha256": self.job(job_id)["bundle_sha256"]})

    def land(self, run_id, body, key):
        h = self.headers()
        h["Idempotency-Key"] = key
        return self.call("POST", "runs/%s/land" % run_id, body=body, headers=h)


def land_body(state, /, repo="contextstack", lane="lane/fixture", main="main", **over):
    body = {"state": state, "repo": repo, "lane": lane, "main": main, "landed_by": "owner"}
    if state == "landed":
        body["commit"] = over.pop("commit", "deadbeef")
        body["reason"] = None
    else:
        body["commit"] = None
        body["reason"] = over.pop("reason", "check failed")
    body.update(over)
    return body


def delta(seq, text, partial=True):
    """A runner-shaped live event: delta text (the gateway's stream chunk), as the node would send it."""
    return {"seq": seq, "kind": "delta", "partial": partial, "payload": {"text": text}, "at": "2026-11-10T00:00:00Z"}


def stop(seq):
    return {"seq": seq, "kind": "stop", "partial": True, "payload": {"reason": "end_turn"}}


def schedule(schedule_id, revision=1, enabled=True, **more):
    s = {"schedule_id": schedule_id, "revision": revision, "enabled": enabled, "cron": "0 2 * * *",
         "timezone": "UTC", "spec": "spec.yaml", "task": "tasks/t"}
    s.update(more)
    return s


def firing(schedule_id, slot, state="recorded", **more):
    f = {"firing_id": "%s@%s" % (schedule_id, slot), "schedule_id": schedule_id, "slot": slot, "state": state,
         "recorded_at": "2026-11-10T02:00:01Z"}
    f.update(more)
    return f
