"""Lab work orders (POST /v1/ai/lab-execute) with the fake CLI child:
argv, stdin, environment filtering, exit codes, admission, refusals,
cancellation and process-tree cleanup, surviving descendants, parent death.
The gateway owns the permit and the process; clients never choose a command."""
import json
import os
import signal
import subprocess
import time

from harness.run import case, eq, sub
from cases.admission import POOL, idle

ALLOWED = {"LAB_STATE_ROOT", "LAB_CONFIG_ROOT", "LAB_WORKSPACES_ROOT", "LAB_CODE_ROOT", "PATH", "HOME", "USER", "LOGNAME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TMP", "TEMP", "SHELL", "HOMEBREW_PREFIX", "HOMEBREW_CELLAR"}
LAB_KEYS = ["LAB_JOB_ID", "LAB_SCHEDULE_DATE", "LAB_SCHEDULE_SLOT", "LAB_SCHEDULE_TIMEZONE", "LAB_TRIAL", "LAB_VIDEO_MUTATION"]
# Added by the child's own runtime, not the gateway: PEP 538 locale
# coercion (LC_CTYPE) and macOS CoreFoundation.
RUNTIME_ADDED = {"LC_CTYPE", "__CF_USER_TEXT_ENCODING"}


def order(fake=None, executor="fake-cli", timeout_s=30, **extra):
    o = {"executor": executor, "timeout_s": timeout_s}
    if fake is not None:
        o["fake"] = fake
    o.update(extra)
    return o


def execute(gw, work_order, environment=None, **kw):
    body = {"work_order": work_order}
    if environment is not None:
        body["environment"] = environment
    return gw.post("/v1/ai/lab-execute", body, **kw)


def alive(pid):
    """Still running. A killed descendant whose parent died too is a zombie (macOS: or exiting) until launchd or
    init reaps it, which lags under load; it runs nothing, so it counts as gone (kill(pid, 0) alone said alive)."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    stat = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return bool(stat) and stat[0] not in "ZEX"


def wait_file(path, timeout=10):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if os.path.exists(path):
            return json.loads(open(path).read() or "{}") if path.endswith(".json") else open(path).read()
        time.sleep(0.02)
    raise AssertionError("child never wrote " + path)


@case("workorders", gw="admit")
def workorder_runs_child(t):
    """The child is <python3> <lab_executor> --gateway-owned with the compact work order on stdin; the reply is {stdout, stderr, exit_code}."""
    wo = order({}, note="x y")
    r = execute(t.gw, wo, {"LAB_JOB_ID": "job-1", "LAB_TRIAL": "2", "SECRET_TOKEN": "nope"})
    eq(r.status, 200, "status")
    eq(r.headers.get("content-type"), "application/json", "content-type")
    b = r.json
    eq(sorted(b), ["exit_code", "stderr", "stdout"], "keys")
    eq((b["exit_code"], b["stderr"]), (0, ""), "exit and stderr")
    rep = json.loads(b["stdout"])
    eq(rep["argv"], ["--gateway-owned"], "argv")
    eq(rep["stdin"], json.dumps(wo, separators=(",", ":")), "stdin is the compact work order")
    eq(rep["lab"], {"LAB_JOB_ID": "job-1", "LAB_SCHEDULE_DATE": "", "LAB_SCHEDULE_SLOT": "", "LAB_SCHEDULE_TIMEZONE": "",
                    "LAB_TRIAL": "2", "LAB_VIDEO_MUTATION": ""}, "LAB_* values (unsent ones present and empty)")


@case("workorders", gw="admit")
def workorder_env_filtered(t):
    """The child environment is the allowlist plus the six LAB_* keys: no proxy, CA, gateway or credential variables, no requested extras."""
    rep = json.loads(execute(t.gw, order({}), {"SECRET_TOKEN": "nope", "PATH": "/evil"}).json["stdout"])
    names = set(rep["env_names"]) - RUNTIME_ADDED
    eq(sorted(names - ALLOWED - set(LAB_KEYS)), [], "names outside the allowlist")
    eq(sorted(names & ALLOWED), ["HOME", "LANG", "LOGNAME", "PATH", "TMPDIR", "USER"], "allowlisted names present (those the gateway had)")
    eq(rep["path"].startswith("/usr/bin:/bin:/usr/sbin:/sbin"), True, "PATH is the gateway's resolver search path, not the request's (%s)" % rep["path"])


@case("workorders", gw=None)
def workorder_env_node_roots(t):
    """The node's roots (LAB_*_ROOT from the gateway's own environment) reach the executor, so its runs land in the node's
    state root, not under the release's code root; a request cannot set them."""
    r = t.run
    roots = {"LAB_STATE_ROOT": "/spec/state", "LAB_CONFIG_ROOT": "/spec/config", "LAB_WORKSPACES_ROOT": "/spec/workspaces"}
    gw = t.gateway("roots", r.config(deployments=[{"id": "cli-spec", "provider": "cli", "models": ["fake-cli"], "max_concurrent": 1}],
                                     lab_executor=True, estate="spec"), env=roots)
    rep = json.loads(execute(gw, order({}), {"LAB_STATE_ROOT": "/evil"}).json["stdout"])
    eq(sorted(set(rep["env_names"]) & set(roots)), sorted(roots), "roots present")


