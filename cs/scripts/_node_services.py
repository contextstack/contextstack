"""Shared node service definitions: launchd plists and systemd user units render from this.

One Service (command, env, cwd, logs) per agent node-install writes: gateway, node, node-listen,
director-rs, client-web. macOS writes ~/Library/LaunchAgents/<label>.plist; Linux writes
~/.config/systemd/user/<label>.service (Restart=on-failure, WantedBy=default.target). NODE_INSTALL_PLATFORM
overrides the host (tests). Stdlib only.
"""
import os
import plistlib
import pwd
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

HOME = Path(os.environ["HOME"])
ROOT = HOME / ".contextstack"
CURRENT, CONFIG = ROOT / "current", ROOT / "config"
GATEWAY_LABEL, NODE_LABEL = "com.contextstack.csgateway-socket", "com.contextstack.node"
LISTEN_LABEL = "com.contextstack.node-listen"
DIRECTOR_RS_LABEL = "com.contextstack.director-rs"
CLIENT_WEB_LABEL = "com.contextstack.client-web"
LARAVEL_LABELS = ("com.contextstack.director", "com.contextstack.director-schedule", "com.contextstack.director-queue")
ALL_LABELS = (GATEWAY_LABEL, NODE_LABEL, LISTEN_LABEL) + LARAVEL_LABELS + (DIRECTOR_RS_LABEL, CLIENT_WEB_LABEL)
DIRECTOR_RS_DATA = ROOT / "state/director"
DIRECTOR_RS_LISTEN = "127.0.0.1:8780"
CLIENT_WEB, CLIENT_WEB_ENV = "client-web", CONFIG / "client-web/client-web.env"
CLIENT_WEB_STATE = ROOT / "state/client-web"
GATEWAY_BIN = {"go": "csgateway", "rust": "cs-gateway"}
RUNNER_BIN = {"go": "cs-go", "rust": "cs-harness"}


def release_bin(current, new):
    """Only the current binary name is supported."""
    current = Path(current)
    path = current / "bin" / new
    return path


SYSTEM_PATH = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
MODELS = ROOT / "gateway/models.json"
ROOTS_ENV = {"LANG": "en_US.UTF-8", "PYTHONDONTWRITEBYTECODE": "1", "LAB_CONFIG_ROOT": str(CONFIG),
             "LAB_STATE_ROOT": str(ROOT / "state"), "LAB_WORKSPACES_ROOT": str(ROOT / "workspaces")}
CREDENTIALS = {"off": "off", "file": str(ROOT / "credentials"), "keychain": "keychain"}
# Unquoted words (systemd.syntax(7)). `%` is a specifier (systemd.unit(5)), so it is never left bare.
_SAFE = re.compile(r"^[A-Za-z0-9_./:@+=-]+$")
# WorkingDirectory=, User= and Standard{Output,Error}= paths are not unquoted by their parsers.
# A space, quote or backslash would be a literal path character or a line continuation.
_SCALAR = re.compile(r"^[A-Za-z0-9_./:@+=$%~-]+$")
_SYSTEM_SYSTEMCTL = ("/usr/bin/systemctl", "/bin/systemctl", "/usr/sbin/systemctl", "/sbin/systemctl")
LINGER_NOTE = ("systemd user units stop at logout unless linger is on: loginctl enable-linger "
               "(the installer does not run it)")


class Refused(Exception):
    """Nothing was changed; the message says why."""


class Service:
    """One agent: command, env, cwd, logs. launchd extras (KeepAlive, StartInterval) stay out of systemd."""

    __slots__ = ("label", "args", "env", "cwd", "logs", "user", "launchd")

    def __init__(self, label, args, env, cwd, logs, user=None, **launchd):
        self.label, self.args, self.env, self.cwd, self.logs = label, list(args), dict(env), cwd, logs
        self.user = user
        self.launchd = launchd


def host_platform():
    """macos or linux. NODE_INSTALL_PLATFORM overrides (tests); else sys.platform."""
    override = os.environ.get("NODE_INSTALL_PLATFORM")
    if override:
        return {"darwin": "macos"}.get(override, override)
    return {"darwin": "macos", "linux": "linux"}.get(sys.platform, sys.platform)


def agents_dir():
    if host_platform() == "linux":
        return HOME / ".config/systemd/user"
    return HOME / "Library/LaunchAgents"


def agent_suffix():
    return ".service" if host_platform() == "linux" else ".plist"


