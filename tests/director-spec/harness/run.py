"""Case registry and runner. A case is a function (ctx) registered with @case(side, area, id); it fails by raising
Failed (ctx.expect) or any exception. Each case gets fresh nodes, so cases never share jobs or envelopes."""
import time
import traceback

from harness import wire

CASES = []


class Failed(AssertionError):
    pass


class Case:
    def __init__(self, fn, side, area, cid, impls=None):
        self.fn, self.side, self.area, self.id, self.impls = fn, side, area, cid, impls
        self.doc = (fn.__doc__ or "").strip().splitlines()[0] if fn.__doc__ else ""


def case(side, area, cid, impls=None):
    """impls: the Director targets the case applies to (None: all). Protocol-2 cases are ("rust",): the stand-in
    speaks protocol 1 only."""
    def register(fn):
        CASES.append(Case(fn, side, area, cid, impls))
        return fn
    return register


class Node:
    """A registered node as the Director side sees it: its token, its installation and a call helper."""

    def __init__(self, target, name):
        self.target, self.name = target, name
        self.token, self.installation = target.register(name)

    def headers(self, **over):
        h = {wire.H_PROTOCOL: str(wire.PROTOCOL), wire.H_INSTALLATION: self.installation}
        h.update({k.replace("_", "-"): v for k, v in over.items()})
        return h

    def call(self, method, path, body=None, raw=None, headers=None, token="own"):
        return wire.call(self.target.url, method, path, token=self.token if token == "own" else token,
                         headers=headers if headers is not None else self.headers(), body=body, raw=raw)

    def offer(self):
        return self.target.offer(self.name)

    def job(self, job_id):
        jobs = self.call("GET", "jobs").json["jobs"]
        return next(j for j in jobs if j["job_id"] == job_id)


class Ctx:
    def __init__(self, cid, target=None, rig=None):
        self.id, self.target, self.rig, self.serial = cid, target, rig, 0

    def node(self, label="a"):
        self.serial += 1
        return Node(self.target, ("%s-%s-%d" % (self.id, label, self.serial))[:90])

    def node2(self, label="a"):
        """A registered node speaking protocol 2 (harness/node2.py)."""
        from harness.node2 import Node2
        self.serial += 1
        return Node2(self.target, ("%s-%s-%d" % (self.id, label, self.serial))[:90])

    def serial_tag(self):
        """A run-id-safe tag unique to this case in this suite run."""
        if not hasattr(self, "_tag"):
            import secrets
            self._tag = "%s-%s" % (self.id, secrets.token_hex(3))
        return self._tag

    def expect(self, cond, what, detail=""):
        if not cond:
            raise Failed("%s%s" % (what, (": %s" % (detail,))[:900] if detail != "" else ""))


def run_cases(selected, make_ctx, verbose=False):
    results = []
    for c in selected:
        started = time.time()
        ctx = make_ctx(c)
        try:
            c.fn(ctx)
            result, detail = "pass", ""
        except Failed as failure:
            result, detail = "fail", str(failure)
        except Exception:  # noqa: BLE001 — a crashing case is a failing case, with its traceback
            result, detail = "fail", traceback.format_exc(limit=4)
        finally:
            close = getattr(ctx.rig, "close", None)
            if close:
                close()
        results.append({"case": c.id, "side": c.side, "area": c.area, "result": result, "detail": detail,
                        "seconds": round(time.time() - started, 2)})
        mark = "ok  " if result == "pass" else "FAIL"
        print("%s %-8s %-10s %s%s" % (mark, c.side, c.area, c.id, ("\n       " + detail.replace("\n", "\n       ")) if detail else ""))
        if verbose and c.doc:
            print("       " + c.doc)
    return results
