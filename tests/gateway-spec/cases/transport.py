"""The unix-socket transport (WIRE Transport): same routes and
auth as TCP, mode 0600, stale-socket handling, refusals, loopback-only
routes refuse over it, clean shutdown removes it."""
import http.client
import json
import os
import socket
import stat
import tempfile
import time

from harness.gateway import model_call
from harness.run import case, eq, norm, sub


def sock_dir(t, mode=0o700):
    # Short path: AF_UNIX paths are limited to ~104 bytes on macOS.
    d = tempfile.mkdtemp(prefix="gws-", dir="/tmp")
    os.chmod(d, mode)
    t.owned_dirs = getattr(t, "owned_dirs", []) + [d]
    return d


def cfg(t):
    return t.run.config(deployments=[{"id": "cli-spec", "provider": "cli", "models": ["fake-cli"], "max_concurrent": 1, "max_queued": 0, "max_wait_ms": 0}],
                        lab_executor=True)


@case("transport", gw=None)
def socket_same_routes(t):
    """Over the socket: health, models and model.call answer exactly as over TCP; the file is a 0600 socket."""
    path = os.path.join(sock_dir(t), "gw.sock")
    gw = t.gateway("socket", socket_path=path)
    st = os.stat(path)
    eq((stat.S_ISSOCK(st.st_mode), stat.S_IMODE(st.st_mode)), (True, 0o600), "socket file and mode")
    eq(gw.get("/v1/health", via_socket=True).status, 200, "health")
    eq(gw.get("/v1/models", via_socket=True).json["models"], gw.get("/v1/models").json["models"], "models")
    a = norm(gw.call(model_call("local-coder"), via_socket=True).json)
    b = norm(gw.call(model_call("local-coder")).json)
    eq(a, b, "model.call over socket equals TCP")


@case("transport", gw=None)
def socket_auth(t):
    """Bearer auth applies on the socket exactly as on TCP."""
    path = os.path.join(sock_dir(t), "gw.sock")
    gw = t.gateway("socket-auth", socket_path=path, auth_token="spec-token")
    eq(gw.get("/v1/health", via_socket=True, auth=False).status, 200, "health public")
    eq(gw.get("/v1/models", via_socket=True, auth=False).status, 401, "models without token")
    eq(gw.get("/v1/models", via_socket=True).status, 200, "models with token")


@case("transport", gw=None)
def refusal_waits_for_late_body(t):
    """A request refused on its headers (401 over TCP, owner_only 403 over the socket) whose body arrives after them
    still gets the refusal, and the connection stays open for the next request: the gateway reads the body before
    answering, as Go's net/http does, so a client that writes headers and body separately never sees a broken pipe."""
    path = os.path.join(sock_dir(t), "gw.sock")
    gw = t.gateway("late-body", socket_path=path, auth_token="spec-token", limits=None)
    body = b'{"version": 1}'
    for via, line, auth, want in ((False, "POST /v1/agent/model.call", "", (401, "auth_required")),
                                  (True, "PUT /v1/budgets", "Authorization: Bearer spec-token\r\n", (403, "owner_only"))):
        s = socket.socket(socket.AF_UNIX) if via else socket.socket()
        s.settimeout(10)
        s.connect(path if via else ("127.0.0.1", gw.port))
        try:
            s.sendall(("%s HTTP/1.1\r\nHost: localhost\r\n%sContent-Type: application/json\r\nContent-Length: %d\r\n\r\n"
                       % (line, auth, len(body))).encode())
            time.sleep(0.3)  # the headers alone decide the refusal; the body comes later
            s.sendall(body)
            r = http.client.HTTPResponse(s)
            r.begin()
            eq((r.status, json.loads(r.read()).get("code")), want, "refused with a late body, socket=%s" % via)
            s.sendall(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            again = http.client.HTTPResponse(s)
            again.begin()
            eq(again.status, 200, "the same connection answers the next request, socket=%s" % via)
        finally:
            s.close()


@case("transport", gw=None)
def socket_lab_execute_refused(t):
    """Loopback-origin routes refuse over the socket (no peer IP): lab-execute is 403; over TCP it runs."""
    path = os.path.join(sock_dir(t), "gw.sock")
    gw = t.gateway("socket-lab", cfg(t), socket_path=path)
    body = {"work_order": {"executor": "fake-cli", "timeout_s": 5, "fake": {"stdout": "ran"}}}
    r = gw.post("/v1/ai/lab-execute", body, via_socket=True)
    eq((r.status, r.text), (403, "Lab execution is local only\n"), "over the socket")
    eq(gw.post("/v1/ai/lab-execute", body).json["stdout"], "ran", "over TCP")


@case("transport", gw=None)
def socket_stale_replaced_and_removed(t):
    """A dead socket left at the path is replaced at start; SIGTERM removes the file."""
    d = sock_dir(t)
    path = os.path.join(d, "gw.sock")
    s = socket.socket(socket.AF_UNIX)
    s.bind(path)
    s.close()  # bound, never listened: nobody answers
    gw = t.gateway("socket-stale", socket_path=path)
    eq(gw.get("/v1/health", via_socket=True).status, 200, "serving on the replaced socket")
    eq(gw.stop(), 0, "exit status on SIGTERM")
    eq(os.path.exists(path), False, "socket removed on clean shutdown")


@case("transport", gw=None)
def socket_refusals(t):
    """Startup refuses: a live socket in use, a non-socket file, a world-writable directory, a relative path."""
    d = sock_dir(t)
    live = os.path.join(d, "live.sock")
    first = t.gateway("socket-live", socket_path=live)
    code, log = t.gateway("socket-live-2", socket_path=live, expect_start=False).start_fails()
    eq((code != 0, "is in use by a running gateway" in log), (True, True), "live socket")
    eq(first.get("/v1/health", via_socket=True).status, 200, "the first gateway keeps its socket")
    plain = os.path.join(d, "plain")
    open(plain, "w").close()
    code, log = t.gateway("socket-file", socket_path=plain, expect_start=False).start_fails()
    eq((code != 0, "exists and is not a socket; refusing to remove it" in log, os.path.exists(plain)), (True, True, True), "regular file kept")
    wide = sock_dir(t, 0o777)
    code, log = t.gateway("socket-wide", socket_path=os.path.join(wide, "gw.sock"), expect_start=False).start_fails()
    eq((code != 0, "is world-writable" in log), (True, True), "world-writable directory")
    code, log = t.gateway("socket-rel", socket_path="rel/gw.sock", expect_start=False).start_fails()
    eq((code != 0, 'CSGATEWAY_SOCKET must be an absolute path or "default"' in log), (True, True), "relative path")


@case("transport", gw=None)
def socket_default_under_state_root(t):
    """CSGATEWAY_SOCKET=default is <HOME>/.contextstack/gateway/csgateway.sock (directory created 0700)."""
    home = sock_dir(t)  # short HOME: AF_UNIX paths are limited to ~104 bytes
    gw = t.gateway("socket-default", env={"CSGATEWAY_SOCKET": "default", "HOME": home})
    gw.socket_path = os.path.join(home, ".contextstack", "gateway", "csgateway.sock")
    eq(gw.get("/v1/health", via_socket=True).status, 200, "default socket serves")
    eq(stat.S_IMODE(os.stat(os.path.dirname(gw.socket_path)).st_mode), 0o700, "directory mode")
