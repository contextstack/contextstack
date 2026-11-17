"""Client-side helpers (CLIENT-API.md): a client credential's calls, the fixture bundle uploaded as objects, and
an event-stream reader."""
import http.client
import json
import secrets
import socket
import time
import urllib.parse

from harness import wire

CLIENT = "/api/v1/client/"


def new_key(label="k"):
    return "%s-%s" % (label, secrets.token_hex(8))


def bundle_files():
    """The fixture job's files as an intent lists them."""
    return [{"path": p, "sha256": wire.sha(data), "mode": mode} for p, (data, mode) in sorted(wire.JOB_FILES.items())]


class Client:
    """One client credential talking to a Director."""

    def __init__(self, target, token, credential_id=None):
        self.target, self.token, self.credential_id = target, token, credential_id

    def call(self, method, path, body=None, raw=None, key=None, headers=None, token="own"):
        h = dict(headers or {})
        if key is not None:
            h["Idempotency-Key"] = key
        return wire.request(self.target.url, method, CLIENT + path, token=self.token if token == "own" else token,
                            headers=h, body=body, raw=raw)

    def upload_bundle(self):
        for data, _ in wire.JOB_FILES.values():
            a = self.call("PUT", "objects/" + wire.sha(data), raw=data)
            if a.status != 200:
                raise RuntimeError("PUT objects: %r" % a)
        return bundle_files()

    def intent(self, node, /, **over):
        body = {"node": node, "spec": "spec.yaml", "task": "tasks/t", "files": bundle_files(), "title": "fixture"}
        body.update(over)
        return {k: v for k, v in body.items() if v is not None}

    def submit(self, node, /, key=None, **over):
        return self.call("POST", "intents", body=self.intent(node, **over), key=key or new_key("intent"))

    def stream(self):
        return Stream(self.target.url, self.token)

    def stream_for(self, conversation):
        """GET events?conversation=ID: only that conversation's changes and live events."""
        return Stream(self.target.url, self.token, query="?conversation=" + conversation)


class Stream:
    """GET events, read frame by frame."""

    def __init__(self, base, token, timeout=10, query=""):
        url = urllib.parse.urlsplit(base)
        self.conn = http.client.HTTPConnection(url.hostname, url.port, timeout=timeout)
        self.conn.request("GET", CLIENT + "events" + query, headers={"Authorization": "Bearer " + token,
                                                             "Accept": "text/event-stream"})
        self.response = self.conn.getresponse()
        self.status = self.response.status
        self.headers = {k.lower(): v for k, v in self.response.getheaders()}
        self.ended = False

    def next(self, timeout=5):
        """The next event as (name, data, id); None when nothing arrives in time or the stream ended."""
        deadline = time.time() + timeout
        name, data, eid = None, [], None
        while not self.ended:
            left = deadline - time.time()
            if left <= 0:
                return None
            self.conn.sock.settimeout(left)
            try:
                line = self.response.readline()
            except (socket.timeout, TimeoutError):
                return None
            if not line:
                self.ended = True
                return None
            line = line.decode("utf-8").rstrip("\r\n")
            if line == "":
                if name or data:
                    return name or "message", json.loads("\n".join(data)) if data else None, eid
                continue
            if line.startswith(":"):
                continue
            field, _, value = line.partition(":")
            value = value[1:] if value.startswith(" ") else value
            if field == "event":
                name = value
            elif field == "data":
                data.append(value)
            elif field == "id":
                eid = value
        return None

    def until(self, want, timeout=5):
        """Events until one satisfies want(name, data); returns (that event, everything read before it)."""
        seen, deadline = [], time.time() + timeout
        while time.time() < deadline:
            e = self.next(max(0.05, deadline - time.time()))
            if e is None:
                if self.ended:
                    break
                continue
            if want(e[0], e[1]):
                return e, seen
            seen.append(e)
        return None, seen

    def close(self):
        try:
            self.conn.close()
        except OSError:
            pass
