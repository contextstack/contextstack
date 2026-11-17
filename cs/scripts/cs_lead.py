"""cs lead: bootstrap a hosted-lead Director conversation and its scoped credential.

Owner-only: creates the conversation bundle on a node, mints POST credentials/lead, writes 0600 files under the
install config. The token is written once and never printed. Stdlib only; loaded by scripts/cs."""
from __future__ import annotations

import hashlib
import math
import json
import os
import re
import sys
import uuid
import time
from urllib.parse import quote
from pathlib import Path

from _atomic_write import write_atomic as _estate_write_atomic  # noqa: E402

FORBIDDEN_PREFIX = "tasks/turn/"
FORBIDDEN_EXACT = frozenset({"conversation/context.md"})
CONTEXT_NEED = "conversation/context.md"
TASK_NEED = "tasks/turn"
# The hosted bundle needs session/mcp.json. Restricted planner/judge/patch
# templates disable MCP or its tools; other executors do not wire this session.
LEAD_MODELS = {
    "claude-code-sonnet": "Claude Sonnet",
    "claude-code": "Claude Code",
    "claude-code-opus": "Claude Opus",
}
DEFAULT_MODEL = next(iter(LEAD_MODELS))
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,99}")


class LeadError(Exception):
    code = 1


def kit_root() -> Path:
    return Path(__file__).resolve().parent.parent


def bundle_dir() -> Path:
    return kit_root() / "lead"


def session_dir(root: Path, conversation_id: str) -> Path:
    return root / "state" / "lead-sessions" / conversation_id


def session_credential_path(root: Path, conversation_id: str) -> Path:
    return session_dir(root, conversation_id) / "lead-credential"


def save_session_meta(root: Path, record: dict) -> None:
    cid = record.get("conversation_id")
    if not cid:
        return
    path = session_dir(root, cid) / "meta.json"
    payload = {k: record[k] for k in ("director", "node", "model", "title", "conversation_id") if record.get(k)}
    write_atomic(path, (json.dumps(payload, indent=2, sort_keys=True) + "\n").encode(), 0o600)


def bundle_files(model: str | None = None) -> dict[str, bytes]:
    """Every file in the shipped lead bundle template (paths the Director accepts)."""
    root = bundle_dir()
    if not root.is_dir():
        raise LeadError("lead bundle template missing at %s" % root)
    spec = (root / "spec.yaml").read_text(encoding="utf-8")
    if model:
        spec = re.sub(r"^model: .*\n", "model: %s\n" % model, spec, count=1, flags=re.MULTILINE)
    out: dict[str, bytes] = {"spec.yaml": spec.encode("utf-8")}
    for path in root.rglob("*"):
        if not path.is_file() or path.name == "spec.yaml":
            continue
        rel = path.relative_to(root).as_posix()
        out[rel] = path.read_bytes()
    return out


def read_spec_facts(yaml: bytes) -> tuple[str, list[str]]:
    """Minimal CLIENT-API §11 conversation spec checks (mirrors director thread::read_spec)."""
    model = None
    context: list[str] = []
    tasks: list[str] = []
    for line in yaml.decode("utf-8").splitlines():
        s = line.strip()
        if s.startswith("model:"):
            model = s.split(":", 1)[1].strip()
        elif s.startswith("context:"):
            inner = s.split(":", 1)[1].strip()
            if inner.startswith("[") and inner.endswith("]"):
                context = [x.strip() for x in inner[1:-1].split(",") if x.strip()]
        elif s.startswith("tasks:"):
            inner = s.split(":", 1)[1].strip()
            if inner.startswith("[") and inner.endswith("]"):
                tasks = [x.strip() for x in inner[1:-1].split(",") if x.strip()]
    if not model:
        raise LeadError("the bundle spec names no model")
    if CONTEXT_NEED not in context:
        raise LeadError("the bundle spec must list %s in context" % CONTEXT_NEED)
    if TASK_NEED not in tasks:
        raise LeadError("the bundle spec must list %s in tasks" % TASK_NEED)
    prompts = []
    for line in yaml.decode("utf-8").splitlines():
        if line.strip().startswith("system:"):
            p = line.split(":", 1)[1].strip()
            if p:
                prompts.append(p)
    prompts.extend(c for c in context if c != CONTEXT_NEED)
    return model, prompts


