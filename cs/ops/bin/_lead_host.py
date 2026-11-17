#!/usr/bin/env python3
"""Hosted lead session host: one live CLI per conversation, turn jobs over a unix socket.

Demanded-by: Design.HostedLead 2026-11-11 lane 3.

Serve lifecycle: ``lead-host serve`` (or ``ensure_serving`` from a turn client) binds a
unix socket, records ``serve_pid`` in state, and polls until exit conditions fire: the
state root, ``lead-sessions/``, or this conversation's state file disappears; ``idle`` seconds
since the last completed turn (``LEAD_HOST_IDLE_EXIT_S``, default 1800); or the watcher parent
(``LEAD_HOST_PARENT_PID``) is gone. Turn clients use ``ensure_serving`` to spawn detached
when the socket is absent or unresponsive; the spawned serve does not watch that client (it exits right
after its turn), only ``LEAD_HOST_WATCH_PID`` when one is set.

Turns for one conversation serialize on ``ConversationHost._lock``: a second concurrent
socket ``turn`` blocks until the in-flight turn finishes (no parallel CLI turns).
"""
from __future__ import annotations

import hashlib
import hmac
import secrets
import shutil
import stat
import fcntl
import json
import os
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path


from _job_reason import persist_output, redact as redact_secrets
from _lead_session import ClaudeStreamSession, prepare_runtime_home, redact_msg
from _lead_wake import start_wake
from _lead_socket import private_socket_dir, read_socket_token, remove_socket_dir, sweep_socket_dirs, temporary_socket_dir
from _roots import roots

HERE = Path(__file__).resolve().parent
_SANDBOX_CMD_CACHE = None
HOSTED_SPEC_ID = "hosted-lead"
CRED_NAME = "lead-credential"
CRED_LOCK_NAME = "lead-credential.lock"
SERVE_POLL_S = 1.0
DEFAULT_IDLE_EXIT_S = 30 * 60


def session_dir(state: Path, conversation_id: str) -> Path:
    return sessions_root(state) / conversation_id


def session_credential_path(state: Path, conversation_id: str) -> Path:
    return session_dir(state, conversation_id) / CRED_NAME


def owner_credential_path(state: Path) -> Path:
    return state / "director" / "local-credential"


def migrate_legacy_lead_config(config_root: Path, state: Path) -> None:
    """Move config/hosted-lead.json and config/lead-credential into per-conversation storage."""
    meta_path = config_root / "hosted-lead.json"
    if not meta_path.is_file():
        return
    try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return
    cid = str(meta.get("conversation_id") or "").strip()
    if not cid:
        return
    sess = session_dir(state, cid)
    sess.mkdir(parents=True, exist_ok=True)
    session_meta = sess / "meta.json"
    if not session_meta.is_file():
        payload = {k: meta[k] for k in ("director", "node", "model", "title") if meta.get(k)}
        payload["conversation_id"] = cid
        session_meta.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    dest = session_credential_path(state, cid)
    if dest.is_file():
        return
    try:
        src = resolve_credential_under_config(config_root, str(meta.get("credential_file") or CRED_NAME))
    except SystemExit:
        return
    if src.is_file():
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dest)
        os.chmod(dest, 0o600)


