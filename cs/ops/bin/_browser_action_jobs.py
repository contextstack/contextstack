"""Execute approved browser follow-ups from the Director's node-only queue."""
import hmac
import json
import re
import socket
from pathlib import Path

from _browser_reliability import BrowserFailure
from _director_client import DirectorUnreachable
from _estate_io import write_atomic

MAX_REPLY = 64 * 1024
MAX_CAPTURE_REPLY = 512 * 1024
MAX_SCREENSHOT = 3 * 1024 * 1024


def action_key(client):
    return hmac.digest(client.token.encode("utf-8"), b"contextstack-browser-action-key-v1", "sha256")


def configured_sockets(client, browser):
    try:
        config = json.loads((browser / "native-host.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    key_path = browser / "action.key"
    if not isinstance(config, dict) or config.get("action_key_file") != str(key_path):
        return None
    content = action_key(client).hex() + "\n"
    try:
        current = key_path.read_text(encoding="utf-8")
    except (OSError, ValueError):
        current = None
    if current != content:
        write_atomic(key_path, content, mode=0o600, encoding="utf-8")
    return browser / "native-sockets"


def broker_call(socket_path, request, limit):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(50)
        connection.connect(str(socket_path))
        connection.sendall(json.dumps(request).encode("utf-8") + b"\n")
        with connection.makefile("rb") as response:
            line = response.readline(limit + 1)
    if not line or len(line) > limit:
        raise ValueError("Invalid browser broker response.")
    return json.loads(line)


def propose(client, job, run, proposal):
    if not isinstance(proposal, dict) or set(proposal) != {"url", "origin_set", "action", "target"}:
        raise ValueError("Provide only url, origin_set, action, and target for a browser action.")
    origin_set = proposal["origin_set"]
    if not isinstance(origin_set, str) or not re.fullmatch(r"[a-z0-9-]{1,80}", origin_set):
        raise ValueError("Invalid browser origin set.")
    if not all(isinstance(proposal[field], str) for field in ("url", "action", "target")):
        raise ValueError("Browser action fields must be strings.")
    path = f"jobs/{job}/action-approvals"
    try:
        client.call("POST", path + "/capture", dict(proposal, run_id=run))
    except DirectorUnreachable as error:
        if error.status in (400, 403, 409, 422):
            raise BrowserFailure("action_refused") from None
        raise
    browser = Path.home() / ".contextstack/browser"
    sockets = configured_sockets(client, browser)
    if sockets is None:
        raise BrowserFailure("native_host_unregistered")
    request = dict(proposal, verb="capture_action", job_id=job, run_id=run)
    wire = json.dumps(["contextstack-browser-capture-request-v1", request], sort_keys=True,
                      separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    request["host_proof"] = hmac.digest(action_key(client), wire, "sha256").hex()
    try:
        capture = broker_call(sockets / (origin_set + ".sock"), request, MAX_CAPTURE_REPLY)
    except TimeoutError:
        raise BrowserFailure("page_timeout") from None
    except (OSError, ValueError):
        raise BrowserFailure("unknown") from None
    if not isinstance(capture, dict):
        raise BrowserFailure("unknown")
    if capture.get("ok") is not True:
        raise BrowserFailure(capture.get("error_kind", "action_refused"), capture.get("error"))
    sha = capture["capture"]["screenshot_sha"]
    if not re.fullmatch(r"[a-f0-9]{64}", sha):
        raise ValueError("Invalid browser screenshot hash.")
    # Only the host bridge can read the screenshot. The broker socket returns the attestation alone.
    with (browser / "action-screenshots" / sha).open("rb") as snapshot:
        image = snapshot.read(MAX_SCREENSHOT + 1)
    if len(image) > MAX_SCREENSHOT:
        raise ValueError("Browser screenshot is too large.")
    return client.call("POST", path, {
        "kind": "browser_action", "run_id": run, "capture": capture["capture"],
        "capture_proof": capture["capture_proof"], "image": image.hex(),
    })


def execute(socket_path, page, claim):
    request = {"verb": "execute_action", "url": page, "approval": claim}
    response = broker_call(socket_path, request, MAX_REPLY)
    if response.get("ok") is False and isinstance(response.get("error"), str):
        return {"status": "refused", "reason": response["error"]}
    if response.get("ok") is True and response.get("outcome") == "submitted":
        return {"status": "submitted"}
    return {"status": "submission_unknown"}


def poll(client, sockets=None):
    if sockets is None:
        sockets = configured_sockets(client, Path.home() / ".contextstack/browser")
    if sockets is None or not sockets.is_dir():
        return
    for row in client.call("GET", "browser-action-jobs").get("jobs", []):
        execution = row["execution"]
        socket_path = sockets / (execution["origin_set"] + ".sock")
        if not socket_path.is_socket():
            continue
        job, approval = row["job_id"], row["approval_id"]
        path = f"jobs/{job}/action-approvals/{approval}"
        binding = {key: row[key] for key in ("origin", "action", "target", "field_values_digest")}
        try:
            claim = client.call("POST", path + "/claim", dict(binding, run_id=job))
        except DirectorUnreachable as error:
            if error.status == 409:
                continue
            raise
        proof = claim.get("claim_proof") if isinstance(claim, dict) else None
        if not isinstance(proof, str) or len(proof) != 64:
            continue
        try:
            outcome = execute(socket_path, execution["page_url"], claim)
        except (OSError, ValueError, AttributeError):
            outcome = {"status": "submission_unknown"}
        # The Director's claim is final. A lost browser reply must never lead to a retry.
        client.call("POST", path + "/outcome", {"run_id": job, **outcome})
