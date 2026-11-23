"""The node's client for the director's node API (CS phase 6). Not an op.

policy/spool.json receiver {kind: "director", url, node, token_env, installation, protocol?} declares this host a
fleet node of that director (Phase6.EstateSplit design §4). The bearer token comes from config/node.env via the in-process loader,
under the name token_env; it never enters a child environment. Every call is api/v1/nodes/me/<path>; a transport failure is
DirectorUnreachable, which callers treat as "try again next tick" (nothing is lost).

Node protocol (tests/director-spec/NODE-PROTOCOL.md): every request names ContextStack-Protocol and, when the
receiver names one, ContextStack-Installation: <installation> (the ID registration printed). The node speaks 2
(§9) and falls back to 1 against a Director that speaks only 1 (the Laravel reference): a 426 whose
ContextStack-Protocol-Supported lists 1 and not 2 is asked again once in 1, and a 2xx naming 1 is used for the
five protocol-1 calls, which mean the same in both versions. `client.protocol` is then 1 for the rest of the
process; receiver `protocol: 1` pins it. An answer is used only when it names that installation and, on success,
the protocol in use; a refusal (426, 421) or a foreign Director is WrongDirector or ProtocolRefused, both
DirectorUnreachable, so callers change nothing and say why.

Connecting (TCP, and the TLS handshake) gets CONNECT_TIMEOUT_S; the request then reads with the client's
timeout (or a call's own, for the held-open GET work). An unreachable director used to hold every tick for the
full 30 s read timeout (prod-1).
"""
import http.client
import json
import re
import urllib.error
import urllib.request
from _node_env import node_env

CONNECT_TIMEOUT_S = 3.0
PROTOCOL = 2  # spoken first; FALLBACK against a protocol-1 Director
FALLBACK = 1
PROTOCOL_HEADER, SUPPORTED_HEADER, INSTALLATION_HEADER = (
    "ContextStack-Protocol", "ContextStack-Protocol-Supported", "ContextStack-Installation")
INSTALLATION = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
PROTOCOL1_PATH = re.compile(r"jobs|jobs/[^/?]+/accept|objects/[a-f0-9]{64}|envelopes")  # §4-7, alike in 1 and 2


def _connect_quickly(conn, connect):
    """Run `connect` with the connect timeout, then give the socket the read timeout back."""
    read = conn.timeout
    conn.timeout = CONNECT_TIMEOUT_S if read is None else min(read, CONNECT_TIMEOUT_S)
    try:
        connect()
    finally:
        conn.timeout = read
    conn.sock.settimeout(read)


class _HTTPConnection(http.client.HTTPConnection):
    def connect(self):
        _connect_quickly(self, super().connect)


class _HTTPSConnection(http.client.HTTPSConnection):
    def connect(self):
        _connect_quickly(self, super().connect)


class _HTTPHandler(urllib.request.HTTPHandler):
    def http_open(self, req):
        return self.do_open(_HTTPConnection, req)


class _HTTPSHandler(urllib.request.HTTPSHandler):
    def https_open(self, req):
        return self.do_open(_HTTPSConnection, req, context=self._context)


OPENER = urllib.request.build_opener(_HTTPHandler, _HTTPSHandler)


class DirectorUnreachable(OSError):
    """No answer, or an HTTP error status: the call did not happen as far as the node can tell.
    `status` and `answer` carry an HTTP refusal's status and JSON body (None for a transport failure)."""

    def __init__(self, message, status=None, answer=None):
        super().__init__(message)
        self.status, self.answer = status, answer


def _json(error):
    try:
        return json.loads(error.read() or b"null")
    except (OSError, ValueError):
        return None


class WrongDirector(DirectorUnreachable):
    """The answer came from another installation, or the Director says this node is configured for another."""


class ProtocolRefused(DirectorUnreachable):
    """The Director does not speak this node's protocol version (426), or answered without naming it."""


class _Downgrade(Exception):
    """A 426 from a protocol-1 Director: ask again in protocol 1."""


def receiver_config(receiver):
    """Validate a kind "director" receiver; ValueError names what is wrong."""
    url, node, token_env = (str(receiver.get(k) or "") for k in ("url", "node", "token_env"))
    if not url.startswith(("http://", "https://")) or not node or not token_env:
        raise ValueError('policy/spool.json: receiver kind "director" wants {url: http(s)://..., node, token_env, installation}')
    installation = receiver.get("installation")
    if installation is not None and not (isinstance(installation, str) and INSTALLATION.fullmatch(installation)):
        raise ValueError("policy/spool.json: receiver installation must match %s" % INSTALLATION.pattern)
    protocol = receiver.get("protocol", PROTOCOL)
    if protocol not in (FALLBACK, PROTOCOL) or isinstance(protocol, bool):
        raise ValueError("policy/spool.json: receiver protocol must be 1 or 2 (absent: 2, falling back to 1)")
    return {"kind": "director", "url": url.rstrip("/"), "node": node, "token_env": token_env, "installation": installation,
            "protocol": protocol}


