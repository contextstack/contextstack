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
from functools import cache
from pathlib import Path

from _atomic_write import write_atomic

HOME = Path(os.environ["HOME"])
ROOT = HOME / ".contextstack"
CURRENT, CONFIG = ROOT / "current", ROOT / "config"
GATEWAY_LABEL, NODE_LABEL = "com.contextstack.csgateway-socket", "com.contextstack.node"
LISTEN_LABEL = "com.contextstack.node-listen"
DIRECTOR_RS_LABEL = "com.contextstack.director-rs"
CLIENT_WEB_LABEL = "com.contextstack.client-web"
BACKUP_LABEL = "com.contextstack.backup"
LARAVEL_LABELS = ("com.contextstack.director", "com.contextstack.director-schedule", "com.contextstack.director-queue")
ALL_LABELS = (GATEWAY_LABEL, NODE_LABEL, LISTEN_LABEL) + LARAVEL_LABELS + (DIRECTOR_RS_LABEL, CLIENT_WEB_LABEL, BACKUP_LABEL)
DIRECTOR_RS_DATA = ROOT / "state/director"
DIRECTOR_RS_LISTEN = "127.0.0.1:8780"
CLIENT_WEB, CLIENT_WEB_ENV = "client-web", CONFIG / "client-web/client-web.env"
# Each open event stream holds a PHP server worker for the duration of the stream.
CLIENT_WEB_WORKERS = "16"
CLIENT_WEB_STATE = ROOT / "state/client-web"
GATEWAY_BIN = {"go": "csgateway", "rust": "cs-gateway"}
RUNNER_BIN = {"go": "cs-go", "rust": "cs-harness"}


def release_bin(current, new):
    """Only the current binary name is supported."""
    current = Path(current)
    path = current / "bin" / new
    return path


LINUX_SYSTEM_PATH = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
SYSTEM_PATH = "/opt/homebrew/bin:" + LINUX_SYSTEM_PATH
MODELS = ROOT / "gateway/models.json"
ROOTS_ENV = {"LANG": "en_US.UTF-8", "PYTHONDONTWRITEBYTECODE": "1", "CS_CONFIG_ROOT": str(CONFIG),
             "CS_STATE_ROOT": str(ROOT / "state"), "CS_WORKSPACES_ROOT": str(ROOT / "workspaces")}
CREDENTIALS = {"off": "off", "file": str(ROOT / "credentials"), "keychain": "keychain"}
# Unquoted words (systemd.syntax(7)). `%` is a specifier (systemd.unit(5)), so it is never left bare.
_SAFE = re.compile(r"^[A-Za-z0-9_./:@+=-]+$")
# WorkingDirectory=, User= and Standard{Output,Error}= paths are not unquoted by their parsers.
# A space, quote or backslash would be a literal path character or a line continuation.
_SCALAR = re.compile(r"^[A-Za-z0-9_./:@+=$%~-]+$")
_SYSTEM_DIRS = ("/usr/bin", "/bin", "/usr/sbin", "/sbin")


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


def agent_path(label):
    return service_backend().path(label)


def unit_name(label):
    return label + ".service"


@cache
def linux_login_path():
    inherited = os.environ.get("PATH", os.defpath)
    shell = os.environ.get("SHELL")
    for candidate in dict.fromkeys(filter(None, (shell, "/bin/sh"))):
        try:
            result = subprocess.run([candidate, "-lc", 'printf "\\n%s\\n" "$PATH"'],
                                    stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                    errors="replace", timeout=5)
            if result.returncode == 0 and result.stdout.strip():
                return result.stdout.strip().splitlines()[-1]
        except (OSError, subprocess.TimeoutExpired):
            continue
    return inherited


def linux_user_path():
    paths = [str(HOME / ".local/bin"), str(HOME / ".cargo/bin"),
             *linux_login_path().split(":"), *os.environ.get("PATH", "").split(":"),
             *LINUX_SYSTEM_PATH.split(":")]
    return ":".join(dict.fromkeys(path for path in paths if path.startswith("/")))