@case("workorders", gw="admit")
def workorder_exit_code(t):
    """A failing child is still HTTP 200, carrying its exit code, stdout and stderr."""
    b = execute(t.gw, order({"stdout": "partial", "stderr": "bad thing", "exit": 3})).json
    eq(b, {"stdout": "partial", "stderr": "bad thing", "exit_code": 3}, "reply")
    idle(t.gw, "cli-spec")


@case("workorders", gw="admit")
def workorder_invalid(t):
    """Bad requests are 400 text, before any admission or child: undecodable, no executor, timeout outside 0..3600."""
    for what, body, text in (
            ("undecodable", None, "invalid work order"),
            ("no work_order", {}, "invalid executor or timeout"),
            ("no executor", {"work_order": {"timeout_s": 5}}, "invalid executor or timeout"),
            ("timeout too long", {"work_order": order(timeout_s=3601)}, "invalid executor or timeout"),
            ("negative timeout", {"work_order": order(timeout_s=-1)}, "invalid executor or timeout"),
            ("work_order not an object", {"work_order": [1]}, "invalid executor or timeout")):
        r = t.gw.post("/v1/ai/lab-execute", body, raw=b"{" if body is None else None)
        eq((r.status, r.text), (400, text + "\n"), what)
    eq(t.gw.admission()["deployments"]["cli-spec"], {"active": 0, "queued": 0}, "no permit taken")


@case("workorders", gw="admit")
def workorder_oversize(t):
    """A request body over 2 MiB is refused as invalid (400)."""
    r = execute(t.gw, order({}, blob="x" * (2 << 20)))
    eq((r.status, r.text), (400, "invalid work order\n"), "oversize")


@case("workorders", gw="admit")
def workorder_unbound_executor(t):
    """An executor no cli deployment binds: 503 deployment_unbound, no child."""
    r = execute(t.gw, order({}, executor="other-cli"))
    eq((r.status, r.text), (503, "deployment_unbound: cli/other-cli\n"), "unbound")


@case("workorders", gw="admit")
def workorder_queue_full(t):
    """cli-spec allows one work order and no queue: a second concurrent order is 503 queue_full; the first completes; the permit is released."""
    first = POOL.submit(execute, t.gw, order({"sleep_s": 1.0, "stdout": "slow"}))
    t.gw.wait(lambda: t.gw.admission()["deployments"]["cli-spec"]["active"] == 1, what="work order active")
    r = execute(t.gw, order({"stdout": "fast"}))
    eq((r.status, r.text), (503, "queue_full: cli-spec\n"), "second order")
    eq(first.result(10).json["stdout"], "slow", "first order")
    idle(t.gw, "cli-spec")
    rows = [r for r in t.gw.log_rows("admission_timing") if r["scope"] == "lab_work_order"]
    eq(sorted(rows[-1]), ["config_sha", "deployment_id", "event", "model", "queue_wait_ms", "request_id", "scope", "work_order_latency_ms"], "timing row keys")
    sub(rows[-1], {"deployment_id": "cli-spec", "model": "fake-cli"}, "timing row")


