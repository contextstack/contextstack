"""Fixed, page-independent diagnostics for owner Chrome."""

import json
import math
import os
import shutil
import subprocess
import sys
import time
from email.utils import parsedate_to_datetime
from pathlib import Path
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

EXTENSION_ID = "haahjgaooejgdomilhdbjbeoeeebhhcl"
HOST_NAME = "com.contextstack.browser"
PAUSE = frozenset({"logged_out", "origin_not_permitted", "extension_missing_or_disabled",
                   "native_host_unregistered", "native_host_misconfigured"})
NEEDS_YOU = PAUSE | {"chrome_not_running"}

ERRORS = {
    "chrome_not_running": ("Chrome is unavailable.", "Open Chrome in your usual profile."),
    "extension_missing_or_disabled": ("The browser extension is unavailable.", "Install or enable the ContextStack extension in Chrome."),
    "native_host_unregistered": ("The browser native host is unregistered.", "Register the ContextStack native host for Chrome."),
    "native_host_misconfigured": ("The Chrome native host could not start.", "Check the native host startup error in cs doctor."),
    "origin_not_permitted": ("Chrome has not granted site access.", "Grant the scheduled site in the ContextStack extension."),
    "logged_out": ("The scheduled site requires sign-in.", "Sign in to the scheduled site in Chrome."),
    "navigation_blocked": ("Navigation left the declared site origins.", "Review the schedule's declared origins and redirects."),
    "page_timeout": ("The browser request timed out.", "Open the scheduled site in Chrome and check that it loads."),
    "reader_schema_invalid": ("The reader returned invalid structured data.", "Review the reader schema and local model configuration."),
    "action_refused": ("The browser action was refused before sending.", "Review the action and the current form."),
    "clock_skew": ("The Director claim time is ahead of this node.", "Synchronize the Director and node clocks."),
    "unknown": ("The browser request failed.", "Check the browser section of cs doctor and try again."),
}


class BrowserFailure(RuntimeError):
    def __init__(self, kind, reason=None):
        self.kind = kind if isinstance(kind, str) and kind in ERRORS else "unknown"
        message, self.fix = ERRORS[self.kind]
        super().__init__(f"{message} {reason}" if reason else message)

    def details(self):
        return {"error_kind": self.kind, "message": str(self), "fix": self.fix}


def read_object(path):
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (OSError, ValueError):
        return {}


def chrome_config(home, product):
    if sys.platform == "darwin":
        config = home / "Library/Application Support/Google/Chrome"
    else:
        config = Path(os.environ.get("XDG_CONFIG_HOME") or home / ".config") / product
    return config


def registration(home, product):
    return read_object(chrome_config(home, product) / "NativeMessagingHosts" / (HOST_NAME + ".json"))


def registered(home, product="google-chrome"):
    manifest = registration(home, product)
    if (manifest.get("name") != HOST_NAME or manifest.get("type") != "stdio"
            or manifest.get("allowed_origins") != [f"chrome-extension://{EXTENSION_ID}/"]
            or not isinstance(manifest.get("path"), str)):
        return False
    binary = Path(manifest["path"])
    settings = read_object(home / ".contextstack/browser/native-host.json")
    return (binary.is_absolute() and binary.resolve().name == "browser-host"
            and binary.is_file() and os.access(binary, os.X_OK)
            and settings.get("extension_id") == EXTENSION_ID)


START_ERRORS = {
    "settings": "native-host.json is missing or invalid",
    "caller": "the extension caller does not match the pinned ID",
    "policy": "the origin policy is missing, invalid, or outside the installed release and owner configuration",
    "lock": "another native host owns the startup lock",
    "socket": "native host sockets or private state could not be opened",
    "runtime": "the native host stopped while serving requests",
}


def startup_error(home):
    browser = home / ".contextstack/browser"
    error = read_object(browser / "log/host-start.json")
    stamp = error.get("ts")
    try:
        last_connected = (browser / "extension-status.json").stat().st_mtime
    except OSError:
        last_connected = 0
    if type(stamp) in (int, float) and stamp >= int(last_connected) and 0 <= time.time() - stamp < 120:
        return START_ERRORS.get(error.get("error"))
    return None