def gateway_service(rec, code):
    binary = CURRENT / "bin" / GATEWAY_BIN["go"] if rec["gateway"] == "go" else release_bin(CURRENT, GATEWAY_BIN["rust"])
    env = {"CSGATEWAY_PORT": "9200", "CSGATEWAY_SOCKET": "default", "CSGATEWAY_AUTH_REQUIRED": "false",
           "CSAGENT_MODELS_CONFIG": str(MODELS), "CSGATEWAY_PROVIDERS_DIR": str(CONFIG / "gateway/providers.d"),
           "PATH": "%s:%s:%s" % (CURRENT / code / ".venv/bin", HOME / ".local/bin", SYSTEM_PATH),
           **ROOTS_ENV}
    if host_platform() == "linux":
        env["PATH"] = "%s:%s" % (CURRENT / code / ".venv/bin", linux_user_path())
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
    if host_platform() == "linux":
        env["PATH"] = "%s:%s" % (venv, linux_user_path())
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
    env = {"PATH": LINUX_SYSTEM_PATH if host_platform() == "linux" else SYSTEM_PATH, "LANG": "en_US.UTF-8"}
    return Service(DIRECTOR_RS_LABEL, args, env, str(DIRECTOR_RS_DATA), "director-rs", KeepAlive=True, RunAtLoad=True)


