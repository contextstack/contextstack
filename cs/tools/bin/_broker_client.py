import json
import os
import socket
import subprocess

from _web import fail

MAX_REPLY = 8 * 1024 * 1024
TIMEOUT = 120


def ask(set_name, request, max_reply=MAX_REPLY):
    line = json.dumps(request, sort_keys=True).encode() + b"\n"
    fake = os.environ.get("CS_BROWSER_BROKER_EXEC")
    if fake:
        done = subprocess.run([fake], input=line, capture_output=True, timeout=TIMEOUT)
        raw = done.stdout
    else:
        var = "CS_SOCKET_BROWSER_" + set_name.upper().replace("-", "_")
        path = os.environ.get(var)
        if not path:
            fail("no browser broker socket granted for set %s (%s unset)" % (set_name, var))
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
            conn.settimeout(TIMEOUT)
            try:
                conn.connect(path)
            except socket.timeout:
                raise
            except OSError as error:
                fail("browser broker unreachable at %s: %s" % (var, error.strerror or error))
            conn.sendall(line)
            with conn.makefile("rb") as stream:
                raw = stream.readline(max_reply + 1)
    if len(raw) > max_reply or not raw.strip():
        fail("browser broker reply is empty or exceeds %d bytes" % max_reply)
    reply = json.loads(raw)
    if not isinstance(reply, dict):
        fail("browser broker reply is not an object")
    return reply