def checkout_paths(home):
    def in_checkout(value):
        if not isinstance(value, str) or not Path(value).is_absolute():
            return False
        try:
            parents = Path(value).resolve().parents
        except (OSError, RuntimeError):
            return False
        for parent in parents:
            try:
                if (parent / ".git").exists():
                    return True
            except OSError:
                continue
        return False

    products = ("google-chrome",) if sys.platform == "darwin" else ("google-chrome", "chromium")
    candidates = {"origin policy": [read_object(home / ".contextstack/browser/native-host.json").get("policy")],
                  "native host": [], "extension": []}
    for product in products:
        candidates["native host"].append(registration(home, product).get("path"))
        config = chrome_config(home, product)
        for pattern in ("*/Preferences", "*/Secure Preferences"):
            for path in config.glob(pattern):
                extensions = read_object(path).get("extensions")
                prefs = extensions.get("settings") if isinstance(extensions, dict) else None
                extension = prefs.get(EXTENSION_ID) if isinstance(prefs, dict) else None
                if isinstance(extension, dict):
                    candidates["extension"].append(extension.get("path"))
    return [label for label, paths in candidates.items() if any(map(in_checkout, paths))]


def director_clock_skew(home):
    url = read_object(home / ".contextstack/config/install.json").get("director")
    if not isinstance(url, str) or urlsplit(url).scheme not in ("http", "https"):
        return None
    started = time.time()
    try:
        with urlopen(Request(url, method="HEAD"), timeout=2) as response:
            date = response.headers.get("Date")
    except OSError as error:
        date = (getattr(error, "headers", None) or {}).get("Date")
    try:
        return parsedate_to_datetime(date).timestamp() - (started + time.time()) / 2
    except (TypeError, ValueError, OverflowError):
        return None


def running_product():
    # Match browser executables without matching helpers or the dedicated CDP broker.
    for product, pattern in (
        ("google-chrome", r"(Google Chrome[.]app/Contents/MacOS/Google Chrome|/chrome|google-chrome)( |$)"),
        ("chromium", r"(/chromium|chromium-browser)( |$)"),
    ):
        try:
            result = subprocess.run(["pgrep", "-u", str(os.geteuid()), "-f", pattern],
                                    capture_output=True, text=True, timeout=2)
            for pid in result.stdout.split():
                command = subprocess.run(["ps", "-p", pid, "-o", "command="],
                                         capture_output=True, text=True, timeout=2).stdout
                if command and "--type=" not in command and "--user-data-dir" not in command:
                    return product
        except (OSError, subprocess.SubprocessError):
            return None
    return None


def launch_chrome():
    if sys.platform == "darwin":
        argv = ["/usr/bin/open", "-g", "-a", "Google Chrome"]
    else:
        executable = shutil.which("google-chrome") or shutil.which("chromium")
        if executable is None:
            return False
        argv = [executable]
    try:
        subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)
    except OSError:
        return False
    return True


def startup_wait():
    try:
        seconds = float(os.environ.get("CS_BROWSER_STARTUP_WAIT_SECONDS", "30"))
        if math.isfinite(seconds) and seconds >= 0:
            return seconds
    except ValueError:
        pass
    return 30


def read_with_recovery(read, home):
    launched = False
    deadline = None
    for attempt in range(2):
        while True:
            try:
                return read()
            except (FileNotFoundError, ConnectionRefusedError, ConnectionResetError, BrokenPipeError):
                if deadline is None:
                    product = running_product()
                    if product and not registered(home, product):
                        if reason := startup_error(home):
                            raise BrowserFailure("native_host_misconfigured", reason) from None
                        raise BrowserFailure("native_host_unregistered") from None
                    launched = not product and launch_chrome()
                    if not product and not launched:
                        raise BrowserFailure("chrome_not_running") from None
                    deadline = time.monotonic() + startup_wait()
                remaining = deadline - time.monotonic()
                if remaining > 0:
                    time.sleep(min(0.25, remaining))
                    continue
                if reason := startup_error(home):
                    raise BrowserFailure("native_host_misconfigured", reason) from None
                if not launched:
                    raise BrowserFailure("extension_missing_or_disabled") from None
                if attempt == 1:
                    raise BrowserFailure("chrome_not_running") from None
                break
            except BrowserFailure as error:
                if error.kind != "page_timeout" or attempt == 1:
                    raise
                break
        time.sleep(1)