def validate_bundle(files: dict[str, bytes]) -> list[str]:
    """Director conversation bundle rules; empty list means valid."""
    problems: list[str] = []
    for path in files:
        if path in FORBIDDEN_EXACT or path.startswith(FORBIDDEN_PREFIX):
            problems.append("%s is reserved for files the Director writes for conversations" % path)
    if "spec.yaml" not in files:
        problems.append("the bundle has no spec.yaml")
        return problems
    try:
        model, prompts = read_spec_facts(files["spec.yaml"])
    except LeadError as e:
        problems.append(str(e))
        return problems
    for p in prompts:
        if p not in files:
            problems.append("the bundle has no %s, which the spec names" % p)
    if "session/mcp.json" not in files:
        problems.append("session/mcp.json wires cs mcp for the lead session")
    body = files.get("session/mcp.json", b"")
    if b"cs" in body and b"mcp" in body and b"credential-file" in body:
        pass
    else:
        problems.append("session/mcp.json must pass --credential-file to cs mcp")
    return problems


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_atomic(path: Path, data: bytes, mode: int = 0o600) -> None:
    _estate_write_atomic(path, data, mode=mode)


class OwnerClient:
    """Small Client API client for the local owner credential."""

    def __init__(self, url: str, credential_file: Path):
        import urllib.error
        import urllib.request

        self.base = url.rstrip("/") + "/api/v1/client/"
        self.credential_file = credential_file
        self._request = urllib.request
        self._error = urllib.error

    def token(self) -> str:
        try:
            return self.credential_file.read_text(encoding="utf-8").strip()
        except OSError as e:
            raise LeadError("cannot read owner credential %s: %s" % (self.credential_file, e.strerror))

    def request(self, method: str, path: str, body=None, raw: bytes | None = None, key: str | None = None, timeout: float = 60):
        headers = {"Authorization": "Bearer " + self.token(), "Accept": "application/json"}
        data = raw
        if raw is not None:
            headers["Content-Type"] = "application/octet-stream"
        elif body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if method == "POST":
            headers["Idempotency-Key"] = key or str(uuid.uuid4())
        req = self._request.Request(self.base + path, data=data, method=method, headers=headers)
        try:
            with self._request.urlopen(req, timeout=timeout) as answer:
                text = answer.read()
                try:
                    doc = json.loads(text or b"{}")
                except ValueError:
                    doc = None
                return answer.status, doc, text
        except self._error.HTTPError as e:
            text = e.read()
            try:
                doc = json.loads(text or b"{}")
            except ValueError:
                doc = {"error": text.decode("utf-8", "replace")[:400]}
            return e.code, doc, text
        except (self._error.URLError, TimeoutError, OSError) as error:
            raise LeadError("Could not reach the Director. Check cs status, then run cs lead create again to resume: %s" % error)

    def upload_files(self, files: dict[str, bytes]) -> list[dict]:
        listed = []
        for path, data in sorted(files.items()):
            digest = sha256(data)
            status, body, _ = self.request("PUT", "objects/" + digest, raw=data)
            if status != 200:
                detail = body.get("detail") or body.get("error") if isinstance(body, dict) else body
                raise LeadError("PUT objects/%s: HTTP %s %s" % (digest[:12], status, detail))
            listed.append({"path": path, "sha256": digest, "mode": 0o644})
        return listed


def resolve_node(cs_mod, explicit: str | None, owner: OwnerClient) -> str:
    if explicit:
        return explicit
    record = cs_mod.read_json(cs_mod.CONFIG / "install.json", {}) or {}
    if record.get("node"):
        return str(record["node"])
    status, body, _ = owner.request("GET", "nodes")
    if status != 200 or not isinstance(body, dict):
        raise LeadError("could not list nodes (HTTP %s); pass --node" % status)
    nodes = [n["name"] for n in body.get("nodes") or [] if isinstance(n, dict) and not n.get("revoked_at")]
    if not nodes:
        raise LeadError("no registered nodes; pass --node")
    if len(nodes) == 1:
        return nodes[0]
    raise LeadError("several nodes (%s); pass --node" % ", ".join(nodes))