def client_web_service(rec):
    app = CURRENT / CLIENT_WEB
    php = rec["client_web_php"]
    args = [php, "-S", rec["serve_client_web"], "-t", str(app / "public"),
            str(app / "vendor/laravel/framework/src/Illuminate/Foundation/resources/server.php")]
    system_path = LINUX_SYSTEM_PATH if host_platform() == "linux" else SYSTEM_PATH
    env = {"PATH": "%s:%s" % (os.path.dirname(php), system_path), "LANG": "en_US.UTF-8",
           "PHP_CLI_SERVER_WORKERS": CLIENT_WEB_WORKERS, "DIRECTOR_ENV_FILE": str(CLIENT_WEB_ENV)}
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
    """A setting whose parser does not unquote (WorkingDirectory=, User=).

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
    lines.append("Type=oneshot" if svc.label == BACKUP_LABEL else "Restart=on-failure")
    lines.extend(("StandardOutput=journal", "StandardError=journal"))
    if svc.label != BACKUP_LABEL:
        lines.extend(("", "[Install]", "WantedBy=default.target"))
    lines.append("")
    return "\n".join(lines).encode()


def render_agent(svc):
    return service_backend().render(svc)


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


def launchctl(*args, timeout=None):
    """launchctl from PATH. The system one only for this account's own HOME: plists under a fake HOME (a test, a
    dry run) must never reach the real gui domain, where they would bootstrap or restart real jobs."""
    found = shutil.which("launchctl") or "launchctl"
    if os.path.realpath(found) == "/bin/launchctl" and _fake_home():
        raise Refused("HOME %s is not this account's home: refusing the real launchctl (pass --no-load, or put a "
                      "launchctl shim first on PATH)" % HOME)
    if timeout is None:
        return subprocess.run([found, *args], capture_output=True, text=True)
    try:
        return subprocess.run([found, *args], capture_output=True, text=True, timeout=timeout,
                              stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired) as error:
        return subprocess.CompletedProcess(args, 127, "", str(error))


def system_tool(name):
    found = shutil.which(name)
    if found and os.path.dirname(os.path.realpath(found)) in _SYSTEM_DIRS and _fake_home():
        raise Refused("HOME %s is not this account's home: refusing the real %s (pass --no-load, or put a "
                      "%s shim first on PATH)" % (HOME, name, name))
    return found


def systemctl(*args):
    """systemctl --user from PATH. The system one only for this account's own HOME (a fake HOME's units must
    never reach the real user instance). Always passes --user; never embeds secrets on the command line."""
    found = system_tool("systemctl")
    if not found:
        raise Refused("systemctl --user is not available on this host (Linux installs systemd user units; pass "
                      "--no-load to write them without loading)")
    try:
        return subprocess.run([found, "--user", *args], capture_output=True, text=True, timeout=90,
                              stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired) as error:
        return subprocess.CompletedProcess(args, 127, "", str(error))


class LaunchdBackend:
    missing = "no plist"

    def check(self):
        pass

    def linger_warning(self):
        return None

    def reload(self):
        pass

    def path(self, label):
        return HOME / "Library/LaunchAgents" / (label + ".plist")

    def render(self, service):
        return render_plist(service)

    def read(self, path):
        try:
            return plistlib.loads(path.read_bytes())
        except (OSError, ValueError, plistlib.InvalidFileException):
            return None

    def load(self, label, path, changed, restart):
        domain = "gui/%d" % os.getuid()
        loaded = launchctl("print", "%s/%s" % (domain, label)).returncode == 0
        if changed or not loaded:
            if loaded:
                self.unload(label)
            for _ in range(10):
                r = launchctl("bootstrap", domain, str(path))
                if r.returncode == 0:
                    return "bootstrapped"
                time.sleep(1)
            raise Refused("launchctl bootstrap %s failed: %s (the files are written; rerun to load)"
                          % (path, (r.stderr or r.stdout).strip()))
        if restart:
            self.restart(label)
            return "restarted"
        return "loaded"

    def unload(self, label, *, require_stopped=False):
        target = "gui/%d/%s" % (os.getuid(), label)
        launchctl("bootout", target)
        for _ in range(20):
            if launchctl("print", target).returncode != 0:
                return
            time.sleep(0.5)
        if require_stopped:
            raise Refused(f"could not stop {label}")

    def restart(self, label):
        launchctl("kickstart", "-k", "gui/%d/%s" % (os.getuid(), label))

    def control(self, action, label, path):
        if action == "stop":
            self.unload(label)
            return "booted out"
        launchctl("enable", "gui/%d/%s" % (os.getuid(), label), timeout=10)
        return self.load(label, path, False, action == "restart")

    def status(self, label, view, data, problems):
        r = launchctl("print", "gui/%d/%s" % (os.getuid(), label), timeout=10)
        if r.returncode == 0:
            view["loaded"] = True
            for line in r.stdout.splitlines():
                if line.startswith("\t") and not line.startswith("\t\t"):
                    key, sep, value = line.strip().partition(" = ")
                    if sep and key in ("state", "pid", "last exit code"):
                        view[{"last exit code": "last_exit"}.get(key, key)] = value.strip()
        view["pid"] = int(view["pid"]) if str(view["pid"] or "").isdigit() else None
        if label == NODE_LABEL:  # a 60 s tick can be healthy while it waits
            view["up"] = view["loaded"] and str(view["last_exit"] or "0").split(":")[0] in ("0", "(never exited)")
        else:
            view["up"] = view["state"] == "running"
        if not view["loaded"]:
            view["reason"] = "not loaded"
        elif not view["up"]:
            view["reason"] = "%s, last exit %s" % (view["state"] or "not running", view["last_exit"] or "?")
        view["reason"] = "; ".join(x for x in [view["reason"]] + problems(data) if x)
        return view

    def logs(self, labels, n, follow, file_logs):
        return file_logs()


class SystemdBackend:
    missing = "no unit"

    def path(self, label):
        return HOME / ".config/systemd/user" / unit_name(label)

    def render(self, service):
        return render_unit(service)

    def read(self, path):
        """Read the metadata from a ContextStack-generated unit."""
        try:
            lines = path.read_text(encoding="utf-8").split("\n")
        except (OSError, UnicodeError):
            return None
        data = {"EnvironmentVariables": {}}
        for line in lines:
            key, _, value = line.partition("=")
            if key == "WorkingDirectory":
                data[key] = value.replace("%%", "%")
            elif key in ("Environment", "ExecStart"):
                # Invert systemd_quote's word format, including literal specifiers.
                words = re.findall(r'"(?:[^"\\]|\\.)*"|[^\s"]+', value)
                words = [re.sub(r'\\(.)', lambda m: "\t" if m[1] == "t" else m[1], word[1:-1])
                         .replace("%%", "%") if word.startswith('"') else word for word in words]
                if key == "ExecStart" and words:
                    words[0] = words[0].removeprefix(":")
                    data["ProgramArguments"] = words
                elif key == "Environment":
                    data["EnvironmentVariables"].update(word.split("=", 1) for word in words if "=" in word)
        return data

    def check(self):
        r = systemctl("show", "--property=Version", "--value")
        if r.returncode:
            raise Refused("systemd user manager is unreachable (systemctl --user): %s. "
                          "Run from a login session with a running systemd user manager, or run loginctl enable-linger."
                          % (r.stderr or r.stdout).strip())

    def linger_warning(self):
        try:
            found = system_tool("loginctl")
            if not found:
                return None
            r = subprocess.run([found, "show-user", str(os.getuid()), "--property=Linger", "--value"],
                               capture_output=True, text=True, timeout=10, stdin=subprocess.DEVNULL)
        except (Refused, OSError, subprocess.TimeoutExpired):
            return None
        if r.returncode == 0 and r.stdout.strip() in ("no", "Linger=no"):
            return ("systemd user lingering is off. User services stop at logout. "
                    "To keep them running, the account owner can run: loginctl enable-linger")

    def reload(self):
        self.command("daemon-reload")

    def command(self, *args):
        r = systemctl(*args)
        if r.returncode:
            raise Refused("systemctl --user %s failed: %s" % (" ".join(args), (r.stderr or r.stdout).strip()))
        return r

    def load(self, label, path, changed, restart):
        unit = label + ".timer" if label == BACKUP_LABEL else unit_name(label)
        if changed and not restart:
            self.command("try-restart", unit)
        self.command("enable", "--now", unit)
        if restart:
            self.restart(label)
        return "restarted" if restart else "enabled"

    def unload(self, label, *, require_stopped=False):
        unit = label + ".timer" if label == BACKUP_LABEL else unit_name(label)
        command = self.command if require_stopped else systemctl
        command("disable", "--now", unit)
        if label == BACKUP_LABEL:
            command("stop", unit_name(label))

    def restart(self, label):
        self.command("restart", unit_name(label))

    def control(self, action, label, path):
        self.command(action, unit_name(label))
        return {"start": "started", "stop": "stopped", "restart": "restarted"}[action]

    def status(self, label, view, data, problems):
        r = systemctl("show", "--property=LoadState,ActiveState,MainPID,ExecMainStatus",
                      unit_name(label))
        fields = dict(line.split("=", 1) for line in r.stdout.splitlines() if "=" in line)
        load, active = fields.get("LoadState"), fields.get("ActiveState")
        view["loaded"] = load == "loaded"
        view["state"] = active
        pid = fields.get("MainPID", "")
        view["pid"] = int(pid) if pid.isdigit() and int(pid) > 0 else None
        view["last_exit"] = fields.get("ExecMainStatus")
        view["up"] = view["loaded"] and active == "active"
        if r.returncode and not load:
            view["reason"] = "systemctl --user show failed: %s" % (r.stderr or r.stdout).strip()
        elif not view["loaded"]:
            view["reason"] = load or "not loaded"
        elif not view["up"]:
            view["reason"] = "%s, last exit %s" % (active or "not running", view["last_exit"] or "?")
        return view

    def logs(self, labels, n, follow, file_logs):
        argv = self.journal_command(labels, n)
        if follow:
            argv.append("-f")
        try:
            with subprocess.Popen(argv, stdout=subprocess.PIPE, text=True, errors="replace",
                                  stdin=subprocess.DEVNULL) as process:
                try:
                    for line in process.stdout:
                        print(line, end="", flush=follow)
                    return process.wait()
                except KeyboardInterrupt:
                    process.terminate()
                    return 0
        except OSError as error:
            raise Refused("journalctl --user: %s" % error) from error

    def journal_command(self, labels, n):
        found = system_tool("journalctl")
        if not found:
            raise Refused("journalctl --user is not available on this host")
        argv = [found, "--user"]
        for label in labels:
            argv += ["-u", unit_name(label)]
        return argv + ["--no-pager", "-n", str(n)]


def service_backend(platform=None):
    return SystemdBackend() if (platform or host_platform()) == "linux" else LaunchdBackend()


def load_agent(label, path, changed, restart):
    return service_backend().load(label, path, changed, restart)


def unload_agent(label, *, require_stopped=False):
    return service_backend().unload(label, require_stopped=require_stopped)


def reload_units():
    service_backend().reload()


def backup_timer_path():
    return SystemdBackend().path(BACKUP_LABEL).with_suffix(".timer")


def backup_timer():
    return ("[Unit]\nDescription=Nightly ContextStack backup\n"
            "[Timer]\nOnCalendar=*-*-* 03:00:00\nPersistent=true\n"
            "[Install]\nWantedBy=timers.target\n").encode()


def install_backup_schedule(no_load):
    plan_agents([BACKUP_LABEL], [])
    linux = host_platform() == "linux"
    timer = backup_timer_path() if linux else None
    if timer and timer.exists() and timer.read_bytes() != backup_timer():
        raise Refused("refusing to replace an existing backup timer")
    backend = service_backend()
    if not no_load:
        backend.check()
        warning = backend.linger_warning()
        if warning:
            print(warning, file=sys.stderr)
    kit = CURRENT / "cs"
    svc = Service(BACKUP_LABEL, [str(kit / ".venv/bin/python3"), "-I", str(kit / "scripts/cs"), "backup", "run"],
                  {"PATH": SYSTEM_PATH, **ROOTS_ENV}, str(ROOT), "backup",
                  StartCalendarInterval={"Hour": 3, "Minute": 0})
    path = agent_path(BACKUP_LABEL)
    (ROOT / "logs").mkdir(parents=True, exist_ok=True)
    content = render_agent(svc)
    changed = not path.is_file() or path.read_bytes() != content
    if changed:
        write_atomic(path, content, mode=0o644)
    if timer and not timer.exists():
        write_atomic(timer, backup_timer(), mode=0o644)
    if not no_load:
        backend.reload()
        backend.load(BACKUP_LABEL, path, changed, False)