@case("workorders", gw="admit")
def workorder_cancel_kills_tree(t):
    """A caller disconnecting mid-order: the child and its descendants are terminated (process group) and the permit released."""
    pidfile = str(t.gw.dir / "cancel-pids.json")
    POOL.submit(t.gw.abandon, "/v1/ai/lab-execute", {"work_order": order({"sleep_s": 30, "grandchild": True, "pidfile": pidfile})}, 1.0)
    pids = wait_file(pidfile)
    eq(alive(pids["pid"]) and alive(pids["grandchild"]), True, "child and grandchild running")
    eq(pids["pgid"], pids["pid"], "child leads its own process group")
    t.gw.wait(lambda: not alive(pids["pid"]) and not alive(pids["grandchild"]), timeout=8, what="child and grandchild gone")
    idle(t.gw, "cli-spec")


@case("workorders", gw="admit", fast=False)
def workorder_surviving_descendant(t):
    """The child exits 0 but leaves a descendant holding stdout: the gateway stops waiting after its pipe grace, kills the group, and reports 502."""
    pidfile = str(t.gw.dir / "survivor-pids.json")
    started = time.monotonic()
    r = execute(t.gw, order({"grandchild": True, "exit_early": True, "pidfile": pidfile, "stdout": "early\n"}))
    took = time.monotonic() - started
    pids = wait_file(pidfile)
    eq(r.status, 502, "status")
    eq("exec: WaitDelay expired" in r.text, True, "body names the wait delay (%r)" % r.text)
    eq(4 <= took < 9, True, "about the 5 s pipe grace (took %.1fs)" % took)
    t.gw.wait(lambda: not alive(pids["grandchild"]), timeout=5, what="descendant killed")
    idle(t.gw, "cli-spec")


@case("workorders", gw=None)
def workorder_parent_death(t):
    """Stdin stays open while the gateway lives: when the gateway dies (SIGKILL) the child sees EOF."""
    gw = t.gateway("parent-death", t.run.config(deployments=[{"id": "cli-spec", "provider": "cli", "models": ["fake-cli"], "max_concurrent": 1,
                                                                "max_queued": 0, "max_wait_ms": 0}], lab_executor=True))
    marker = gw.dir / "eof.txt"
    pidfile = str(gw.dir / "parent-death-pids.json")
    request = POOL.submit(lambda: _swallow(execute, gw, order({"eof_marker": str(marker), "pidfile": pidfile})))
    # Admission precedes spawn. The atomically published PID file proves the
    # child has read its work order; a fixed sleep cannot establish readiness.
    pids = wait_file(pidfile)
    os.kill(pids["pid"], 0)
    eq(os.path.exists(marker), False, "child still waiting while the gateway lives")
    gw.send(signal.SIGKILL)
    # open() makes the marker visible before write()/close() publish its contents.
    # The fixture exits after writing; wait for the completed message, not just
    # the directory entry.
    gw.wait(lambda: marker.exists() and marker.read_text() == "stdin closed\n",
            timeout=5, what="child saw EOF")
    request.result(5)


def _swallow(fn, *a):
    try:
        return fn(*a)
    except OSError:
        return None


@case("workorders")
def workorder_not_configured(t):
    """No lab_executor in models.json: 503 not_configured."""
    r = execute(t.gw, order({}))
    eq((r.status, r.json), (503, {"error": "Lab executor not configured", "code": "not_configured"}), "reply")