def agent_path(label):
    return agents_dir() / (label + agent_suffix())


def unit_name(label):
    return label + ".service"


def gateway_service(rec, code):
    binary = CURRENT / "bin" / GATEWAY_BIN["go"] if rec["gateway"] == "go" else release_bin(CURRENT, GATEWAY_BIN["rust"])
    env = {"CSGATEWAY_PORT": "9200", "CSGATEWAY_SOCKET": "default", "CSGATEWAY_AUTH_REQUIRED": "false",
           "CSAGENT_MODELS_CONFIG": str(MODELS), "CSGATEWAY_PROVIDERS_DIR": str(CONFIG / "gateway/providers.d"),
           "PATH": "%s:%s:%s" % (CURRENT / code / ".venv/bin", HOME / ".local/bin", SYSTEM_PATH),
           **ROOTS_ENV}
    env["CSGATEWAY_CREDENTIALS"] = CREDENTIALS[rec["credentials"]]
    return Service(GATEWAY_LABEL, [str(binary)], env, str(ROOT / "gateway"), "csgateway",
                   KeepAlive=True, RunAtLoad=True)


def node_args_env(rec, code, op):
    """Tick and node-listen: same public env; node.env is read in-process, op is ops/bin/<op>."""
    venv = CURRENT / code / ".venv/bin"
    runner = CURRENT / "bin" / RUNNER_BIN["go"] if rec["runner"] == "go" else release_bin(CURRENT, RUNNER_BIN["rust"])
    env = {"PATH": "%s:%s" % (venv, SYSTEM_PATH), **ROOTS_ENV,
           "CS_RUNNER_BIN": str(runner),
           "CSGATEWAY_ENDPOINT": "http://127.0.0.1:9200/v1/agent/model.call"}
    args = [str(venv / "python3"), str(CURRENT / code / "ops/bin" / op)]
    return args, env


def node_service(rec, code):
    args, env = node_args_env(rec, code, "tick")
    return Service(NODE_LABEL, args, env, str(CURRENT / code), "node", StartInterval=60, RunAtLoad=True)


def listen_service(rec, code):
    args, env = node_args_env(rec, code, "node-listen")
    return Service(LISTEN_LABEL, args, env, str(CURRENT / code), "node-listen",
                   KeepAlive={"SuccessfulExit": False}, RunAtLoad=True, ThrottleInterval=10)


def director_rs_service(rec):
    args = [str(release_bin(CURRENT, "cs-director")), "serve", "--data-dir", str(DIRECTOR_RS_DATA)]
    for addr in rec["director_rs_listen"]:
        args += ["--listen", addr]
    args += ["--gateway-owner-token", str(CONFIG / "gateway/owner-token")]
    env = {"PATH": SYSTEM_PATH, "LANG": "en_US.UTF-8"}
    return Service(DIRECTOR_RS_LABEL, args, env, str(DIRECTOR_RS_DATA), "director-rs", KeepAlive=True, RunAtLoad=True)


def client_web_service(rec):
    app = CURRENT / CLIENT_WEB
    php = rec["client_web_php"]
    args = [php, "-S", rec["serve_client_web"], "-t", str(app / "public"),
            str(app / "vendor/laravel/framework/src/Illuminate/Foundation/resources/server.php")]
    env = {"PATH": "%s:%s" % (os.path.dirname(php), SYSTEM_PATH), "LANG": "en_US.UTF-8", "PHP_CLI_SERVER_WORKERS": "4",
           "DIRECTOR_ENV_FILE": str(CLIENT_WEB_ENV)}
    return Service(CLIENT_WEB_LABEL, args, env, str(app / "public"), "client-web", KeepAlive=True, RunAtLoad=True)


def agent_specs(rec, code, node=False, serve_rs=False, serve_cw=False):
    """The services this activation writes, in load order (gateway, then node pair, then director, client-web)."""
    out = [gateway_service(rec, code)]
    if node:
        out.extend((node_service(rec, code), listen_service(rec, code)))
    if serve_rs:
        out.append(director_rs_service(rec))
    if serve_cw:
        out.append(client_web_service(rec))
    return out


def render_plist(svc):
    d = {"Label": svc.label, "ProgramArguments": svc.args, "WorkingDirectory": svc.cwd,
         "EnvironmentVariables": svc.env,
         "StandardOutPath": str(ROOT / "logs" / (svc.logs + ".out.log")),
         "StandardErrorPath": str(ROOT / "logs" / (svc.logs + ".err.log")), **svc.launchd}
    return plistlib.dumps(d, sort_keys=False)


