"""Agent presence and sign-in probes shared by setup and node capability reports."""
import json
import os
import plistlib
import re
import shutil
import subprocess
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

# name: (executor template the gateway runs it with, or None when only reported)
AGENTS = {"claude": "claude-code", "codex": "codex", "gemini": "gemini-cli", "cursor-agent": "cursor-agent",
          "opencode": None, "pi": "pi", "goose": "goose", "grok": "grok"}


def discover_agents(path, probe):
    with ThreadPoolExecutor(len(AGENTS)) as pool:
        return list(pool.map(lambda name: probe(name, path), AGENTS))


def probe_agent(name, path, home, run, read_json, *, strict=False):
    exe = shutil.which(name, path=path)
    if exe is None and name == "grok":  # Grok Build installs beside its login, off the gateway PATH
        candidate = home / ".grok/bin/grok"
        exe = str(candidate) if candidate.is_file() else None
    out = {"name": name, "template": AGENTS[name], "path": exe, "version": None, "signed_in": None, "how": "not installed"}
    if not exe:
        return out
    env = {k: v for k, v in os.environ.items() if k not in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY")}  # as the executor runs it
    env["PATH"] = path
    v = run([exe, "--version"], 15, env=env)
    out["version"] = (v.stdout or v.stderr).strip().splitlines()[0][:60] if v.returncode == 0 and (v.stdout or v.stderr).strip() else None
    if name == "claude":
        r = run([exe, "auth", "status", "--json"], 20, env=env)
        try:
            doc = json.loads(r.stdout)
        except ValueError:
            doc = {}
        out["signed_in"] = bool(doc.get("loggedIn")) if isinstance(doc, dict) and "loggedIn" in doc else None
        out["how"] = "claude auth status: %s" % (doc.get("authMethod") or "?" if out["signed_in"] else
                                                 "not signed in (run: claude, then /login)" if out["signed_in"] is False
                                                 else "no answer")
    elif name == "codex":
        r = run([exe, "login", "status"], 20, env=env)
        text = (r.stdout + r.stderr).strip().splitlines()
        status = text[0].lower() if text else ""
        out["signed_in"] = r.returncode == 0 and "logged in" in status and "not logged in" not in status
        if strict and not out["signed_in"] and "not logged in" not in status:
            raise ValueError("could not determine sign-in status for codex")
        out["how"] = "codex login status: %s" % ((text[0][:60] if text else "?") if out["signed_in"] else "not signed in (run: codex login)")
    elif name == "gemini":
        settings = read_json(home / ".gemini/settings.json", {}) or {}
        auth = ((settings.get("security") or {}).get("auth") or {}).get("selectedType") or settings.get("selectedAuthType")
        out["signed_in"] = auth == "oauth-personal"
        out["how"] = ("settings: Google sign-in chosen (not verified without a call)" if out["signed_in"]
                      else "settings name no Google sign-in (run: gemini, then sign in)")
    elif name == "cursor-agent":
        r = run([exe, "status"], 20, env=env)
        text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", r.stdout + r.stderr).lower()
        out["signed_in"] = r.returncode == 0 and "logged in" in text and "not logged in" not in text
        if strict and not out["signed_in"] and "not logged in" not in text:
            raise ValueError("could not determine sign-in status for cursor-agent")
        out["how"] = "cursor-agent status: %s" % ("signed in" if out["signed_in"] else "not signed in (run: cursor-agent login)")
    elif name == "opencode":
        r = run([exe, "auth", "list"], 20, env=env)
        n = len([l for l in r.stdout.splitlines() if l.rstrip().endswith("stored")])
        out["signed_in"] = n > 0
        out["how"] = "opencode auth list: %d credential(s) stored" % n
    elif name in ("pi", "goose", "grok"):
        # Presence only. Login/config artifacts may sit beside provider secrets; this probe opens none of them.
        marker = {"pi": home / ".pi/agent/auth.json",
                  "goose": home / ".config/goose/config.yaml",
                  "grok": home / ".grok/auth.json"}[name]
        out["signed_in"] = marker.is_file()
        out["how"] = "login/config present" if out["signed_in"] else "no login/config at %s" % marker
    return out


def discover_node_agents():
    home = Path.home()
    path = os.environ.get("CS_AGENT_PATH")
    if path is None:
        plist = home / "Library/LaunchAgents/com.contextstack.csgateway-socket.plist"
        try:
            service = plistlib.loads(plist.read_bytes())
        except FileNotFoundError:
            service = {}
        path = (service.get("EnvironmentVariables") or {}).get("PATH")
        if not path:
            path = os.pathsep.join((str(home / ".local/bin"), os.environ.get("PATH", os.defpath)))

    def run(argv, timeout, **kwargs):
        return subprocess.run(argv, capture_output=True, text=True, timeout=timeout,
                              stdin=subprocess.DEVNULL, **kwargs)

    def read_json(path, default):
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return default

    def probe(name, path):
        agent = probe_agent(name, path, home, run, read_json, strict=True)
        if agent["template"] and agent["path"] and agent["signed_in"] is None:
            raise ValueError("could not determine sign-in status for " + name)
        return agent

    return discover_agents(path, probe)
