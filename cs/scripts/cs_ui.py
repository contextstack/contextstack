"""cs ui: open a one-time Director UI pairing URL (ADR 0022).

Runs the installed `director-rs ui-link`. The URL is a five-minute, one-time browser
login; browser handoff uses a private, short-lived HTML file. Stdlib only; loaded
by scripts/cs.
"""
import json
import os
import shutil
import sys
import tempfile
import subprocess
from pathlib import Path

DEFAULT_LISTEN = "127.0.0.1:8780"


def _flag(args, name):
    return [args[i + 1] for i, a in enumerate(args[:-1]) if a == name]


def data_dir(cs):
    """The Director's data dir: the plist's --data-dir, else the install's state/director."""
    _, data = cs.load_plist(cs.DIRECTOR)
    args = [str(a) for a in (data or {}).get("ProgramArguments") or []]
    return Path((_flag(args, "--data-dir") or [str(cs.ROOT / "state/director")])[0])


def _loopback(addr):
    host = addr.rsplit(":", 1)[0]
    host = host[1:-1] if host.startswith("[") and host.endswith("]") else host
    return host in ("127.0.0.1", "::1") or host.startswith("127.")


def listen_addr(cs):
    """The Director's loopback listen address: install.json, else the plist, else the default."""
    record = cs.read_json(cs.CONFIG / "install.json", {}) or {}
    addrs = record.get("director_rs_listen") or []
    if isinstance(addrs, str):
        addrs = [addrs]
    if not addrs:
        _, data = cs.load_plist(cs.DIRECTOR)
        args = [str(a) for a in (data or {}).get("ProgramArguments") or []]
        addrs = _flag(args, "--listen")
    chosen = next((a for a in addrs if _loopback(a)), addrs[0] if addrs else DEFAULT_LISTEN)
    return chosen


def ui_link_argv(cs):
    binary = cs.release_bin(cs.CURRENT, "cs-director")
    if not os.access(binary, os.X_OK):
        raise cs.Stop("cs ui: the Director is not installed on this machine (no %s); "
                      "run `cs update`, then `cs init`" % binary)
    argv = [str(binary), "ui-link", "--data-dir", str(data_dir(cs))]
    listen = listen_addr(cs)
    if listen != DEFAULT_LISTEN:
        argv += ["--listen", listen]
    return argv


def issue_url(cs):
    """Run director-rs ui-link; return the URL. Never writes stdout to disk."""
    argv = ui_link_argv(cs)
    env = os.environ.copy()
    env.pop("DIRECTOR_LISTEN", None)
    env.pop("DIRECTOR_DATA_DIR", None)
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL, env=env)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise cs.Stop("cs ui: director-rs ui-link failed: %s" % error)
    url = next((line.strip() for line in (result.stdout or "").splitlines() if line.strip().startswith("http")), "")
    if result.returncode != 0 or not url:
        detail = (result.stderr or result.stdout or "director-rs ui-link failed").strip()
        raise cs.Stop("cs ui: %s" % detail)
    return url


def open_browser(url, platform):
    """Hand the opener a private file; the pairing code never enters process argv."""
    opener = "open" if platform == "macos" else "xdg-open"
    directory = Path(tempfile.mkdtemp(prefix="cs-ui-"))
    page = directory / "pair.html"
    try:
        # Escape HTML-significant characters even though this is a JavaScript string.
        target = json.dumps(url).replace("<", "\\u003c").replace(">", "\\u003e").replace("&", "\\u0026")
        with open(page, "x", opener=lambda path, flags: os.open(path, flags, 0o600)) as stream:
            stream.write("<!doctype html><meta charset=utf-8><script>location.replace(" + target + ");</script>")
        result = subprocess.run([opener, str(page)], capture_output=True, text=True, timeout=15, stdin=subprocess.DEVNULL)
        if result.returncode != 0:
            shutil.rmtree(directory)
            return False
        # Leave time for the browser to read the file, then remove the secret.
        subprocess.Popen([sys.executable, "-c", "import shutil,sys,time; time.sleep(60); shutil.rmtree(sys.argv[1], ignore_errors=True)",
                          str(directory)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)
    except (OSError, subprocess.TimeoutExpired):
        shutil.rmtree(directory, ignore_errors=True)
        return False
    return True


def print_url(url, say=print, *, explain=True):
    """Print a pairing link only to the requested output, never a log file."""
    say(url)
    if explain:
        say("This one-time URL expires in five minutes and works once.")


def run(args, cs):
    cs.need_install()
    url = issue_url(cs)
    if args.print_url:
        print_url(url, explain=False)
        return 0
    if open_browser(url, cs.PLATFORM):
        return 0
    print("Could not open a browser.")
    print_url(url)
    return 0