def systemd_escape(value, field="unit value"):
    """Keep unit values literal and refuse injected directives."""
    value = str(value)
    if "\n" in value or "\r" in value or "\0" in value:
        raise Refused("systemd %s must not contain newlines or NUL" % field)
    return value.replace("%", "%%")


def systemd_quote(value, field="unit value"):
    """Quote a systemd word. A safe token stays bare; `%` is always quoted as `%%`."""
    value = systemd_escape(value, field)
    if _SAFE.fullmatch(value):
        return value
    return '"%s"' % value.replace("\\", "\\\\").replace('"', '\\"').replace("\t", "\\t")


def systemd_scalar(value, field):
    """A setting whose parser does not unquote (WorkingDirectory=, User=, append: paths).

    Specifiers still apply, so a literal `%` is written `%%`. Anything the line reader or the
    setting would treat specially is refused, not written raw.
    """
    raw = str(value)
    if "\n" in raw or "\r" in raw or "\0" in raw:
        raise Refused("systemd %s must not contain newlines or NUL" % field)
    if not _SCALAR.fullmatch(raw):
        raise Refused("systemd %s must be one unquoted token (no spaces, quotes or backslashes): %r"
                      % (field, raw))
    if field == "WorkingDirectory" and not raw.startswith("/"):
        raise Refused("systemd WorkingDirectory must be an absolute path, not %r" % raw)
    if field == "User" and not re.fullmatch(r"[A-Za-z_][A-Za-z0-9._%-]*|[0-9]+", raw):
        raise Refused("systemd User must be a user name, not %r" % raw)
    if field in ("StandardOutput", "StandardError") and not raw.startswith("/"):
        raise Refused("systemd %s path must be absolute, not %r" % (field, raw))
    return raw.replace("%", "%%")


def systemd_text(value, field):
    """Free text on its own line. `%` is doubled; a trailing backslash would continue the next line."""
    value = systemd_escape(value, field)
    if value.endswith("\\"):
        raise Refused("systemd %s must not end with a backslash (it continues the next line): %r" % (field, value))
    return value


def render_unit(svc):
    """A systemd user unit: same command, env and cwd as the plist; Restart=on-failure, WantedBy=default.target."""
    lines = ["[Unit]", "Description=ContextStack %s" % systemd_text(svc.logs, "Description"), "", "[Service]"]
    if svc.user:
        lines.append("User=%s" % systemd_scalar(svc.user, "User"))
    lines.append("WorkingDirectory=%s" % systemd_scalar(svc.cwd, "WorkingDirectory"))
    for key, value in svc.env.items():
        raw = "%s=%s" % (key, value)
        lines.append("Environment=%s" % systemd_quote(raw, "Environment %s" % key))
    # ':' disables ExecStart variable expansion (systemd.service(5)). Unlike $$,
    # this also preserves dollars in the executable path, not just in argv.
    # Quoting keeps the prefix inside the first word: systemd strips it after unquoting.
    args = [":" + str(svc.args[0]), *svc.args[1:]]
    lines.append("ExecStart=" + " ".join(systemd_quote(a, "ExecStart") for a in args))
    lines.append("Restart=on-failure")
    lines.append("StandardOutput=append:%s" % systemd_scalar(
        ROOT / "logs" / (svc.logs + ".out.log"), "StandardOutput"))
    lines.append("StandardError=append:%s" % systemd_scalar(
        ROOT / "logs" / (svc.logs + ".err.log"), "StandardError"))
    lines.extend(("", "[Install]", "WantedBy=default.target", ""))
    return "\n".join(lines).encode()


def render_agent(svc):
    return render_unit(svc) if host_platform() == "linux" else render_plist(svc)


def our_agent(path):
    """True for an agent this installer wrote: it runs from ROOT/current."""
    if not path.is_file():
        return False
    marker = str(CURRENT) + "/"
    if path.suffix == ".service":
        try:
            text = path.read_text(encoding="utf-8")
            command_marker = systemd_quote(marker, "unit path")
            if command_marker.startswith('"'):
                command_marker = command_marker[1:-1]
            return systemd_escape(marker, "unit path") in text or command_marker in text
        except Refused as error:
            raise Refused("cannot tell whether %s is our unit: %s" % (path, error)) from error
        except (OSError, UnicodeDecodeError):
            return False
    try:
        args = plistlib.loads(path.read_bytes()).get("ProgramArguments") or []
    except (OSError, ValueError, plistlib.InvalidFileException):
        return False
    return any(str(a).startswith(marker) for a in args)


