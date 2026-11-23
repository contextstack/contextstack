"""Assemble a node bundle offline for the node-install regressions: scripts/node-bundle's own stage() with the
host's uv, an empty python mirror, and given binaries; plus a fake HOME whose release interpreter is seeded."""
import atexit
import importlib.machinery
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

CODE_ROOT = Path(__file__).resolve().parents[3]
# A commit hook runs scripts/check with GIT_DIR and GIT_INDEX_FILE set; those would reach node-bundle, uv and the
# installed release through every child of these checks. A node install never runs under a caller's git: drop them.
for _k in [k for k in os.environ if k.startswith("GIT_")]:
    del os.environ[_k]
# The lane can read the checkout but may be denied access to the owner's Git config.
os.environ["GIT_CONFIG_GLOBAL"] = os.devnull
BIN_NAMES = ("cs-harness", "cs-sandbox", "cs-tools", "cs-gateway")


def node_bundle():
    loader = importlib.machinery.SourceFileLoader("node_bundle", str(CODE_ROOT / "scripts/node-bundle"))
    spec = importlib.util.spec_from_loader("node_bundle", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def host_python():
    """(uv, managed interpreter dir) or (None, reason)."""
    nb = node_bundle()
    version = (CODE_ROOT / ".python-version").read_text().strip()
    uv = os.environ.get("UV") or shutil.which("uv") or str(Path.home() / ".local/bin/uv")
    if not os.access(uv, os.X_OK):
        return None, "no uv on this host"
    result = subprocess.run([uv, "python", "dir"], capture_output=True, text=True)
    pydir = Path(result.stdout.strip())
    if result.returncode or not pydir.is_absolute():
        return None, "uv python dir did not return an absolute path"
    managed = sorted(p for p in pydir.glob("cpython-%s-*" % version)
                     if nb.host_target() in p.name and os.access(p / "bin/python3", os.X_OK))
    if not managed:
        return None, "no uv-managed cpython-%s for %s under %s" % (version, nb.host_target(), pydir)
    return uv, managed[0]


def fake_bins(where):
    """Executable stand-ins that print their name: enough for layout, plists and an idle tick."""
    where.mkdir(parents=True, exist_ok=True)
    out = {}
    for name in BIN_NAMES:
        p = where / name
        p.write_text("#!/bin/sh\necho %s \"$@\"\n" % name)
        p.chmod(0o755)
        out[name] = p
    return out


def bundle(out, uv, release_id, bins, extra=None):
    """OUT/{uv/uv, python/, release/, bundle.json} as `node-bundle build` writes them, minus the downloads.
    `extra` {code-root-relative path: text} makes releases differ."""
    nb = node_bundle()
    (out / "uv").mkdir(parents=True)
    (out / "python").mkdir()
    shutil.copy2(uv, out / "uv/uv")
    files, binaries = nb.stage(out, nb.host_target(), bins)
    for rel, text in (extra or {}).items():
        (out / "release" / nb.KIT / rel).write_text(text)
        files["release/%s/%s" % (nb.KIT, rel)] = nb.sha256(out / "release" / nb.KIT / rel)
    tools = nb.manifest_tools()
    uvver = subprocess.run([uv, "--version"], capture_output=True, text=True).stdout.split()[1]
    nb.write_meta(out, {"target": nb.host_target(), "release": release_id, "python": {"version": (CODE_ROOT / ".python-version").read_text().strip()},
                        "uv": {"version": uvver}, "files": files, "binaries": binaries,
                        "tools": [{"name": t["name"], "runtime": t["runtime"], "requires": t.get("requires", [])} for t in tools]})
    return files


def seed_python(home, release_id, managed):
    """Put the host's managed interpreter where `uv python install` will look (releases/<id>/python), so it finds
    the version installed and never reaches the empty mirror. Independent copies, never hard links: uv rewrites
    BUILD, EXTERNALLY-MANAGED and _sysconfigdata in place, which through a hard link changed the host's own
    interpreter and every parallel check's release (node-install-layout red only inside scripts/check, beside
    cs-cli, 2026-11-10). On macOS `cp -c` clones (copy-on-write, instant); elsewhere a plain copy."""
    dst = home / ".contextstack/releases" / release_id / "python" / managed.name
    dst.parent.mkdir(parents=True, exist_ok=True)
    if sys.platform == "darwin" and subprocess.run(["cp", "-cRp", str(managed), str(dst)]).returncode == 0:
        return
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(managed, dst, symlinks=True)


SHIM_DIR = Path(tempfile.mkdtemp(prefix="node-install-launchctl-shim-"))  # removed at exit
(SHIM_DIR / "launchctl").write_text('#!/bin/sh\n'
                                    '[ -z "$LAUNCHCTL_SHIM_LOG" ] || echo "launchctl $*" >> "$LAUNCHCTL_SHIM_LOG"\n'
                                    '[ "$1" = print ] && exit 113\nexit 0\n')
(SHIM_DIR / "launchctl").chmod(0o755)
atexit.register(shutil.rmtree, SHIM_DIR, True)


def dead_network_env(home, **more):
    """HOME replaced; every proxy variable a dead port (with UV_OFFLINE=1 inside node-install, nothing downloads);
    a launchctl shim first on PATH unless the caller brings its own PATH: a test never reaches the real launchd
    (an install without --no-load once bootstrapped a fake HOME's plist into the real gui domain)."""
    dead = "http://127.0.0.1:9"
    env = dict(os.environ, HOME=str(home), HTTP_PROXY=dead, HTTPS_PROXY=dead, http_proxy=dead, https_proxy=dead,
               ALL_PROXY=dead, PATH="%s:%s" % (SHIM_DIR, os.environ.get("PATH", "/usr/bin:/bin")))
    env.update(more)
    return env


def directory_snapshot(path):
    """Observe directory changes even when the lane can stat a path but cannot list it."""
    try:
        return sorted(path.iterdir())
    except FileNotFoundError:
        return None
    except PermissionError:
        try:
            return path.stat().st_mtime_ns
        except (FileNotFoundError, PermissionError):
            return None


def real_agents():
    """{label: pid} for every com.contextstack.* job in the real gui domain (read-only /bin/launchctl list)."""
    if sys.platform != "darwin":
        return {}
    try:
        out = subprocess.run(["/bin/launchctl", "list"], capture_output=True, text=True).stdout
    except PermissionError:
        # Lanes cannot reach the owner's launchd. Installer calls still use the fixture shim.
        return {}
    return {parts[2]: parts[0] for parts in (l.split("\t") for l in out.splitlines()[1:])
            if len(parts) == 3 and parts[2].startswith("com.contextstack.")}


def periodic(label):
    """A real agent launchd starts on a timer (StartInterval / StartCalendarInterval, e.g. the installed node's tick):
    its pid changes on its own, so a new pid is not a test touching it. Unreadable plist: not periodic (strict)."""
    import plistlib
    try:
        with open(os.path.expanduser("~/Library/LaunchAgents/%s.plist" % label), "rb") as f:
            plist = plistlib.load(f)
    except (OSError, ValueError, plistlib.InvalidFileException):
        return False
    return "StartInterval" in plist or "StartCalendarInterval" in plist


def launchd_untouched(before):
    """'' when the real gui domain gained no com.contextstack.* label and no running one changed pid, else why."""
    after = real_agents()
    gained = sorted(set(after) - set(before))
    restarted = sorted(l for l in set(after) & set(before)
                       if before[l] != "-" and after[l] != before[l] and not periodic(l))
    if gained or restarted:
        return "the real launchd domain changed during the test: gained %s, restarted %s" % (gained, restarted)
    return ""


def report(stdout):
    try:
        return json.loads(stdout)
    except ValueError:
        return {"unparsed": stdout[-1500:]}