def conversation_meta(config_root: Path, state: Path, conversation_id: str) -> dict:
    migrate_legacy_lead_config(config_root, state)
    meta_path = session_dir(state, conversation_id) / "meta.json"
    meta = {}
    if meta_path.is_file():
        try:
            meta = json.loads(meta_path.read_text(encoding="utf-8"))
            if meta.get("director"):
                return meta
        except (OSError, ValueError):
            pass
    legacy = config_root / "hosted-lead.json"
    if legacy.is_file():
        try:
            legacy_meta = json.loads(legacy.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            legacy_meta = {}
        if legacy_meta.get("conversation_id") == conversation_id and legacy_meta.get("director"):
            return legacy_meta
    for path in (config_root / "policy/spool.json", config_root / "install.json"):
        try:
            config = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        director = (config.get("receiver") or {}).get("url") if path.name == "spool.json" else config.get("director")
        if director:
            meta.update(director=director, conversation_id=conversation_id)
            meta_path.parent.mkdir(parents=True, exist_ok=True)
            meta_path.write_text(json.dumps(meta, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            os.chmod(meta_path, 0o600)
            return meta
    return meta


def resolve_conversation_id_or_none(config_root: Path) -> str | None:
    meta_path = config_root / "hosted-lead.json"
    try:
        return str(json.loads(meta_path.read_text(encoding="utf-8")).get("conversation_id") or "") or None
    except (OSError, ValueError):
        return None


def _director_receiver(config_root: Path, meta: dict) -> dict:
    """Node receiver settings for calling the Director's node API."""
    from _director_client import receiver_config
    receiver = None
    for path in (config_root / "policy/spool.json", config_root / "install.json"):
        try:
            config = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if path.name == "spool.json":
            receiver = config.get("receiver")
        elif config.get("director"):
            receiver = {"kind": "director", "url": config["director"], "node": config.get("node"), "token_env": "CONTEXTSTACK_NODE_TOKEN"}
    if not isinstance(receiver, dict) or receiver.get("kind") != "director":
        return {}
    if meta.get("director"):
        receiver = dict(receiver)
        receiver["url"] = meta["director"]
    if meta.get("node"):
        receiver = dict(receiver)
        receiver["node"] = meta["node"]
    try:
        return receiver_config(receiver)
    except ValueError:
        return {}


def _write_session_credential_locked(state: Path, conversation_id: str, token: str) -> None:
    cred_path = session_credential_path(state, conversation_id)
    cred_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = cred_path.parent / CRED_LOCK_NAME
    scripts = HERE.parents[1] / "scripts"
    if str(scripts) not in sys.path:
        sys.path.insert(0, str(scripts))
    import cs_lead

    with open(lock_path, "a") as lock_file:
        os.chmod(lock_path, 0o600)
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        cs_lead.write_atomic(cred_path, (str(token).strip() + "\n").encode())


def _mint_lead_credential_from_node(conversation_id: str, config_root: Path, state: Path) -> str | None:
    migrate_legacy_lead_config(config_root, state)
    meta = conversation_meta(config_root, state, conversation_id)
    cfg = _director_receiver(config_root, meta)
    if not cfg:
        return None
    from urllib.parse import quote
    from _director_client import Client, DirectorUnreachable
    from _node_env import node_env

    token = node_env(config_root / "node.env").get(cfg["token_env"]) or os.environ.get(cfg["token_env"]) or ""
    if not token:
        return None
    try:
        answer = Client(cfg, timeout=15, token=token).call(
            "POST", "conversations/%s/lead-credential" % quote(conversation_id, safe=""))
    except DirectorUnreachable:
        return None
    lead = (answer or {}).get("token") if isinstance(answer, dict) else None
    return str(lead).strip() if lead else None


def fetch_lead_credential_from_director(conversation_id: str, config_root: Path, state: Path) -> bool:
    """Fetch a lead credential from the Director node route; never prints the token."""
    if session_credential_path(state, conversation_id).is_file():
        return True
    minted = _mint_lead_credential_from_node(conversation_id, config_root, state)
    if not minted:
        return False
    _write_session_credential_locked(state, conversation_id, minted)
    return True


def refetch_lead_credential_from_director(conversation_id: str, config_root: Path, state: Path) -> bool:
    """Replace the stored lead credential from the node route (used after Client API 401)."""
    minted = _mint_lead_credential_from_node(conversation_id, config_root, state)
    if not minted:
        return False
    _write_session_credential_locked(state, conversation_id, minted)
    return True


def director_client_for_host(host: "ConversationHost") -> "DirectorClient":
    from _lead_wake import DirectorClient

    meta = host.conversation_meta()
    url = str(meta.get("director") or os.environ.get("LEAD_WAKE_DIRECTOR_URL", "")).strip()
    if not url:
        fail("no Director URL for this conversation")

    def refetch() -> bool:
        return refetch_lead_credential_from_director(host.conversation_id, host.config_root, host.state)

    return DirectorClient(url, host.wake_credential_path(), refetch_credential=refetch)


def ensure_session_credential(conversation_id: str, config_root: Path, state: Path, model: str) -> None:
    """Ensure the session holds a lead credential: fetch from the Director, or mint with a local owner."""
    if session_credential_path(state, conversation_id).is_file():
        return
    if fetch_lead_credential_from_director(conversation_id, config_root, state):
        return
    ensure_lead_ready(conversation_id, config_root, state, model)


def ensure_lead_ready(conversation_id: str, config_root: Path, state: Path, model: str) -> None:
    migrate_legacy_lead_config(config_root, state)
    meta = conversation_meta(config_root, state, conversation_id)
    director = str(meta.get("director") or "").strip()
    if not director:
        fail("no Director URL for conversation %s" % conversation_id)
    owner_path = owner_credential_path(state)
    cred_path = session_credential_path(state, conversation_id)
    scripts = HERE.parents[1] / "scripts"
    if str(scripts) not in sys.path:
        sys.path.insert(0, str(scripts))
    import cs_lead
    if not cred_path.is_file():
        if not owner_path.is_file():
            fail("cannot host a new lead without the owner credential on this machine")
        owner = cs_lead.OwnerClient(director, owner_path)
        # Mint only for a conversation the Director marks as a hosted lead (or the legacy single lead,
        # which predates the marker); a turn job's spec id alone is not enough.
        from urllib.parse import quote
        status, body, _ = owner.request("GET", "conversations/" + quote(conversation_id, safe=""))
        if status != 200:
            fail("could not read conversation %s (HTTP %s)" % (conversation_id, status))
        legacy = resolve_conversation_id_or_none(config_root) == conversation_id
        if not ((body or {}).get("conversation") or {}).get("lead") and not legacy:
            fail("conversation %s is not a hosted lead; refusing to mint a lead credential" % conversation_id)
        status, minted, _ = owner.request(
            "POST",
            "credentials/lead",
            body={"name": "hosted-lead", "conversation_id": conversation_id},
        )
        if status != 201 or not (minted or {}).get("token"):
            fail("could not mint lead credential (HTTP %s)" % status)
        cs_lead.write_atomic(cred_path, (minted["token"] + "\n").encode())


def _idle_exit_seconds() -> float:
    raw = os.environ.get("LEAD_HOST_IDLE_EXIT_S", "").strip()
    if not raw:
        return float(DEFAULT_IDLE_EXIT_S)
    try:
        return max(0.0, float(raw))
    except ValueError:
        return float(DEFAULT_IDLE_EXIT_S)


def _watch_parent_pid() -> int | None:
    raw = os.environ.get("LEAD_HOST_PARENT_PID", "").strip()
    if not raw:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except OSError:
        return True
    return True


def _stop_host_session(host: ConversationHost) -> None:
    """Drop the CLI child quickly, then finalize private login copies before host exit."""
    sess = host._session
    if sess and getattr(sess, "mcp_bridge", None):
        sess.mcp_bridge.close()
    if not sess:
        return
    proc = sess.proc
    if proc and proc.poll() is None:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(proc.pid, sig)
            except (ProcessLookupError, OSError):
                try:
                    proc.kill()
                except OSError:
                    pass
            try:
                proc.wait(timeout=1)
                break
            except subprocess.TimeoutExpired:
                continue
    if (not proc or proc.poll() is not None) and hasattr(sess, "_finish_login_copies"):
        sess._finish_login_copies()


def serve_should_exit(
    state: Path,
    conversation_id: str,
    *,
    last_turn_at: float,
    started_at: float,
    host: ConversationHost | None = None,
) -> str | None:
    if not state.exists():
        return "state root removed"
    sessions = sessions_root(state)
    if not sessions.exists():
        return "lead-sessions removed"
    sf = state_file(state, conversation_id)
    if not sf.is_file():
        return "conversation state removed"
    parent = _watch_parent_pid()
    if parent is not None and not _pid_alive(parent):
        return "parent process gone"
    if host is not None:
        if host._turn_depth > 0:
            return None
        wake = host._wake
        if wake is not None and wake.blocks_idle_exit():
            return None
    idle_s = _idle_exit_seconds()
    if idle_s > 0:
        anchor = last_turn_at if last_turn_at > 0 else started_at
        if time.monotonic() - anchor >= idle_s:
            return "idle timeout"
    return None


def fail(msg: str) -> None:
    raise SystemExit("lead-host: " + msg)


def sessions_root(state: Path) -> Path:
    return state / "lead-sessions"


def state_file(state: Path, conversation_id: str) -> Path:
    return sessions_root(state) / ("%s.json" % conversation_id)


def socket_file(state: Path, conversation_id: str) -> Path:
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", conversation_id):
        fail("invalid conversation id")
    candidate = state.resolve() / "lead-sessions" / conversation_id / "sock" / "socket"
    if len(os.fsencode(candidate)) < 104:
        return candidate
    material = "%s\x00%s" % (state.resolve(), conversation_id)
    digest = hashlib.sha256(material.encode()).hexdigest()[:16]
    # Fixed short namespace: every sandbox denies this prefix, even before creation.
    return Path("/tmp").resolve() / ("cs-lead-%s-%s" % (os.getuid(), digest)) / "socket"


def remove_stale_socket(path: Path) -> None:
    private_socket_dir(path)
    try:
        info = path.lstat()
    except FileNotFoundError:
        return
    if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
        fail("refusing unsafe stale socket")
    path.unlink()


def load_state(path: Path) -> dict:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def save_state(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp-%s" % uuid.uuid4().hex)
    tmp.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def hosted_meta(config_root: Path) -> dict:
    path = config_root / "hosted-lead.json"  # the config root itself (installed: ~/.contextstack/config)
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        fail("cannot read %s: %s" % (path, error))


def resolve_conversation_id(config_root: Path, override: str | None = None) -> str:
    if override:
        return override
    meta = hosted_meta(config_root)
    cid = str(meta.get("conversation_id") or "").strip()
    if not cid:
        fail("hosted-lead.json has no conversation_id")
    return cid


def resolve_credential_under_config(config_root: Path, rel: str) -> Path:
    """Credential paths are relative to the config root only; contents are never read here."""
    text = str(rel or "").strip()
    # Records made before 2026-11-12 name it relative to ~/.contextstack ("config/lead-credential").
    if text.startswith("config/") and config_root.name == "config":
        text = text[len("config/"):]
    if not text or text.startswith("/") or ".." in Path(text).parts:
        fail("credential_file must be a relative path inside the config root")
    root = config_root.resolve()
    candidate = (root / text).resolve()
    try:
        candidate.relative_to(root)
    except ValueError:
        fail("credential_file escapes config root: %s" % text)
    if not candidate.is_file():
        fail("lead credential missing at %s" % candidate)
    return candidate


def credential_path(config_root: Path, conversation_id: str, state: Path) -> Path:
    migrate_legacy_lead_config(config_root, state)
    path = session_credential_path(state, conversation_id)
    if path.is_file():
        return path
    legacy = config_root / "hosted-lead.json"
    if legacy.is_file():
        meta = hosted_meta(config_root)
        if meta.get("conversation_id") == conversation_id:
            return resolve_credential_under_config(config_root, str(meta.get("credential_file") or CRED_NAME))
    fail("lead credential missing for conversation %s" % conversation_id)


def build_mcp_config(workspace: Path, cred: Path, meta: dict | None = None) -> dict:
    # Fixed host-owned command: bundle bytes never select host code or credentials.
    server = {"command": sys.executable,
              "args": [str(HERE.parents[1] / "scripts/cs"), "mcp", "--no-land", "--max-wait", "30",
                       "--credential-file", str(cred)]}
    for flag, key in (("--node", "node"), ("--director", "director")):
        if (meta or {}).get(key):
            server["args"] += [flag, str(meta[key])]
    return {"mcpServers": {"contextstack": server}}


def turn_message(workspace: Path, *, first_turn: bool) -> str:
    goal = (workspace / "tasks/turn/goal.md").read_text(encoding="utf-8")
    if not first_turn:
        return goal
    parts = []
    system = workspace / "prompts/system.md"
    context = workspace / "conversation/context.md"
    if system.is_file():
        parts.append(system.read_text(encoding="utf-8").strip())
    if context.is_file():
        parts.append(context.read_text(encoding="utf-8").strip())
    parts.append(goal.strip())
    return "\n\n".join(p for p in parts if p)


def _load_sandbox_cmd():
    global _SANDBOX_CMD_CACHE
    if _SANDBOX_CMD_CACHE is not None:
        return _SANDBOX_CMD_CACHE
    import _executor

    # sandbox_cmd reads the module's HOME; take it when the lead host first needs it, as before.
    _executor.HOME = Path.home()
    _SANDBOX_CMD_CACHE = _executor.sandbox_cmd
    return _SANDBOX_CMD_CACHE


def require_host_sandbox(sandbox: bool) -> bool:
    """Fail closed: the hosted CLI has no supported Linux confinement yet."""
    disabled = not sandbox or os.environ.get("LEAD_HOST_NO_SANDBOX") == "1"
    if disabled:
        if (os.environ.get("LEAD_HOST_NO_SANDBOX") != "1"
                or os.environ.get("CS_EXECUTOR_ALLOW_UNSANDBOXED") != "1"):
            fail("unconfined hosted lead requires both LEAD_HOST_NO_SANDBOX=1 and CS_EXECUTOR_ALLOW_UNSANDBOXED=1")
        print("WARNING: HOSTED LEAD SANDBOX DISABLED; the lead can access host files (development only)", file=sys.stderr, flush=True)
        return False
    if sys.platform != "darwin":
        fail("hosted lead sandbox unavailable on %s; refusing an unconfined turn (Linux bwrap integration is not supported)" % sys.platform)
    return True


def parse_cli_extra(raw: str) -> list[str]:
    """Only a bounded test delay may be supplied; never CLI security/config flags."""
    if not raw.strip():
        return []
    try:
        args = json.loads(raw) if raw.lstrip().startswith("[") else raw.split("\0")
        if (not isinstance(args, list) or len(args) != 2 or args[0] != "--turn-delay"
                or not isinstance(args[1], str) or not re.fullmatch(r"[0-9]+(?:\.[0-9]+)?", args[1])
                or not 0 <= float(args[1]) <= 10):
            raise ValueError()
    except (TypeError, ValueError, OverflowError):
        fail("LEAD_HOST_CLI_EXTRA permits only --turn-delay with a numeric value from 0 to 10")
    return args


def sandbox_builder(ws: Path, tmp: Path, profile: Path, read_links=(), write_roots=(), login_writes=None,
                    allow_sockets=()):
    sandbox_cmd = _load_sandbox_cmd()
    session_roots = {(roots(HERE.parent.parent, os.environ)["state"] / "lead-sessions").resolve()}
    own_roots = []
    for raw in write_roots:
        root = Path(raw).resolve()
        parent = next((p for p in root.parents if p.name == "lead-sessions"), None)
        if parent is not None:
            session_roots.add(parent)
            own_roots.append(parent / root.relative_to(parent).parts[0])
        else:
            own_roots.append(root)
    record = dict(write_roots=[str(Path(p).resolve()) for p in write_roots],
                  denied_session_roots=[str(p) for p in sorted(session_roots)],
                  own_session_roots=[str(p) for p in own_roots],
                  bridge_sockets=[os.path.abspath(str(p)) for p in allow_sockets])

    def build(binary: str, args: list[str]) -> list[str]:
        return sandbox_cmd(binary, args, ws, tmp, profile, read_links=tuple(read_links),
                           login_writes=login_writes, read_roots=tuple(write_roots), caller=dict(lead=record))
    return build


def make_session(
    workspace: Path,
    tmp: Path,
    *,
    model: str,
    resume: str | None,
    binary: str,
    mcp_config: dict,
    transcript: Path | None,
    sandbox: bool,
    cli_extra: list[str] | None = None,
    extra_env: dict[str, str] | None = None,
    runtime_home: Path | None = None,
) -> ClaudeStreamSession:
    sandbox = require_host_sandbox(sandbox)
    # One HOME per conversation, kept across turns: each turn is a new job, and claude --resume
    # finds the session under HOME/.claude (2026-11-12: a fresh HOME per turn gave error_during_execution).
    runtime_home = Path(runtime_home) if runtime_home else tmp / "runtime-home"
    prepare_runtime_home(runtime_home, stage_login=False)
    profile = tmp / "sandbox.sb"
    # File credentials are private copies; only the login Keychain needs a read exception.
    keychain_link = runtime_home / "Library/Keychains"
    links = [keychain_link] if sys.platform == "darwin" else []
    bridge = None
    if sandbox and (mcp_config or {}).get("mcpServers"):
        from _lead_mcp import HostMCPBridge
        host_env = dict(os.environ)
        host_env.update(extra_env or {})
        bridge = HostMCPBridge(mcp_config, workspace, env=host_env, secret_dir=runtime_home)
        mcp_config = bridge.config
    # The lead's own MCP bridge is the one local socket it may reach: the socket allowlist and the
    # lead-mcp-* lane deny would otherwise cut its tools off (2026-11-12: "MCP server disconnected").
    builder = sandbox_builder(workspace, tmp, profile, read_links=links, write_roots=(runtime_home,),
                              allow_sockets=(bridge.path,) if bridge else ()) if sandbox else None
    session = ClaudeStreamSession(
        workspace,
        runtime_home,
        tmp,
        model=model,
        resume=resume,
        binary=binary,
        sandbox_argv_builder=builder,
        mcp_config=mcp_config,
        tools="",
        transcript=transcript,
        cli_extra=cli_extra,
        extra_env=extra_env,
    )
    session.mcp_bridge = bridge
    return session


class ConversationHost:
  def __init__(
      self,
      conversation_id: str,
      state: Path,
      config_root: Path,
      *,
      model: str = "sonnet",
      cli_binary: str = "claude",
      sandbox: bool = True,
  ):
      self.conversation_id = conversation_id
      self.state = state
      self.config_root = config_root
      self.model = model
      self.cli_binary = cli_binary
      self.sandbox = require_host_sandbox(sandbox)
      self._session: ClaudeStreamSession | None = None
      self._workspace: Path | None = None
      self._tmp: Path | None = None
      self._first_turn = True
      self._lock = threading.Lock()
      self._last_turn_at = 0.0
      self._wake = None
      self._sock_path: Path | None = None
      self._turn_depth = 0
      self._token: str | None = None
      self._cli_extra = parse_cli_extra(os.environ.get("LEAD_HOST_CLI_EXTRA", ""))

  def _persist(self, cli_session_id: str | None) -> None:
      data = load_state(state_file(self.state, self.conversation_id))
      if cli_session_id:
          data["cli_session_id"] = cli_session_id
      data["conversation_id"] = self.conversation_id
      data["updated_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
      save_state(state_file(self.state, self.conversation_id), data)

  def wake_credential_path(self, workspace: Path | None = None) -> Path:
      return credential_path(self.config_root, self.conversation_id, self.state)

  def deliver_nudge(self, text: str) -> bool:
      if not text.strip():
          return False
      with self._lock:
          if self._turn_depth > 0:
              return False
          sess = self._session
          if not sess or not sess.alive:
              return False
          self._turn_depth += 1
          try:
              from _lead_wake import _wake_reply_seconds

              sess.send(text)
              try:
                  log = sessions_root(self.state) / ("%s-serve.log" % self.conversation_id)
                  with log.open("a", encoding="utf-8") as fh:
                      for line in redact_text(text).splitlines():
                          fh.write("NUDGE: %s\n" % line.strip())
              except OSError:
                  pass
              if self._wake:
                  self._wake.drop_watches_for_nudge(text)
              # The nudge is in the session now: report it delivered whatever happens to the reply,
              # so the coordinator never sends it twice.
              recorded = False
              try:
                  try:
                      reply = redact_text(sess.collect_turn(timeout=_wake_reply_seconds()))
                  except TimeoutError:
                      sess.stop()  # same as a turn: a late reply must not become the next turn's answer
                      raise
                  if reply.strip():
                      if self._wake:
                          recorded = self._wake.post_wake_reply(self.conversation_id, redact_text(text), reply)
                      else:
                          recorded = director_client_for_host(self).record_wake_reply(
                              self.conversation_id, redact_text(text), reply)
              except (TimeoutError, RuntimeError, OSError, ValueError) as e:
                  self._wake_log("WAKE-REPLY-FAILED: %s" % type(e).__name__)
              if not recorded:
                  self._wake_log("WAKE-REPLY-NOT-RECORDED")
              self._last_turn_at = time.monotonic()
              return True
          finally:
              self._turn_depth -= 1

  def conversation_meta(self) -> dict:
      return conversation_meta(self.config_root, self.state, self.conversation_id)

  def _wake_log(self, line: str) -> None:
      try:
          log = sessions_root(self.state) / ("%s-serve.log" % self.conversation_id)
          with log.open("a", encoding="utf-8") as fh:
              fh.write(line + "\n")
      except OSError:
          pass

  def watch_job(self, job_id: str, *, lane: str | None = None) -> None:
      if self._wake:
          self._wake.watch_job(job_id, lane=lane)

  def validate_workspace(self, workspace: Path) -> Path:
      ws = Path(workspace).absolute()
      jobs = (self.state / "jobs").resolve()
      if ws.is_symlink() or ws.resolve() != ws or ws.parent != jobs:
          fail("workspace is not a conversation job bundle")
      record = load_state(ws / "job.json")
      if record.get("job_id") != ws.name or record.get("task") != "tasks/turn":
          fail("workspace is not a turn job bundle")
      from urllib.parse import quote
      client = director_client_for_host(self)
      status, thread = client.request("GET", "conversations/%s/thread" % quote(self.conversation_id, safe=""))
      if status != 200 or not isinstance(thread, dict) or not any(
          turn.get("job_id") == ws.name for turn in thread.get("turns", []) if isinstance(turn, dict)
      ):
          fail("job is not a turn of this conversation")
      return ws

  def handle_turn(self, workspace: Path, message: str | None = None, timeout: float = 300) -> str:
      with self._lock:
          ensure_session_credential(self.conversation_id, self.config_root, self.state, self.model)
          ws = self.validate_workspace(workspace)
          if self._workspace is None:
              self._workspace = ws
              self._tmp = Path(tempfile.mkdtemp(prefix="lead-host-"))
          cred = credential_path(self.config_root, self.conversation_id, self.state)
          mcp = build_mcp_config(ws, cred, conversation_meta(self.config_root, self.state, self.conversation_id))
          text = message if message is not None else turn_message(ws, first_turn=self._first_turn)
          if self._session is None or not self._session.alive:
              if self._session is not None:
                  self._session.stop()
              saved = load_state(state_file(self.state, self.conversation_id)).get("cli_session_id")
              home = sessions_root(self.state) / self.conversation_id / "home"
              # Resume only a session this conversation's home holds (an id saved under an older,
              # per-turn HOME cannot be resumed and fails the turn).
              if saved and not any((home / ".claude").rglob("%s.jsonl" % saved)):
                  saved = None
              extra_env = {}
              if self._sock_path:
                  extra_env["LEAD_HOST_SOCKET"] = str(self._sock_path)
              self._session = make_session(
                  ws,
                  self._tmp,
                  model=self.model,
                  resume=saved,
                  binary=self.cli_binary,
                  mcp_config=mcp,
                  transcript=state_file(self.state, self.conversation_id).with_suffix(".ndjson"),
                  sandbox=self.sandbox,
                  cli_extra=self._cli_extra,
                  extra_env=extra_env,
                  runtime_home=sessions_root(self.state) / self.conversation_id / "home",
              ).start()
              if saved:
                  self._first_turn = False
          # Counted only once the session is up: a setup failure above must not leave nudges blocked forever.
          self._turn_depth += 1
          try:
              try:
                  reply = self._session.run_turn(text, timeout=timeout)
              except TimeoutError:
                  # The CLI may still answer this turn later; a reused session would hand that late reply to the
                  # next turn or wake-up. Stop it so the next turn starts (or resumes) a clean one.
                  self._session.stop()
                  raise
              self._first_turn = False
              self._last_turn_at = time.monotonic()
              if self._session.session_id:
                  self._persist(self._session.session_id)
              return reply
          finally:
              self._turn_depth -= 1
              if self._turn_depth == 0 and self._wake:
                  self._wake.force_flush()


def _serve_ready_seconds() -> float:
    raw = os.environ.get("LEAD_HOST_SERVE_READY_S", "60").strip() or "60"
    try:
        return max(5.0, float(raw))
    except ValueError:
        return 60.0


def _wait_socket_ping(sock: Path, deadline: float) -> bool:
    """Poll until the serve answers ping or deadline (tolerates CPU contention)."""
    while time.monotonic() < deadline:
        if not sock.is_socket():
            time.sleep(0.05)
            continue
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        attempt = min(10.0, max(1.0, remaining))
        try:
            response = socket_request(sock, {"op": "ping"}, timeout=attempt)
            if response.get("ok") is True:
                return True
        except (OSError, TimeoutError, json.JSONDecodeError, ValueError):
            time.sleep(0.1)
    return False


def socket_request(sock_path: Path, payload: dict, timeout: float = 300) -> dict:
    payload = dict(payload, token=read_socket_token(sock_path))
    wait_budget = max(timeout, 0.1) + 5
    deadline = time.monotonic() + wait_budget
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    with client:
        client.settimeout(max(timeout, 10) + 30)
        while time.monotonic() < deadline:
            try:
                client.connect(str(sock_path))
                break
            except (ConnectionRefusedError, FileNotFoundError):
                time.sleep(0.05)
        else:
            raise TimeoutError("lead session socket did not become ready")
        client.sendall((json.dumps(payload) + "\n").encode("utf-8"))
        chunks = []
        while True:
            part = client.recv(65536)
            if not part:
                break
            chunks.append(part)
            if b"\n" in part:
                break
        lines = b"".join(chunks).splitlines()
        if not lines:
            raise RuntimeError("lead session host closed the connection without a reply "
                               "(its traceback is in state/lead-sessions/<conversation>-serve.log)")
        return json.loads(lines[0].decode("utf-8"))


def ensure_serving(
    conversation_id: str,
    state: Path,
    config_root: Path,
    *,
    model: str,
    cli_binary: str,
    sandbox: bool,
) -> Path:
    sandbox = require_host_sandbox(sandbox)
    parse_cli_extra(os.environ.get("LEAD_HOST_CLI_EXTRA", ""))
    sock = socket_file(state, conversation_id)
    if sock.parent.exists() or sock.parent.is_symlink():
        private_socket_dir(sock)
    deadline = time.monotonic() + _serve_ready_seconds()
    state_path = state_file(state, conversation_id)
    if sock.is_socket():
        if _wait_socket_ping(sock, min(deadline, time.monotonic() + 1)):
            recorded = load_state(state_path).get("serve_sandboxed", True)
            if recorded == sandbox:
                return sock
            pid = load_state(state_path).get("serve_pid")
            if pid:
                try:
                    os.kill(int(pid), signal.SIGTERM)
                except (ProcessLookupError, ValueError, OSError):
                    pass
            stop_at = time.monotonic() + 5.0
            while sock.is_socket() and time.monotonic() < stop_at:
                time.sleep(0.05)
        # Only the new serve, holding its lifetime lock, removes a stale socket.
    sessions_root(state).mkdir(parents=True, exist_ok=True)
    log = sessions_root(state) / ("%s-serve.log" % conversation_id)
    r = roots(HERE.parent.parent, os.environ)
    env = dict(os.environ)
    env.update(
        LAB_STATE_ROOT=str(state),
        LAB_CONFIG_ROOT=str(config_root),
        LAB_CODE_ROOT=str(r["code"]),
        LEAD_HOST_QUIET="1",
    )
    # The serve outlives the turn client that spawned it: it must stay up between turns to deliver lane
    # wake-ups (_lead_wake). It exits on idle, on state removal, or when an explicitly named long-lived
    # watcher (LEAD_HOST_WATCH_PID, e.g. the node service) is gone, never when this short-lived client exits.
    env.pop("LEAD_HOST_PARENT_PID", None)
    watch = os.environ.get("LEAD_HOST_WATCH_PID", "").strip()
    if watch:
        env["LEAD_HOST_PARENT_PID"] = watch
    if os.environ.get("LEAD_HOST_IDLE_EXIT_S", "").strip():
        env["LEAD_HOST_IDLE_EXIT_S"] = os.environ["LEAD_HOST_IDLE_EXIT_S"].strip()
    argv = [
        sys.executable,
        "-I",
        str(HERE / "lead-host"),
        "serve",
        "--conversation",
        conversation_id,
        "--model",
        model,
        "--cli",
        cli_binary,
    ]
    if not sandbox:
        argv.append("--no-sandbox")
    with log.open("ab") as fh:
        subprocess.Popen(argv, stdout=fh, stderr=subprocess.STDOUT, env=env, start_new_session=True)
    if _wait_socket_ping(sock, time.monotonic() + _serve_ready_seconds()):
        return sock
    raise TimeoutError("ensure_serving timed out waiting for socket %s (LEAD_HOST_SERVE_READY_S)" % sock)


def client_turn(
    workspace: Path,
    *,
    conversation_id: str | None,
    config_root: Path,
    state: Path,
    model: str = "sonnet",
    cli_binary: str = "claude",
    sandbox: bool = True,
    timeout: float = 300,
) -> str:
    cid = resolve_conversation_id(config_root, conversation_id)
    host = ConversationHost(cid, state, config_root)
    ensure_session_credential(cid, config_root, state, model)
    host.validate_workspace(Path(workspace).resolve())
    sock = ensure_serving(cid, state, config_root, model=model, cli_binary=cli_binary, sandbox=sandbox)
    resp = socket_request(
        sock,
        {"op": "turn", "workspace": str(Path(workspace).resolve()), "timeout": timeout},
        timeout=timeout,
    )
    if resp.get("error"):
        fail(str(resp["error"]))
    return str(resp.get("reply") or "")


def _peer_uid(conn: socket.socket) -> int | None:
    if sys.platform == "darwin":
        try:
            import ctypes
            import ctypes.util

            libc = ctypes.CDLL(ctypes.util.find_library("c"))
            uid = ctypes.c_uint32()
            gid = ctypes.c_uint32()
            if libc.getpeereid(conn.fileno(), ctypes.byref(uid), ctypes.byref(gid)) == 0:
                return int(uid.value)
        except (AttributeError, OSError):
            return None
    elif hasattr(socket, "SO_PEERCRED"):
        try:
            cred = conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i"))
            return struct.unpack("3i", cred)[1]
        except OSError:
            return None
    return None


def serve_loop(
    conversation_id: str,
    config_root: Path,
    state: Path,
    *,
    model: str,
    cli_binary: str,
    sandbox: bool,
) -> int:
    sandbox = require_host_sandbox(sandbox)
    parse_cli_extra(os.environ.get("LEAD_HOST_CLI_EXTRA", ""))
    sock_path = socket_file(state, conversation_id)
    sweep_socket_dirs(sock_path.parent.parent if temporary_socket_dir(sock_path.parent) else Path("/tmp").resolve())
    private_socket_dir(sock_path, create=True)
    # Held for the lifetime of the serve: concurrent executor launches cannot replace it.
    lock_fd = os.open(sock_path.parent / "serve.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    lock_info = os.fstat(lock_fd)
    if not stat.S_ISREG(lock_info.st_mode) or lock_info.st_uid != os.getuid():
        fail("unsafe serve lock")
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(lock_fd)
        return 0
    # A sweeper may have removed the old directory between open and flock.
    # Never start against an unlinked lock (a new serve could own its replacement).
    try:
        current = (sock_path.parent / "serve.lock").stat(follow_symlinks=False)
        if (current.st_dev, current.st_ino) != (lock_info.st_dev, lock_info.st_ino):
            os.close(lock_fd)
            return 0
    except OSError:
        os.close(lock_fd)
        return 0
    host = None
    server = None

    def terminate_serve(signum, frame):
        raise SystemExit(0)

    old_term = signal.signal(signal.SIGTERM, terminate_serve)
    try:
        owner_tmp = sock_path.parent / ("owner-" + uuid.uuid4().hex)
        owner_tmp.write_text(str(os.getpid()) + "\n", encoding="ascii")
        os.replace(owner_tmp, sock_path.parent / "owner.pid")
        sessions_root(state).mkdir(parents=True, exist_ok=True)
        host = ConversationHost(conversation_id, state, config_root, model=model, cli_binary=cli_binary, sandbox=sandbox)
        host._sock_path = sock_path
        host._wake = start_wake(host, config_root)
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        remove_stale_socket(sock_path)
        host._token = secrets.token_urlsafe(32)
        token_tmp = sock_path.parent / ("token-" + uuid.uuid4().hex)
        token_fd = os.open(token_tmp, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        with os.fdopen(token_fd, "w") as stream:
            stream.write(host._token + "\n")
        os.replace(token_tmp, sock_path.parent / "token")
        server.bind(str(sock_path))
        os.chmod(sock_path, 0o600)
        server.listen(5)
        data = load_state(state_file(state, conversation_id))
        data["serve_pid"] = os.getpid()
        data["serve_sandboxed"] = sandbox
        data["conversation_id"] = conversation_id
        save_state(state_file(state, conversation_id), data)
        started_at = time.monotonic()
        log_path = sessions_root(state) / ("%s-serve.log" % conversation_id)
        while True:
            reason = serve_should_exit(state, conversation_id, last_turn_at=host._last_turn_at, started_at=started_at, host=host)
            if reason:
                if os.environ.get("LEAD_HOST_QUIET") != "1":
                    print("lead-host: exit (%s)" % reason, file=sys.stderr)
                try:
                    with log_path.open("a", encoding="utf-8") as fh:
                        fh.write("exit: %s\n" % reason)
                except OSError:
                    pass
                break
            if host._wake:
                host._wake.tick_batch()
            ready, _, _ = select.select([server], [], [], SERVE_POLL_S)
            if not ready:
                continue
            conn, _ = server.accept()
            threading.Thread(target=_serve_conn, args=(conn, host), daemon=True).start()
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        try:
            if server is not None:
                server.close()
            sock_path.unlink(missing_ok=True)
            if host is not None:
                if host._wake:
                    host._wake.stop()
                _stop_host_session(host)
        finally:
            if host is not None and host._tmp is not None:
                shutil.rmtree(host._tmp, ignore_errors=True)
            try:
                remove_socket_dir(sock_path)
            finally:
                os.close(lock_fd)
                signal.signal(signal.SIGTERM, old_term)
    return 0


def _serve_conn(conn: socket.socket, host: ConversationHost) -> None:
    peer = _peer_uid(conn)
    if peer != os.getuid():
        conn.sendall(json.dumps({"error": "forbidden peer"}).encode("utf-8") + b"\n")
        conn.close()
        return
    try:
        conn.settimeout(10)
        data = b""
        while b"\n" not in data:
            part = conn.recv(65536)
            if not part:
                break
            data += part
            if len(data) > 65536:
                raise ValueError("request too large")
        if not data:
            return
        req = json.loads(data.splitlines()[0].decode("utf-8"))
        if not isinstance(req, dict) or not isinstance(req.get("token"), str) or not host._token or not hmac.compare_digest(req["token"], host._token):
            conn.sendall(b'{"error":"unauthenticated request"}\n')
            return
        if req.get("op") == "ping":
            conn.sendall(b'{"ok":true}\n')
            return
        if req.get("op") == "watch_job":
            host.watch_job(str(req.get("job_id") or ""), lane=req.get("lane"))
            conn.sendall(b'{"ok":true}\n')
            return
        if req.get("op") != "turn":
            conn.sendall(json.dumps({"error": "unknown op"}).encode("utf-8") + b"\n")
            return
        workspace = Path(req.get("workspace") or "")
        timeout = float(req.get("timeout") or 300)
        reply = host.handle_turn(workspace, timeout=timeout)
        safe = redact_text(reply)
        conn.sendall(
            json.dumps({"reply": safe, "session_id": host._session.session_id if host._session else None}).encode("utf-8") + b"\n"
        )
    except BaseException as error:  # fail() raises SystemExit: still answer, and keep the traceback
        import traceback
        print(redact_text(traceback.format_exc()), flush=True)
        try:
            conn.sendall(json.dumps({"error": redact_text("%s: %s" % (type(error).__name__, error))}).encode("utf-8") + b"\n")
        except OSError:
            pass
    finally:
        conn.close()


def redact_text(text: str) -> str:
    return redact_secrets(text)


def is_hosted_turn_task(task: Path, spec: dict) -> bool:
    if str(spec.get("id") or "") != HOSTED_SPEC_ID:
        return False
    rel = str(task).replace("\\", "/")
    return rel.endswith("/tasks/turn") or task.name == "turn"


def authorize_hosted_turn(job: dict, task: Path, spec: dict, lab: Path) -> str:
    """Only transport-owned Director identity authorizes the hosted path."""
    if not job.get("conversation_id") or not job.get("turn_id"):
        return "hosted-lead and tasks/turn require a Director conversation turn"
    if not is_hosted_turn_task(task, spec):
        return "conversation turn requires hosted-lead tasks/turn"
    return ""


def turn_bundle(task: Path, ws: Path) -> Path:
    """The job bundle a turn reads (session/, prompts/, conversation/, tasks/turn/): <bundle>/tasks/turn.

    The executor's run workspace holds only the task setup; the bundle is two levels above the task
    (2026-11-12: every hosted turn failed with "job bundle has no session/mcp.json").
    """
    bundle = Path(task).resolve().parent.parent
    return bundle if (bundle / "session/mcp.json").is_file() else Path(ws)


def run_hosted_lead_turn(globals_dict: dict) -> None:
    """Executor hook: one conversation turn via the session host; reply becomes the run result."""
    import _run_record
    from _agent_stream import append as append_event
    from functools import partial

    ws = Path(globals_dict["ws"])  # the run workspace: the task check and evidence run here
    bundle = turn_bundle(Path(globals_dict["task"]), ws)  # what the session host reads
    run_dir = Path(globals_dict["run_dir"])
    emit = globals_dict["emit"]
    started = globals_dict["started"]
    check_window = globals_dict["check_window"]
    stim_dir = run_dir / "stimulus"
    child_env = globals_dict["child_env"]
    LAB = globals_dict["LAB"]
    run_op = globals_dict["run_op"]

    r = roots(LAB, os.environ)
    config_root = r["config"]
    state = r["state"]
    job = globals_dict.get("a") or {}
    cid = str(job.get("conversation_id") or "")
    meta = conversation_meta(config_root, state, cid)
    model = str(meta.get("model") or "claude-code-sonnet")
    cli_binary = os.environ.get("LEAD_CLI_BIN", "claude")
    sandbox = True
    template_model = "sonnet"
    if "haiku" in model:
        template_model = "haiku"
    elif "opus" in model:
        template_model = "opus"

    try:
        reply = client_turn(
            bundle,
            conversation_id=cid,
            config_root=config_root,
            state=state,
            model=template_model,
            cli_binary=cli_binary,
            sandbox=sandbox,
        )
    except (Exception, SystemExit) as error:  # client fail() raises SystemExit; the turn still gets its row
        emit("fail", "lead_host", extra={"note": redact_text(str(error))})

    safe_reply = redact_text(reply)
    append_event(run_dir / "messages.jsonl", {"event": "turn", "turn": 1, "text": safe_reply})
    (run_dir / "lead-reply.txt").write_text(safe_reply + "\n", encoding="utf-8")
    log = "reply:\n%s\n" % safe_reply
    persist_output(run_dir / "cli.log", log)
    persist_output(run_dir / "transcript.md", log)
    globals_dict["row"]["cli_invoked"] = True
    globals_dict["row"]["cli_exit"] = 0
    globals_dict["row"]["turns"] = 1

    try:
        chk = globals_dict["run_check_sandboxed"]()
    except Exception as error:
        emit("fail", "check_timeout", extra={"note": redact_text(str(error))})

    _run_record.check_json(
        run_dir,
        globals_dict["row"].get("stimulus_sha"),
        check_window,
        chk.returncode,
        (chk.stdout or "") + (chk.stderr or ""),
    )
    if chk.returncode != 0:
        emit("fail", "check", extra={"note": redact_text(chk.stderr or chk.stdout or "")[:500]})

    publish_verdict = "pass"
    _run_record.footer(run_dir, "done", publish_verdict, globals_dict["row"])
    published, publish_err = run_op(
        LAB,
        "publish-evidence",
        {
            "run_dir": str(run_dir),
            "adapter": "executor",
            "workspace": str(ws),
            "verdict": publish_verdict,
            "stop": "done",
            "check_output": redact_text((chk.stdout or "") + (chk.stderr or ""))[:8192],
            "check_exit": chk.returncode,
            "check_skipped": False,
        },
    )
    if publish_err:
        emit("fail", "publish_evidence", extra={"note": "publish-evidence: " + publish_err})
    globals_dict["row"]["evidence_sha"] = (published or {}).get("evidence_sha", "")
    globals_dict["row"]["capture_state"] = (published or {}).get("capture_state", "")
    if (published or {}).get("capture_state") != "complete":
        emit("fail", "evidence_incomplete", extra={"note": "; ".join((published or {}).get("incomplete_reasons") or [])})
    emit("pass", "done")