def plan_agents(labels, adopt):
    """Refused when an agent we would write is someone else's and not adopted; returns the adopted ones."""
    foreign = [l for l in labels if agent_path(l).exists() and not our_agent(agent_path(l))]
    refused = [l for l in foreign if l not in adopt]
    if refused:
        raise Refused("plist not ours, left untouched: %s (it does not run from %s; pass --adopt LABEL to replace "
                      "it, the original is kept in config/replaced-agents/)"
                      % (", ".join(str(agent_path(l)) for l in refused), CURRENT))
    return foreign


def _account_home():
    return os.path.realpath(pwd.getpwuid(os.getuid()).pw_dir)


def _fake_home():
    return os.path.realpath(str(HOME)) != _account_home()


def launchctl(*args):
    """launchctl from PATH. The system one only for this account's own HOME: plists under a fake HOME (a test, a
    dry run) must never reach the real gui domain, where they would bootstrap or restart real jobs."""
    found = shutil.which("launchctl") or "launchctl"
    if os.path.realpath(found) == "/bin/launchctl" and _fake_home():
        raise Refused("HOME %s is not this account's home: refusing the real launchctl (pass --no-load, or put a "
                      "launchctl shim first on PATH)" % HOME)
    return subprocess.run([found, *args], capture_output=True, text=True)


def systemctl(*args):
    """systemctl --user from PATH. The system one only for this account's own HOME (a fake HOME's units must
    never reach the real user instance). Always passes --user; never embeds secrets on the command line."""
    found = shutil.which("systemctl")
    if not found:
        raise Refused("systemctl --user is not available on this host (Linux installs systemd user units; pass "
                      "--no-load to write them without loading)")
    if os.path.realpath(found) in _SYSTEM_SYSTEMCTL and _fake_home():
        raise Refused("HOME %s is not this account's home: refusing the real systemctl (pass --no-load, or put a "
                      "systemctl shim first on PATH)" % HOME)
    return subprocess.run([found, "--user", *args], capture_output=True, text=True)


def need_systemctl():
    """Refuse before load when systemctl --user cannot be used. Does not run loginctl."""
    found = shutil.which("systemctl")
    if not found:
        raise Refused("systemctl --user is not available on this host (Linux installs systemd user units; pass "
                      "--no-load to write them without loading)")
    if os.path.realpath(found) in _SYSTEM_SYSTEMCTL and _fake_home():
        raise Refused("HOME %s is not this account's home: refusing the real systemctl (pass --no-load, or put a "
                      "systemctl shim first on PATH)" % HOME)


def load_agent(label, path, changed, restart):
    if host_platform() == "linux":
        r = systemctl("enable", "--now", unit_name(label))
        if r.returncode != 0:
            raise Refused("systemctl --user enable --now %s failed: %s (the files are written; rerun to load)"
                          % (unit_name(label), (r.stderr or r.stdout).strip()))
        if restart:
            systemctl("restart", unit_name(label))
            return "restarted"
        return "enabled"
    domain = "gui/%d" % os.getuid()
    loaded = launchctl("print", "%s/%s" % (domain, label)).returncode == 0
    if changed or not loaded:
        if loaded:
            unload_agent(label)
        for _ in range(10):
            r = launchctl("bootstrap", domain, str(path))
            if r.returncode == 0:
                return "bootstrapped"
            time.sleep(1)
        raise Refused("launchctl bootstrap %s failed: %s (the files are written; rerun to load)"
                      % (path, (r.stderr or r.stdout).strip()))
    if restart:
        launchctl("kickstart", "-k", "%s/%s" % (domain, label))
        return "restarted"
    return "loaded"


def unload_agent(label):
    if host_platform() == "linux":
        systemctl("disable", "--now", unit_name(label))
        return
    target = "gui/%d/%s" % (os.getuid(), label)
    launchctl("bootout", target)
    for _ in range(20):
        if launchctl("print", target).returncode != 0:
            return
        time.sleep(0.5)


def reload_units():
    """systemctl --user daemon-reload after writing or removing units."""
    r = systemctl("daemon-reload")
    if r.returncode != 0:
        raise Refused("systemctl --user daemon-reload failed: %s" % (r.stderr or r.stdout).strip())