def ready_model(owner):
    """Prefer Sonnet among templates that can use the hosted lead's MCP tools."""
    status, body, _ = owner.request("GET", "models")
    if status != 200 or not isinstance(body, dict):
        raise LeadError("Could not list models (HTTP %s). Try again." % status)
    available = {row.get("name") for row in body.get("models", [])
                 if isinstance(row, dict) and row.get("available") is True}
    return next((name for name in LEAD_MODELS if name in available), None)


def check_model(cs, model):
    """Use exactly init's subscription CLI probes and interactive login flow."""
    import cs_init
    from types import SimpleNamespace

    name = next((n for n, template in cs_init.AGENTS.items()
                 if template and (model == template or model.startswith(template + "-"))), None)
    if not name:
        raise LeadError("Cannot check sign-in for %s. Choose a subscription agent model verified by cs init." % model)
    probe = object.__new__(cs_init.Init)
    probe.cs, probe.args = cs, SimpleNamespace(yes=False)
    probe.nr = cs.node_release() if hasattr(cs, "node_release") else SimpleNamespace(SYSTEM_PATH=os.defpath)
    path = probe.agent_path() if hasattr(cs, "load_plist") else os.environ.get("CS_AGENT_PATH", os.environ.get("PATH", os.defpath))
    agent = probe.probe(name, path)
    if agent["path"] and agent["signed_in"] is not True and name in cs_init.LOGINS and probe.offer_login(agent):
        probe.login(agent)
        agent = probe.probe(name, path)
    if agent["signed_in"] is not True:
        raise LeadError("%s is not ready: %s. Sign in with that agent, then run cs lead create again." %
                        (cs_init.LABELS[name], agent["how"]))


def save_record(path, record):
    write_atomic(path, (json.dumps(record, indent=2, sort_keys=True) + "\n").encode())


def welcome(owner, record, save, say, timeout):
    if record.get("welcome_reply"):
        return
    if not record.get("welcome_key"):
        record["welcome_key"] = "lead-welcome-" + uuid.uuid4().hex
        save()
    if not record.get("welcome_turn"):
        status, body, _ = owner.request("POST", "conversations/%s/turns" % quote(record["conversation_id"], safe=""),
            body={"message": "Welcome! Briefly introduce yourself as my ContextStack lead. First use the contextstack MCP machines tool to check your connection, then tell me how we can start working together."},
            key=record["welcome_key"])
        if status not in (200, 201) or not (body or {}).get("turn", {}).get("turn_id"):
            raise LeadError("Could not send the welcome. Run cs lead create again to resume (HTTP %s)." % status)
        record["welcome_turn"] = body["turn"]["turn_id"]
        save()
    deadline = time.monotonic() + timeout
    while True:
        status, body, _ = owner.request("GET", "turns/" + quote(record["welcome_turn"], safe=""))
        turn = (body or {}).get("turn", {})
        if status != 200:
            raise LeadError("Could not check the welcome (HTTP %s). Run cs lead create again to resume." % status)
        if turn.get("state") == "done" and turn.get("reply"):
            record["welcome_reply"] = turn["reply"]
            save()
            return
        if turn.get("state") in ("error", "stopped"):
            record.pop("welcome_turn", None)
            record.pop("welcome_key", None)
            save()
            raise LeadError("The lead's welcome did not finish. Run cs doctor, then cs lead create to retry.")
        if time.monotonic() >= deadline:
            raise LeadError("The lead is still preparing its welcome. Run cs lead create again to resume waiting.")
        say("Waiting for your lead to reply…")
        time.sleep(min(3, max(0, deadline - time.monotonic())))


def open_conversation(cs, record, args, say):
    import cs_ui
    try:
        url = cs_ui.issue_url(cs)
    except Exception as error:
        say("Talk to your lead with cs ui: open Hosted lead and send a message.")
        if getattr(args, "verbose", False):
            say(str(error))
        return
    url += "&conversation=" + quote(record["conversation_id"], safe="")
    if sys.stdin.isatty() and not getattr(args, "no_open", False) and not getattr(args, "json", False):
        say("Opening your conversation…")
        if cs_ui.open_browser(url, cs.PLATFORM):
            return
    say(url)
    say("Open this link and send a message to talk to your lead.")