class Client:
    def __init__(self, config, timeout=30, token=None):
        """`token`, when passed, is the bearer. Otherwise it is read from node.env under `token_env`."""
        self.base = config["url"] + "/api/v1/nodes/me/"
        self.token = (node_env().get(config["token_env"]) or "") if token is None else token
        self.timeout = timeout
        self.installation = config.get("installation")
        self.protocol = config.get("protocol") or PROTOCOL
        if not self.token:
            raise ValueError("director token: %s is not set" % config["token_env"])

    def call(self, method, path, body=None, raw=None, timeout=None, headers=None):
        """JSON in (body) or bytes in (raw); JSON out, or bytes when the answer is not JSON.
        `headers` adds request headers (a landing report sends Idempotency-Key)."""
        try:
            return self._call(method, path, body, raw, timeout, headers)
        except _Downgrade:
            return self._call(method, path, body, raw, timeout, headers)

    def _call(self, method, path, body, raw, timeout, headers=None):
        data, ctype = (raw, "application/octet-stream") if raw is not None else (
            (json.dumps(body).encode("utf-8"), "application/json") if body is not None else (None, None))
        request = urllib.request.Request(self.base + path, data=data, method=method)
        request.add_header("Authorization", "Bearer " + self.token)
        request.add_header("Accept", "application/json")
        sent = self.protocol
        shared_path = PROTOCOL1_PATH.fullmatch(path.split("?")[0])
        where = "director %s %s" % (method, path)
        # A concurrent request can discover the downgrade after a listener
        # duty has checked the protocol. Guard the version used on the wire.
        if sent == FALLBACK and not shared_path:
            raise ProtocolRefused("%s: the director speaks node protocol 1 only" % where)
        request.add_header(PROTOCOL_HEADER, str(sent))
        if self.installation:
            request.add_header(INSTALLATION_HEADER, self.installation)
        if ctype:
            request.add_header("Content-Type", ctype)
        for name, value in (headers or {}).items():
            if value is not None:
                request.add_header(name, value)
        try:
            with OPENER.open(request, timeout=timeout or self.timeout) as response:
                self._check_answer(where, response.headers)
                answered = response.headers.get(PROTOCOL_HEADER)
                if answered == str(FALLBACK) and sent == PROTOCOL and shared_path:
                    self.protocol = FALLBACK  # a protocol-1 Director answered a call that means the same in 1
                elif answered != str(sent):
                    raise ProtocolRefused("%s: the answer does not name node protocol %d (%s: %r)"
                                          % (where, sent, PROTOCOL_HEADER, answered))
                payload = response.read()
                if (response.headers.get("Content-Type") or "").startswith("application/json"):
                    out = json.loads(payload or b"{}")
                    if response.headers.get("Idempotent-Replayed", "").lower() == "true" and isinstance(out, dict):
                        out = {**out, "_idempotent_replay": True}
                    return out
                return payload
        except urllib.error.HTTPError as error:
            self._check_answer(where, error.headers)
            if error.code == 426:
                supported = (error.headers.get(SUPPORTED_HEADER) or "").replace(" ", "").split(",")
                if sent == PROTOCOL and str(FALLBACK) in supported and str(PROTOCOL) not in supported:
                    self.protocol = FALLBACK
                    if shared_path:
                        raise _Downgrade() from error  # asked again in 1; a protocol-2 call has no meaning there
                    raise ProtocolRefused("%s: the director speaks node protocol 1 only" % where) from error
                raise ProtocolRefused("%s: HTTP 426, the director speaks node protocol %s, this node %d"
                                      % (where, error.headers.get(SUPPORTED_HEADER) or "?", sent)) from error
            if error.code == 421:
                raise WrongDirector("%s: HTTP 421, the director refuses installation %r: set policy/spool.json "
                                    "receiver.installation to the ID director:node-register printed"
                                    % (where, self.installation)) from error
            raise DirectorUnreachable("%s: HTTP %s" % (where, error.code), error.code, _json(error)) from error
        except DirectorUnreachable:
            raise
        except (urllib.error.URLError, OSError, ValueError) as error:
            raise DirectorUnreachable("%s: %s" % (where, error)) from error

    def _check_answer(self, where, headers):
        """An answer naming another installation is not from this node's Director: never act on it."""
        theirs = headers.get(INSTALLATION_HEADER) if headers is not None else None
        if self.installation and theirs is not None and theirs != self.installation:
            raise WrongDirector("%s: answered by installation %r, this node belongs to %r" % (where, theirs, self.installation))
