"""Layout helpers for node-release: record, links, config seed, models, tokens, drain.

Split out of scripts/node-release so that file and _node_services stay near 500 lines. Stdlib only.
"""
import base64
from contextlib import closing, contextmanager
import fcntl
import json
import sqlite3
import os
import re
import shutil
import stat
import subprocess
import sys
import time
from pathlib import Path

from _atomic_write import write_atomic as _write_atomic_bytes  # noqa: E402

from _node_services import (CLIENT_WEB, CLIENT_WEB_ENV, CLIENT_WEB_STATE, CONFIG, CURRENT, DIRECTOR_RS_DATA,
                            DIRECTOR_RS_LISTEN, GATEWAY_BIN, HOME, MODELS, Refused, ROOT, host_platform,
                            linux_login_path, release_bin)

RELEASES, RECORD = ROOT / "releases", CONFIG / "install.json"
DIRECTOR_ENV = CONFIG / "director/director.env"
CLIENT_WEB_DIRS = ("app/private", "app/public", "framework/cache/data", "framework/sessions", "framework/views", "logs")
CLIENT_WEB_PHP = 80300
KEPT_STATE = ("director", "client-web")
LARAVEL_KEYS = ("serve_director", "php", "director_key_from", "director_url")
LINKS = {"csgateway": None, "node-install": "scripts/node-install", "cs": "scripts/cs"}
RETIRED_LINKS = ("u" + "ah",)
SEEDED = ("policy/spool.json", "policy/env.json")
OVERLAY_PROVIDERS = ()