def create(args, cs_mod) -> int:
    import fcntl
    root = Path(args.config_root) if getattr(args, "config_root", None) else cs_mod.ROOT
    lock_path = root / "config/.hosted-lead.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with open(lock_path, "a") as lock:
        os.chmod(lock_path, 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX)
        return _create(args, cs_mod, root)


def _create(args, cs_mod, root):
    timeout = getattr(args, "welcome_timeout", 180)
    if not math.isfinite(timeout) or not 0 <= timeout <= 3600:
        raise LeadError("--welcome-timeout must be between 0 and 3600 seconds")
    quiet = getattr(args, "json", False)
    def say(message):
        if not quiet:
            print(message, flush=True)
    install = cs_mod.read_json(cs_mod.CONFIG / "install.json", {}) or {}
    director = args.director or install.get("director")
    if not director and cs_mod.installed():
        director = (cs_mod.director_data() or {}).get("url")
    if not director:
        raise LeadError("no Director URL; install with cs init")
    cred = Path(args.credential_file) if args.credential_file else root / "state/director/local-credential"
    if not cred.is_file():
        raise LeadError("owner credential missing; install with cs init or pass --credential-file")
    owner = OwnerClient(director, cred)
    meta_path = root / "config/hosted-lead.json"
    record = cs_mod.read_json(meta_path, {}) or {}
    if record and record.get("director") != director:
        if not getattr(args, "new", False):
            raise LeadError("This hosted lead belongs to another Director; use --new to replace it.")
    keep_primary = False
    if getattr(args, "new", False):
        keep_primary = bool((cs_mod.read_json(meta_path, {}) or {}).get("conversation_id"))
        record = {}
    if record.get("conversation_id"):
        status, body, _ = owner.request("GET", "conversations/" + quote(record["conversation_id"], safe=""))
        if status == 404:
            record = {}
        elif status != 200:
            raise LeadError("Could not check your existing lead (HTTP %s). Try again." % status)
        else:
            say("Your hosted lead is already set up" if record.get("welcome_reply") else "Resuming your hosted lead setup…")
    explicit = getattr(args, "model", None)
    model = explicit or record.get("model")
    reason = "requested with --model" if explicit else "saved lead model"
    if not model:
        model = ready_model(owner)
        reason = "signed in" if model else "sign-in required"
        model = model or DEFAULT_MODEL
    say("lead: using %s (%s)" % (LEAD_MODELS.get(model, model), reason))
    check_model(cs_mod, model)
    cred_name = getattr(args, "name", None) or "hosted-lead"
    if not NAME.fullmatch(cred_name):
        raise LeadError("credential name %r is invalid" % cred_name)
    if not record:
        record = {"director": director, "node": resolve_node(cs_mod, args.node, owner), "model": model,
                  "title": (getattr(args, "title", None) or "Hosted lead").strip() or "Hosted lead",
                  "create_key": "lead-create-" + uuid.uuid4().hex}
        if not keep_primary:
            save_record(meta_path, record)
    def save():
        save_session_meta(root, record)
        if not keep_primary:
            save_record(meta_path, record)
    if not record.get("conversation_id"):
        say("Setting up your hosted lead…")
        status, body, _ = owner.request(
            "POST",
            "conversations",
            body={"node": record["node"], "spec": "hosted-lead", "model": model, "title": record["title"]},
            key=record["create_key"],
        )
        if status not in (200, 201) or not (body or {}).get("conversation", {}).get("conversation_id"):
            raise LeadError("Could not create your lead (HTTP %s). Run cs lead create again to resume." % status)
        record["conversation_id"] = body["conversation"]["conversation_id"]
        save()
        save_session_meta(root, record)
    cred_path = session_credential_path(root, record["conversation_id"])
    credential_ok = False
    if record.get("credential_id") and cred_path.is_file():
        status, me, _ = OwnerClient(director, cred_path).request("GET", "me")
        if status not in (200, 401, 403):
            raise LeadError("Could not check the lead credential (HTTP %s). Try again." % status)
        credential_ok = status == 200 and (me or {}).get("conversation_id") == record["conversation_id"]
    if not credential_ok:
        # Minting returns a token only once. Revoke any orphan from an interrupted mint before replacing it.
        status, body, _ = owner.request("GET", "credentials")
        if status != 200:
            raise LeadError("Could not check the lead credential. Run cs lead create again to resume.")
        for credential in (body or {}).get("credentials", []):
            if credential.get("conversation_id") == record["conversation_id"] and not credential.get("revoked_at"):
                status, _, _ = owner.request("DELETE", "credentials/" + quote(credential["id"], safe=""))
                if status != 200:
                    raise LeadError("Could not replace an interrupted lead credential. Try again.")
        status, minted, _ = owner.request("POST", "credentials/lead", body={"name": cred_name, "conversation_id": record["conversation_id"]})
        if status != 201 or not (minted or {}).get("token"):
            raise LeadError("Could not prepare the lead credential (HTTP %s). Run cs lead create again to resume." % status)
        write_atomic(cred_path, (minted["token"] + "\n").encode())
        record.update(credential_id=minted.get("credential_id"), credential_file="lead-credential")
        save_session_meta(root, record)
        if not keep_primary:
            write_atomic(root / "config/lead-credential", (minted["token"] + "\n").encode())
        save()
    if not record.get("welcome_reply"):
        say("Checking your lead with a welcome message…")
    welcome(owner, record, save, say, timeout)
    label = LEAD_MODELS.get(model, "Codex" if model == "codex" else model)
    say("Hosted lead ready on %s (%s)." % (record["node"], label))
    say("Your lead says: " + record["welcome_reply"])
    if not keep_primary:
        save_record(meta_path, record)
    if quiet:
        print(json.dumps(record, sort_keys=True))
    else:
        if getattr(args, "verbose", False):
            print(json.dumps(record, indent=2, sort_keys=True))
        open_conversation(cs_mod, record, args, say)
    return 0