def granted(patterns, origin):
    try:
        host = urlsplit(origin).hostname
    except ValueError:
        return False
    if not host:
        return False
    for pattern in patterns:
        if pattern == "<all_urls>":
            return True
        scheme, sep, rest = pattern.partition("://")
        if not sep or scheme not in ("https", "*"):
            continue
        domain = rest.partition("/")[0]
        if domain in ("*", host) or (domain.startswith("*.") and
                                     (host == domain[2:] or host.endswith(domain[1:]))):
            return True
    return False


def doctor_findings(home, state, kit):
    from _node_schedules import rows

    registered_ok = registered(home) or (sys.platform != "darwin" and registered(home, "chromium"))
    status_path = home / ".contextstack/browser/extension-status.json"
    status = read_object(status_path)
    try:
        recent = bool(status) and 0 <= time.time() - status_path.stat().st_mtime < 90
    except OSError:
        recent = False
    patterns = status.get("origins")
    if not isinstance(patterns, list):
        patterns = []
    patterns = [item for item in patterns if isinstance(item, str)]
    declared = set()
    for row in {row["id"]: row for row in rows(state)}.values():
        spec = row.get("spec")
        if row.get("deleted") or not isinstance(spec, str) or not spec.startswith("specs/"):
            continue
        path = (kit / spec).resolve()
        if not path.is_relative_to((kit / "specs").resolve()):
            continue
        browser = read_object(path).get("browser")
        origins = browser.get("origins") if isinstance(browser, dict) else None
        if isinstance(origins, list):
            declared.update(origin for origin in origins if isinstance(origin, str))
    missing = sorted(origin for origin in declared if not granted(patterns, origin))
    grants = ", ".join(sorted(patterns)) or "none"
    origin_detail = f"granted patterns, last seen: {grants} | declared: {', '.join(sorted(declared)) or 'none'}"
    if missing:
        origin_detail += " | missing: " + ", ".join(missing)

    def finding(name, ok, detail, kind):
        return {"check": "browser " + name, "ok": ok, "detail": detail,
                "fix": "" if ok else ERRORS[kind][1], "info": ok, "warn": not ok}

    checkout = checkout_paths(home)
    extra = []
    if checkout:
        extra.append(finding("release paths", False, "Checkout paths in Chrome setup: " + ", ".join(checkout),
                             "native_host_unregistered"))
    if reason := startup_error(home):
        extra.append(finding("host startup", False, reason, "native_host_misconfigured"))
    if not declared:
        # Nothing scheduled uses the browser: report state, never a problem to fix.
        return [{"check": "browser " + name, "ok": True, "info": True, "warn": False, "fix": "",
                 "detail": detail + " (no scheduled browser tasks)"}
                for name, detail in (("native host", "registered" if registered_ok else "not registered"),
                                     ("extension", "seen recently" if recent else "not seen recently"))] + extra
    offset = director_clock_skew(home)
    if offset is not None and abs(offset) > 30:
        extra.append(finding("clock", False, f"Director time differs from this node by {offset:+.0f} seconds",
                             "clock_skew"))
    return [
        finding("native host", registered_ok, "registered" if registered_ok else "registration missing or invalid",
                "native_host_unregistered"),
        finding("extension", recent, "seen recently" if recent else "no recent connection",
                "native_host_misconfigured" if reason else "extension_missing_or_disabled"),
        finding("origins", not missing, origin_detail, "origin_not_permitted"),
    ] + extra