def configured_overlay_providers():
    """Provider names whose deployments ship in the catalog but require a host overlay (config/gateway/overlay-providers.json)."""
    cfg = CONFIG / "gateway/overlay-providers.json"
    if not cfg.is_file():
        return OVERLAY_PROVIDERS
    try:
        doc = json.loads(cfg.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return OVERLAY_PROVIDERS
    items = doc.get("providers") if isinstance(doc, dict) else doc
    if not isinstance(items, list):
        return OVERLAY_PROVIDERS
    return tuple(p for p in items if isinstance(p, str) and p)

DEFAULTS = {"runner": "rust", "gateway": "rust", "token_env": "CONTEXTSTACK_NODE_TOKEN", "keep": 3, "skip_links": []}
INSTALLATION = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
TOKEN = re.compile(r"[A-Za-z0-9._~+/=-]{8,512}")
DRAIN_DEFAULT_WAIT_S, DRAIN_POLL_S = 1800, 2
TERMINAL_STATES = {"done", "error", "operator_close", "cancelled"}


class DrainRefused(Exception):
    """The command refused with --no-wait while jobs were active (exit 2)."""

    def __init__(self, reason):
        super().__init__("active jobs")
        self.reason = reason


INSTALL_LOCK = RELEASES / "install.lock"
INSTALL_LOCK_FD = "NODE_INSTALL_LOCK_FD"
RESTART_INTAKE_FD = "CS_HOSTED_RESTART_INTAKE_FD"


@contextmanager
def install_lock(command):
    """Hold the same lock as the shell bootstrap, including for direct node-release calls."""
    inherited = os.environ.pop(INSTALL_LOCK_FD, None)
    if inherited is not None:
        fd = int(inherited)
        if os.path.samestat(os.fstat(fd), INSTALL_LOCK.stat()):
            with os.fdopen(fd, "r+") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                os.set_inheritable(fd, False)
                yield lock
                return
    ROOT.mkdir(mode=0o700, parents=True, exist_ok=True)
    RELEASES.mkdir(mode=0o700, exist_ok=True)
    with INSTALL_LOCK.open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            lock.seek(0)
            holder = lock.read().strip()
            raise Refused("install lock held by " + (holder or "another node-install invocation")) from None
        lock.seek(0)
        lock.truncate()
        lock.write(f"pid {os.getpid()} ({command})\n")
        lock.flush()
        yield lock


def exec_locked(script, args):
    try:
        with install_lock("node-install " + args[0]) as lock:
            fd = lock.fileno()
            os.set_inheritable(fd, True)
            os.environ[INSTALL_LOCK_FD] = str(fd)
            os.execv("/bin/sh", ["/bin/sh", script, *args])
    except (OSError, Refused) as error:
        sys.exit(f"node-install: {error}")


def load_record():
    return json.loads(RECORD.read_text(encoding="utf-8")) if RECORD.is_file() else {}


def write_atomic(path, data, mode=0o644):
    """Write bytes when they differ; True when the file changed."""
    if path.is_file() and path.read_bytes() == data:
        return False
    _write_atomic_bytes(path, data, mode=mode)
    return True


def complete(rid):
    return bool(rid) and "/" not in rid and (RELEASES / rid / ".complete").is_file()


def kit(rid):
    """The release code root."""
    return "cs"


def current_id():
    return os.readlink(CURRENT).rsplit("/", 1)[-1] if CURRENT.is_symlink() else None


def user_path():
    probe = os.environ.get("NODE_INSTALL_USER_PATH")
    if probe is not None:
        return probe
    if host_platform() == "linux":
        return linux_login_path()
    try:
        out = subprocess.run([os.environ.get("SHELL") or "/bin/zsh", "-lic", 'printf "\\n%s" "$PATH"'],
                             stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=15).stdout
        return out.strip().splitlines()[-1] if out.strip() else os.environ.get("PATH", "")
    except (OSError, subprocess.TimeoutExpired):
        return os.environ.get("PATH", "")


def pick_link_dir(given, recorded):
    if given or recorded:
        return Path(given or recorded)
    return HOME / ".local/bin"


PATH_MARKER = "# ContextStack PATH"


def shell_profiles():
    return (HOME / ".zprofile", HOME / ".bash_profile", HOME / ".config/fish/config.fish")


def setup_path(link_dir, yes=False):
    if link_dir != HOME / ".local/bin" or str(link_dir) in user_path().split(":"):
        return
    linux = host_platform() == "linux"
    shell = Path(os.environ.get("SHELL", "/bin/sh" if linux else "/bin/zsh")).name
    profiles = dict(zip(("zsh", "bash", "fish"), shell_profiles()))
    profile = profiles.get(shell, HOME / ".profile" if linux else profiles["zsh"])
    line = ('fish_add_path --prepend "$HOME/.local/bin"' if shell == "fish"
            else 'export PATH="$HOME/.local/bin:$PATH"') + "  " + PATH_MARKER
    try:
        old = profile.read_text() if profile.exists() else ""
        if line not in old.splitlines():
            answer = "n"
            if not yes and sys.stdin.isatty():
                print("Add ~/.local/bin to your PATH in ~/%s? [Y/n] " % profile.relative_to(HOME),
                      end="", file=sys.stderr, flush=True)
                answer = sys.stdin.readline().strip().lower()
            if answer in ("", "y", "yes"):
                profile.parent.mkdir(parents=True, exist_ok=True)
                with profile.open("a") as stream:
                    stream.write(("\n" if old and not old.endswith("\n") else "") + line + "\n")
            else:
                print("Add this line to ~/%s:\n%s" % (profile.relative_to(HOME), line), file=sys.stderr)
    except OSError as error:
        print("Could not update %s: %s\nAdd this line:\n%s" % (profile, error, line), file=sys.stderr)
    print('Open a new terminal (or run: `export PATH="$HOME/.local/bin:$PATH"`) to use `cs`', file=sys.stderr)


def remove_path():
    for profile in shell_profiles():
        if profile.is_file():
            old = profile.read_text()
            text = "".join(line for line in old.splitlines(keepends=True)
                           if not line.rstrip().endswith("  " + PATH_MARKER))
            if text != old:
                profile.write_text(text)


def ours(path):
    """True for a symlink we made (into ROOT/current or ROOT/releases)."""
    return path.is_symlink() and os.readlink(path).startswith(str(ROOT) + "/")


def plan_links(link_dir, skip, gateway, runner="rust", code="cs", release=None):
    """Plan current command links, refusing paths owned by another application."""
    links, foreign = {}, []
    selected = {"csgateway": "bin/" + GATEWAY_BIN[gateway]}
    for name, rel in LINKS.items():
        if name in skip:
            continue
        path, target = link_dir / name, str(CURRENT / (code + "/" + rel if rel else selected[name]))
        if os.path.lexists(path) and not ours(path):
            foreign.append(str(path))
        links[name] = (path, target)
    if foreign:
        raise Refused("not ours, left untouched: %s (remove it or pass --skip-link NAME)" % ", ".join(foreign))
    return links


def director_rs_listen(addrs):
    """The Rust Director's listen addresses: loopback or Tailscale only (it serves plain HTTP; director-rs refuses
    other peers itself, and nothing here asks it to listen where they could reach it)."""
    return private_listen(addrs, "--director-rs-listen", "127.0.0.1:8780", "the Director serves plain HTTP")


def private_listen(addrs, flag, example, why):
    """addrs when each is IP:PORT on loopback or a Tailscale address (100.64.0.0/10, fd7a:115c:a1e0::/48); else Refused."""
    import ipaddress
    tailnets = [ipaddress.ip_network(n) for n in ("100.64.0.0/10", "fd7a:115c:a1e0::/48")]
    for addr in addrs:
        host, sep, port = addr.rpartition(":")
        try:
            ip = ipaddress.ip_address(host[1:-1] if host.startswith("[") and host.endswith("]") else host)
        except ValueError:
            ip = None
        if not (sep and port.isdigit() and 0 < int(port) < 65536 and ip is not None
                and (ip.is_loopback or any(ip in n for n in tailnets))):
            raise Refused("%s %r: want IP:PORT on loopback (%s) or a Tailscale address (100.64.0.0/10, "
                          "[TAILSCALE_IPV6]:PORT); %s" % (flag, addr, example, why))
    return list(addrs)


def client_web_php(given):
    """The absolute php client-web runs on: --php, else the login-shell PATH's; Refused unless it is >= 8.3."""
    php = given or shutil.which("php", path=user_path())
    if not php or not os.path.isabs(php) or not os.access(php, os.X_OK):
        raise Refused("--serve-client-web needs php >= 8.3: %s (pass --php /absolute/path/to/php)"
                      % ("%s is not an executable absolute path" % php if php else "none on the login-shell PATH"))
    try:
        got = subprocess.run([php, "-r", "echo PHP_VERSION_ID;"], capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        got = ""
    if not got.isdigit() or int(got) < CLIENT_WEB_PHP:
        raise Refused("--serve-client-web needs php >= 8.3; %s says PHP_VERSION_ID %r (pass --php PATH)" % (php, got))
    return php


def director_url(rec):
    """The Rust Director client-web talks to: the one served here (its first listen address), else the Director this
    node is registered with (--director), else the default."""
    if rec.get("serve_director_rs"):
        return "http://" + (rec.get("director_rs_listen") or [DIRECTOR_RS_LISTEN])[0]
    return (rec.get("director") or "http://" + DIRECTOR_RS_LISTEN).rstrip("/")


def seed_client_web(rid, rec):
    """state/client-web and its storage dirs (0700, created when missing, never re-moded) and config/client-web's env
    file, seeded once from the release's template with a fresh APP_KEY (written straight into it, never printed).
    True when the env file was written."""
    for rel in ("",) + CLIENT_WEB_DIRS:
        d = CLIENT_WEB_STATE / rel if rel else CLIENT_WEB_STATE
        if not d.exists():
            d.mkdir(mode=0o700, parents=True)
            d.chmod(0o700)
    if not CLIENT_WEB_ENV.parent.exists():
        CLIENT_WEB_ENV.parent.mkdir(mode=0o700)
        CLIENT_WEB_ENV.parent.chmod(0o700)
    if os.path.lexists(CLIENT_WEB_ENV):
        return False
    text = (RELEASES / rid / CLIENT_WEB / ".env.install.example").read_text(encoding="utf-8")
    text = text.replace("__ROOT__", str(ROOT)).replace("__URL__", "http://" + rec["serve_client_web"])
    text = text.replace("__DIRECTOR_URL__", director_url(rec))
    key = "APP_KEY=base64:" + base64.b64encode(os.urandom(32)).decode()
    text = re.sub(r"(?m)^APP_KEY=.*$", lambda _: key, text, count=1)
    if key not in text:
        raise Refused("%s has no APP_KEY= line" % (RELEASES / rid / CLIENT_WEB / ".env.install.example"))
    fd = os.open(CLIENT_WEB_ENV, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(text)
    return True


def read_node_token(source):
    """(token, installation, node) from --director-token-from: the JSON line `director-rs node register` printed, or
    the bare token. Its content never reaches a message."""
    try:
        text = (sys.stdin.read() if source == "-" else Path(source).read_text(encoding="utf-8")).strip()
    except (OSError, UnicodeDecodeError) as error:
        raise Refused("--director-token-from %s: %s" % (source, getattr(error, "strerror", None) or "unreadable"))
    installation = node = None
    token = text
    if text.startswith("{"):
        try:
            doc = json.loads(text)
        except ValueError:
            doc = None
        doc = doc if isinstance(doc, dict) else {}
        token, installation, node = doc.get("token"), doc.get("installation"), doc.get("node")
    if not isinstance(token, str) or not TOKEN.fullmatch(token):
        raise Refused("--director-token-from %s: no token in it (want the JSON line `director-rs node register NAME` "
                      "printed, or the bare token)" % source)
    return token, installation, node


def write_node_token(token_env, token):
    """config/node.env (0600, created that way): `export <token_env>=<token>` replaces that name's line, the rest kept.
    True when it changed."""
    path = CONFIG / "node.env"
    old = path.read_text(encoding="utf-8") if path.is_file() else ""
    line = re.compile(r"\s*(?:export\s+)?%s=" % re.escape(token_env))
    text = "".join(l + "\n" for l in old.splitlines() if not line.match(l)) + "export %s=%s\n" % (token_env, token)
    if text == old:
        return False
    tmp = path.with_name(".node.env.tmp.%d" % os.getpid())
    if os.path.lexists(tmp):
        tmp.unlink()
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(text)
    os.replace(tmp, path)
    return True


def check_config(rec):
    """Every config refusal, before the first write."""
    estate = CONFIG / "ESTATE"
    have = estate.read_text(encoding="utf-8").strip() if estate.is_file() else None
    if rec.get("estate") and rec.get("node") and rec["estate"] != rec["node"]:
        raise Refused("--estate %r and --node %r differ: a node's ESTATE is its director node name"
                      % (rec["estate"], rec["node"]))
    want = rec.get("estate") or rec.get("node")
    if want and have and have != want:
        raise Refused("config/ESTATE is %r, the install says %r: an estate keeps its name (edit ESTATE by hand to "
                      "rename)" % (have, want))
    if rec.get("installation") and not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", rec["installation"]):
        raise Refused("--installation %r is not an installation ID (registration prints it)" % rec["installation"])
    if rec.get("director") and not rec.get("node") and not have:
        raise Refused("--director needs --node NAME (the node's ESTATE and its director node name)")


def ensure_private_root(path):
    path.mkdir(mode=0o700, exist_ok=True)
    if stat.S_IMODE(path.stat().st_mode) != 0o700:
        path.chmod(0o700)


def seed_config(rid, rec):
    """Seed config and ensure the install, state and workspace roots are private."""
    estate, want = CONFIG / "ESTATE", rec.get("estate") or rec.get("node")
    for path in (ROOT, ROOT / "state", ROOT / "workspaces"):
        ensure_private_root(path)
    for d in ("config", "config/policy", "config/gateway", "logs", "gateway"):
        if not (ROOT / d).exists():
            (ROOT / d).mkdir(mode=0o700)
            (ROOT / d).chmod(0o700)
    providers, legacy = CONFIG / "gateway/providers.d", ROOT / "gateway/providers.d"
    if not providers.exists():
        if legacy.is_dir():
            shutil.copytree(legacy, providers, symlinks=True)
        else:
            providers.mkdir()
        providers.chmod(0o700)
    if rec.get("credentials") == "file" and not os.path.lexists(ROOT / "credentials"):
        (ROOT / "credentials").mkdir(mode=0o700)
        (ROOT / "credentials").chmod(0o700)
    for rel in SEEDED:
        if not (CONFIG / rel).exists():
            shutil.copy2(RELEASES / rid / kit(rid) / rel, CONFIG / rel)
    template = RELEASES / rid / kit(rid) / "policy/land-host-probes.json"
    if template.is_file():  # Older releases can still be activated.
        probe_root = Path(os.environ.get("CS_STATE_ROOT") or ROOT)
        if not probe_root.is_absolute():
            raise Refused("CS_STATE_ROOT must be an absolute path")
        destination = probe_root / "config/land-host-probes.json"
        destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        try:
            fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError:
            ops = str(Path(__file__).resolve().parents[1] / "ops/bin")
            if ops not in sys.path:
                sys.path.insert(0, ops)
            from _land_host_probes import policy_drift, policy_repair

            try:
                drift = policy_drift(destination, template)
            except ValueError as error:
                drift = str(error)
            if drift:
                print(f"land host probes: {drift}. {policy_repair(template)}.", file=sys.stderr)
        else:
            with os.fdopen(fd, "wb") as output:
                output.write(template.read_bytes())
    have = estate.read_text(encoding="utf-8").strip() if estate.is_file() else None
    if want and not have:
        write_atomic(estate, (want + "\n").encode())
    if rec.get("director"):
        spool = json.loads((CONFIG / "policy/spool.json").read_text(encoding="utf-8"))
        old = spool.get("receiver") if isinstance(spool.get("receiver"), dict) else {}
        receiver = {"kind": "director", "url": rec["director"], "node": rec.get("node") or have, "token_env": rec["token_env"]}
        installation = rec.get("installation") or old.get("installation")
        if installation:
            receiver["installation"] = installation
        spool.update({"receiver": receiver, "retention": "prune_acked"})
        write_atomic(CONFIG / "policy/spool.json", (json.dumps(spool, indent=2) + "\n").encode())


def overlay_providers():
    """Provider names this host's overlay (config/gateway/providers.d/*.json) defines."""
    names = set()
    for f in sorted((CONFIG / "gateway/providers.d").glob("*.json")):
        try:
            doc = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        for p in (doc.get("providers") if isinstance(doc, dict) else None) or []:
            if isinstance(p, dict) and isinstance(p.get("name"), str):
                names.add(p["name"])
    return names


def derive_models(rid):
    """ROOT/gateway/models.json: the release's catalog, its absolute cli_executor replaced by this install's, and
    the deployments of OVERLAY_PROVIDERS this host's overlay does not define dropped. True when it changed."""
    models = json.loads((RELEASES / rid / "gateway/models.json").read_text(encoding="utf-8"))
    if "cli_executor" in models:
        models["cli_executor"] = str(CURRENT / kit(rid) / "ops/bin/_executor.py")
    absent = set(configured_overlay_providers()) - overlay_providers()
    if isinstance(models.get("deployments"), list):
        models["deployments"] = [d for d in models["deployments"] if not (isinstance(d, dict) and d.get("provider") in absent)]
    return write_atomic(MODELS, (json.dumps(models, indent=2) + "\n").encode())



DIRECTOR_DB = DIRECTOR_RS_DATA / "director.sqlite"
DIRECTOR_DB_SNAP_PREFIX = "director.sqlite.pre-"
# Beside the Director's data, not in it: the data directory is what an uninstall --purge keeps.
DIRECTOR_DB_SNAPS = DIRECTOR_RS_DATA.parent / "director-snapshots"
DIRECTOR_DB_BACKUPS = DIRECTOR_RS_DATA.parent / "director-backups"


def director_db_snapshot_path(rid):
    return DIRECTOR_DB_SNAPS / (DIRECTOR_DB_SNAP_PREFIX + rid)


def prune_director_db_snapshots(keep=2):
    for directory, prefix in ((DIRECTOR_DB_SNAPS, DIRECTOR_DB_SNAP_PREFIX),
                              (DIRECTOR_DB_BACKUPS, "backup-")):
        copies = sorted(directory.glob(prefix + "*"), key=lambda path: path.stat().st_mtime, reverse=True)
        for stale in copies[keep:]:
            stale.unlink(missing_ok=True)


def director_db_schema(path=DIRECTOR_DB):
    try:
        # A normal read-only SQLite connection can leave WAL sidecars behind
        # even when the stopped Director has already checkpointed its database.
        journals = [path.with_name(path.name + suffix) for suffix in ("-wal", "-journal")]
        if not any(journal.exists() for journal in journals):
            with closing(sqlite3.connect(path.as_uri() + "?mode=ro&immutable=1", uri=True)) as db:
                version = db.execute("PRAGMA user_version").fetchone()[0]
            if not any(journal.exists() for journal in journals):
                return version
        with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as db:
            return db.execute("PRAGMA user_version").fetchone()[0]
    except sqlite3.Error as error:
        raise Refused(f"cannot read Director database schema from {path}: {error}") from error


def director_supported_schema(rid):
    binary = release_bin(RELEASES / rid, "cs-director")
    try:
        result = subprocess.run([binary, "--schema-version"], check=True, capture_output=True, text=True, timeout=10)
        version = result.stdout.strip()
        if version.isascii() and version.isdecimal():
            return int(version)
    except (OSError, ValueError, subprocess.SubprocessError):
        pass
    print(f"warning: Director schema support is unknown for release {rid} ({binary} --schema-version)",
          file=sys.stderr)
    return None


def check_director_schema(rid):
    if DIRECTOR_DB.is_file():
        live = director_db_schema()
        supported = director_supported_schema(rid)
        if supported is not None and live > supported:
            raise Refused(f"release {rid} supports Director schema {supported}, but the live database is schema {live}")


def snapshot_director_db(rid, *, checked=False):
    """Consistent copy of director.sqlite before a release that will restart the Director."""
    if not DIRECTOR_DB.is_file():
        return None
    dest = director_db_snapshot_path(rid)
    DIRECTOR_DB_SNAPS.mkdir(mode=0o700, parents=True, exist_ok=True)
    with closing(sqlite3.connect(DIRECTOR_DB)) as src, closing(sqlite3.connect(dest)) as dst:
        src.backup(dst)
        if checked:
            if dst.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                raise Refused("pre-upgrade database copy failed the integrity check")
            if dst.execute("PRAGMA foreign_key_check").fetchone() is not None:
                raise Refused("pre-upgrade database copy failed the foreign-key check")
            DIRECTOR_DB_BACKUPS.mkdir(mode=0o700, parents=True, exist_ok=True)
            backup = DIRECTOR_DB_BACKUPS / ("backup-" + dest.name)
            with closing(sqlite3.connect(backup)) as saved:
                dst.backup(saved)
            backup.chmod(0o600)
    os.chmod(dest, stat.S_IRUSR | stat.S_IWUSR)
    prune_director_db_snapshots()
    return dest


def restore_director_db(snapshot):
    # Restore through SQLite so pages in the live WAL cannot overlay the restored database.
    with closing(sqlite3.connect(snapshot.as_uri() + "?mode=ro", uri=True)) as src:
        with closing(sqlite3.connect(DIRECTOR_DB)) as dst:
            src.backup(dst)
    os.chmod(DIRECTOR_DB, stat.S_IRUSR | stat.S_IWUSR)


def switch_current(rid):
    tmp = ROOT / (".current.tmp.%d" % os.getpid())
    if os.path.lexists(tmp):
        tmp.unlink()
    os.symlink("releases/" + rid, tmp)
    os.replace(tmp, CURRENT)


def pid_live(pid):
    if not pid:
        return False
    try:
        os.kill(int(pid), 0)
        return True
    except PermissionError:
        return True
    except (OSError, ValueError, TypeError):
        return False


def active_jobs():
    """The node's local active jobs (queued, or running under a live pid) from state/dispatch.jsonl."""
    shelf = ROOT / "state/dispatch.jsonl"
    if not shelf.is_file():
        return []
    by_id = {}
    for line in shelf.read_bytes().splitlines():
        try:
            row = json.loads(line)
        except (ValueError, UnicodeError):
            continue
        if not isinstance(row, dict):
            continue
        jid = str(row.get("id") or row.get("job") or "")
        if jid:
            by_id[jid] = row
    out = []
    for jid, row in sorted(by_id.items()):
        state = str(row.get("state") or "")
        if state in TERMINAL_STATES:
            continue
        if state == "pending":
            out.append({"id": jid, "job_id": str(row.get("director_job") or jid), "state": "queued"})
            continue
        if state == "running":
            try:
                pid = int(row.get("pid") or 0)
            except (TypeError, ValueError):
                continue
            if pid > 0 and pid_live(pid):
                out.append({"id": jid, "job_id": str(row.get("director_job") or jid), "state": "running", "pid": pid})
    return out


def format_jobs(jobs):
    return ", ".join("%s (%s%s)" % (j["job_id"], j["state"], ", pid=%s" % j["pid"] if j.get("pid") else "")
                     for j in jobs)


def drain_before_restart(load, opts):
    """Before a supervisor restart, wait for active jobs unless forced/refused."""
    if not load:
        return None
    if opts.get("force") and opts.get("no_wait"):
        raise Refused("--force and --no-wait conflict: choose one")
    if opts.get("force"):
        return {"forced": True, "active": [j["job_id"] for j in active_jobs()]}
    wait_s = opts.get("wait")
    wait_s = DRAIN_DEFAULT_WAIT_S if wait_s is None else int(wait_s)
    if wait_s < 0:
        raise Refused("--wait must be >= 0 seconds")
    jobs = active_jobs()
    if not jobs:
        return {"waited_s": 0, "active": []}
    if opts.get("no_wait"):
        raise DrainRefused({
            "code": "active_jobs",
            "detail": "active jobs present; refusing with --no-wait",
            "active": jobs,
            "hint": "wait for jobs to finish, pass --wait SECONDS, or use --force",
        })
    deadline = time.monotonic() + wait_s
    waited = 0
    while jobs:
        remain = max(0, int(deadline - time.monotonic()))
        print("node-install: waiting for active jobs (%ds left): %s" % (remain, format_jobs(jobs)),
              file=sys.stderr, flush=True)
        if remain <= 0:
            raise Refused("active jobs did not drain within %ss: %s (wait longer with --wait SECONDS, or override with "
                          "--force)" % (wait_s, format_jobs(jobs)))
        time.sleep(min(DRAIN_POLL_S, remain))
        waited = wait_s - max(0, int(deadline - time.monotonic()))
        jobs = active_jobs()
    return {"waited_s": waited, "active": []}


def restart_lock(lock, opts):
    try:
        flags = fcntl.LOCK_EX | (fcntl.LOCK_NB if opts.get("bounded") else 0)
        fcntl.flock(lock, flags)
    except BlockingIOError:
        raise DrainRefused({"code": "busy", "detail": "restart lock is busy", "active": active_jobs()})


@contextmanager
def restart_window(load, opts):
    if not load:
        yield None
        return
    state = ROOT / "state"
    ensure_private_root(state)
    # Stop job pulls while letting already queued jobs run and drain.
    with (state / "restart-intake.lock").open("a") as intake:
        inherited = os.environ.get(RESTART_INTAKE_FD) if opts.get("bounded") else None
        if inherited is None:
            restart_lock(intake, opts)
        elif not os.path.samestat(os.fstat(int(inherited)), os.fstat(intake.fileno())):
            raise Refused("invalid hosted restart intake lock")
        drain = drain_before_restart(load, opts)
        # Tick holds this lock while recording admission. Keep it through the
        # service restart and check the shelf again once admission is frozen.
        with (state / "dispatch.lock").open("a") as admission:
            restart_lock(admission, opts)
            jobs = [] if opts.get("force") else active_jobs()
            if jobs:
                if opts.get("no_wait"):
                    raise DrainRefused({"code": "active_jobs", "detail": "jobs arrived before restart", "active": jobs})
                raise Refused("jobs arrived during drain; retry the release switch")
            yield drain


def migrate_noindex(parent, name):
    """Make parent/<name>.noindex the real directory and parent/<name> a relative symlink to it.

    Spotlight does not index a directory whose name ends in .noindex. A real directory is renamed onto
    that name, so its files stay. A symlink is never followed: a foreign link, a file, or two real
    directories already side by side are left untouched (None).
    """
    parent = Path(parent)
    link, dest = parent / name, parent / (name + ".noindex")
    if not parent.is_dir() or parent.is_symlink():
        return None
    if link.is_symlink():
        if os.readlink(link) != name + ".noindex":
            return None
        if not dest.exists():
            dest.mkdir()
            dest.chmod(0o700)
        elif dest.is_symlink() or not dest.is_dir():
            return None
        return dest
    if link.exists() and not link.is_dir():
        return None
    if dest.is_symlink():
        return None
    if link.is_dir():
        if dest.exists():
            return None
        link.rename(dest)
        os.symlink(name + ".noindex", link)
        return dest
    if dest.is_dir():
        if not link.exists():
            os.symlink(name + ".noindex", link)
        return dest
    if link.exists() or dest.exists():
        return None
    dest.mkdir()
    dest.chmod(0o700)
    os.symlink(name + ".noindex", link)
    return dest


def migrate_churn_dirs():
    """Point bundles and state/runs at *.noindex directories. workspaces/ and releases/ stay real
    directories: current -> releases/<id>, CS_WORKSPACES_ROOT, and uninstall's rmtree all name them,
    and Python's rmtree refuses to remove a directory symlink."""
    # state/runs stays a real directory: agent profiles resolve run paths (CODEX_HOME's realpath
    # needs metadata on every literal parent), and a symlinked runs dir broke that once (2026-11-12).
    return migrate_noindex(ROOT, "bundles")


def _kept_release_ids():
    """Release ids still on disk (a complete releases/<id>), plus current. Symlinks are not followed."""
    ids = set()
    current = current_id()
    if current:
        ids.add(current)
    if not RELEASES.is_dir():
        return ids
    for entry in os.scandir(RELEASES):
        if entry.is_symlink() or not entry.is_dir(follow_symlinks=False):
            continue
        marker = Path(entry.path) / ".complete"
        try:
            st = marker.lstat()
        except OSError:
            continue
        if stat.S_ISREG(st.st_mode):
            ids.add(entry.name)
    return ids


def _bundle_release_id(path):
    meta = path / "bundle.json"
    try:
        st = meta.lstat()
        if not stat.S_ISREG(st.st_mode):
            return None
        doc = json.loads(meta.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, ValueError):
        return None
    rid = doc.get("release") if isinstance(doc, dict) else None
    return rid if isinstance(rid, str) and rid else None


def prune_bundles(installed=None, directory=None):
    """Delete bundle directories inside `directory` beyond the newest `keep` (default 3).

    Never removes the bundle just installed, one a kept release came from, a non-bundle, or anything
    reached through a symlink. Only a direct child of the bundles directory is eligible. Returns the
    removed names.
    """
    if directory is None:
        directory = migrate_noindex(ROOT, "bundles")
    if directory is None or directory.is_symlink() or not directory.is_dir():
        return []
    real_dir = os.path.realpath(directory)
    if os.path.dirname(real_dir) != os.path.realpath(ROOT) or Path(real_dir).name not in ("bundles", "bundles.noindex"):
        return []
    if os.path.islink(real_dir):
        return []
    try:
        installed_real = os.path.realpath(installed) if installed else None
    except OSError:
        installed_real = None
    keep = max(1, int(load_record().get("keep") or DEFAULTS["keep"]))
    kept_ids = _kept_release_ids()
    kids = []
    for entry in os.scandir(real_dir):
        if entry.is_symlink() or not entry.is_dir(follow_symlinks=False):
            continue
        path = Path(entry.path)
        if os.path.islink(path):
            continue
        real = os.path.realpath(path)
        if os.path.dirname(real) != real_dir or os.path.basename(real) != entry.name:
            continue
        meta = path / "bundle.json"
        try:
            st = meta.lstat()
        except OSError:
            continue
        if not stat.S_ISREG(st.st_mode):
            continue
        kids.append(path)
    kids.sort(key=lambda item: (item.lstat().st_mtime_ns, item.name), reverse=True)
    newest = set(kids[:keep])
    removed = []
    for path in kids:
        if path in newest:
            continue
        if _bundle_release_id(path) in kept_ids:
            continue
        if installed_real and os.path.realpath(path) == installed_real:
            continue
        shutil.rmtree(path)
        removed.append(path.name)
    return sorted(removed)