def add_create_flags(parser):
    parser.add_argument("--new", action="store_true", help="create a separate lead conversation")
    parser.add_argument("--no-open", action="store_true", help="print the conversation link without opening a browser")
    parser.add_argument("--json", action="store_true", help="print the setup record as JSON")
    parser.add_argument("--verbose", action="store_true", help="include setup IDs and details")
    parser.add_argument("--welcome-timeout", type=float, default=180, help="seconds to wait for the welcome (default: 180)")


def main_standalone(argv=None):
    """Entry without the cs wrapper (tests)."""
    import argparse

    ap = argparse.ArgumentParser(prog="cs lead")
    sub = ap.add_subparsers(dest="cmd", required=True)
    cr = sub.add_parser("create")
    cr.add_argument("--director")
    cr.add_argument("--credential-file")
    cr.add_argument("--config-root", help="ContextStack root for config output (default: ~/.contextstack)")
    cr.add_argument("--node")
    cr.add_argument("--model", help="executor template (default: ready lead-capable model, preferring Claude Sonnet)")
    cr.add_argument("--name", help="lead credential name (default: hosted-lead)")
    cr.add_argument("--title", help="conversation title (default: Hosted lead)")
    add_create_flags(cr)
    args = ap.parse_args(argv)
    cs = types_shim()
    try:
        return create(args, cs) if args.cmd == "create" else 2
    except LeadError as e:
        print("cs lead: %s" % e, file=sys.stderr)
        return e.code


def types_shim():
    """Minimal cs module surface for tests."""
    from types import SimpleNamespace

    home = Path(os.environ.get("HOME") or os.path.expanduser("~"))
    root = Path(os.environ.get("CS_ROOT") or home / ".contextstack")

    def read_json(path, default=None):
        try:
            return json.loads(Path(path).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return default

    def installed():
        return (root / "current").is_symlink()

    def director_data():
        return None

    return SimpleNamespace(HOME=home, ROOT=root, CONFIG=root / "config", read_json=read_json, installed=installed, director_data=director_data)
