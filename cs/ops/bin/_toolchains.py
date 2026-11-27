"""Node-owned, read-only toolchain directories, resolved once at job start."""
from contextlib import contextmanager
from functools import cache
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


TOOLCHAIN_BINARIES = ("php", "node", "npm", "composer", "cargo", "go", "python3", "uv", "rustc")
LOGIN_LOOKUP = ";".join("command -v %s || :" % name for name in TOOLCHAIN_BINARIES)
LOGIN_TIMEOUT = 5


def login_toolchain_additions(path, service_path):
    """Read the current entries and plan additions from the owner's login shell."""
    try:
        existing = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        existing = []
    if not isinstance(existing, list):
        raise ValueError("toolchains.json must be a list")

    shell = os.environ.get("SHELL")
    if (os.environ.get("CS_PLATFORM") or sys.platform) == "linux":
        if not shell or not shutil.which(shell):
            shell = "/bin/sh"
    if not shell:
        raise ValueError("login shell lookup failed (SHELL is unset)")
    try:
        result = subprocess.run([shell, "-lc", LOGIN_LOOKUP], stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL,
                                text=True, errors="replace", timeout=LOGIN_TIMEOUT, start_new_session=True)
    except subprocess.TimeoutExpired:
        raise ValueError("login shell lookup failed (timed out)") from None
    except OSError:
        raise ValueError("login shell lookup failed (could not start shell)") from None
    if result.returncode != 0:
        raise ValueError("login shell lookup failed (exit %s)" % result.returncode)

    found = {}
    for line in result.stdout.splitlines():
        binary = Path(line)
        if (binary.is_absolute() and binary.name in TOOLCHAIN_BINARIES
                and binary.is_file() and os.access(binary, os.X_OK)):
            found.setdefault(binary.name, binary)

    configured = {entry["name"] for entry in existing
                  if isinstance(entry, dict) and isinstance(entry.get("name"), str)}
    added = []
    for name, binary in found.items():
        if name in configured:
            continue
        service_binary = shutil.which(name, path=service_path)
        if name != "uv" and service_binary and Path(service_binary).resolve() == binary.resolve():
            continue
        bindir = binary.parent.resolve()
        if not _grantable(bindir) or not _grantable(binary.resolve().parent):
            continue
        entry = {"name": name, "bin": str(bindir)}
        if name == "uv":
            python = uv_python_dir(binary)
            if python is None or not _grantable(python):
                continue
            entry["python"] = str(python)
        elif name == "cargo":
            paths = cargo_toolchain_paths(binary, rustc=found.get("rustc"))
            if not paths or not all(path.is_dir() and _grantable(path) for path in paths.values()):
                continue
            entry.update({key: str(value) for key, value in paths.items()})
        added.append(entry)
    return existing, added


def config_file():
    # Match repos.json's install-config location, including a custom repos file.
    return Path(os.environ.get("CS_TOOLCHAINS_FILE") or
                Path(os.environ.get("CS_REPOS_FILE") or
                     Path.home() / ".contextstack/config/repos.json").with_name("toolchains.json"))


def binary_roots(binary, seen=None):
    """Follow symlinks and static shell launcher paths without executing the launcher."""
    seen = set() if seen is None else seen
    named = Path(binary).absolute()
    target = named.resolve()
    if target in seen:
        return []
    seen.add(target)
    roots = [named.parent, target.parent]
    try:
        with target.open("rb") as stream:
            raw = stream.read(65536)
    except OSError:
        return roots
    if not raw.startswith(b"#!") or b"\0" in raw:
        return roots
    text = raw.decode("utf-8", errors="replace")
    variables = {"HOME": str(Path.home()), "DIR": str(target.parent),
                 "SCRIPT_DIR": str(target.parent)}
    def expand(value):
        return re.sub(r'\$\{?(\w+)\}?', lambda m: variables.get(m[1], m[0]), value)
    for key, value in re.findall(r'''(?m)^\s*(\w+)=["']([^"'\n]+)["']''', text):
        variables[key] = expand(value)
    # Quoted paths preserve spaces in Herd's Application Support install.
    paths = re.findall(r'''["']([^"'\n]+)["']''', text)
    paths.extend(re.findall(r'(?<![\w/])(/[A-Za-z0-9_./+-]+)', text))
    for value in paths:
        value = expand(value)
        if value.startswith("~/"):
            value = str(Path.home() / value[2:])
        candidate = Path(value)
        if candidate.is_absolute() and candidate.is_file() and os.access(candidate, os.X_OK):
            roots.extend(binary_roots(candidate, seen))
    return roots


def _entries():
    path = config_file()
    try:
        raw = path.read_text(encoding="utf-8") if path.is_file() else None
    except OSError:
        raw = None
    if raw is not None:
        entries = json.loads(raw)
        if not isinstance(entries, list):
            raise ValueError("toolchains.json must be a list")
        return entries
    return [{"name": name, "bin": str(Path(found).parent)}
            for name in ("php", "composer") if (found := shutil.which(name))]


def _validate_entry(entry):
    if (not isinstance(entry, dict) or not isinstance(entry.get("name"), str)
            or not re.fullmatch(r"[A-Za-z0-9_.+-]+", entry["name"])
            or not isinstance(entry.get("bin"), str) or not Path(entry["bin"]).is_absolute()
            or not Path(entry["bin"]).is_dir()):
        raise ValueError("toolchains.json entries require name and an absolute bin directory")
    if entry["name"] == "uv":
        python = entry.get("python")
        if not isinstance(python, str) or not Path(python).is_absolute() or not Path(python).is_dir():
            raise ValueError("uv toolchain entries require an absolute python directory")
    if entry["name"] == "cargo":
        for field in ("rustup", "toolchain"):
            value = entry.get(field)
            if not isinstance(value, str) or not Path(value).is_absolute() or not Path(value).is_dir():
                raise ValueError("cargo toolchain entries require absolute rustup and toolchain directories")
        registry = entry.get("registry")
        if registry is not None and (
                not isinstance(registry, str) or not Path(registry).is_absolute() or not Path(registry).is_dir()):
            raise ValueError("cargo toolchain entries require an absolute registry directory when set")


def uv_python_dir(uv_binary):
    """Managed Python install root from `uv python dir` (read-only in lanes)."""
    try:
        done = subprocess.run(
            [str(uv_binary), "python", "dir"], capture_output=True, text=True,
            timeout=15, stdin=subprocess.DEVNULL,
            env={key: os.environ[key] for key in ("HOME", "PATH", "XDG_DATA_HOME", "UV_PYTHON_INSTALL_DIR")
                 if key in os.environ},
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if done.returncode != 0:
        return None
    line = (done.stdout or "").strip().splitlines()
    if not line:
        return None
    directory = Path(line[0].strip())
    return directory.resolve() if directory.is_absolute() and directory.is_dir() else None


def cargo_toolchain_paths(cargo_binary, *, rustc=None):
    """Active rustc sysroot, rustup home, and host registry dir for lane grants."""
    home = Path.home()
    rustup = Path(os.environ.get("RUSTUP_HOME", home / ".rustup"))
    cargo_home = Path(os.environ.get("CARGO_HOME", home / ".cargo"))
    rustc = rustc or Path(cargo_binary).parent / "rustc"
    if not rustc.is_file():
        found = shutil.which("rustc")
        rustc = Path(found) if found else None
    if rustc is None or not rustc.is_file():
        return None
    try:
        done = subprocess.run(
            [str(rustc), "--print", "sysroot"], capture_output=True, text=True,
            timeout=15, stdin=subprocess.DEVNULL,
            env={"HOME": str(home), "PATH": os.environ.get("PATH", "")},
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if done.returncode != 0:
        return None
    toolchain = Path((done.stdout or "").strip())
    if not toolchain.is_absolute() or not toolchain.is_dir():
        return None
    rustup = rustup.resolve()
    registry = (cargo_home / "registry").resolve()
    out = {"rustup": rustup, "toolchain": toolchain.resolve()}
    if registry.is_dir():
        out["registry"] = registry
    return out


def _cargo_bins(entries):
    return {entry["bin"] for entry in entries if isinstance(entry, dict)
            and entry.get("name") == "cargo" and isinstance(entry.get("bin"), str)}


@cache
def land_entries():
    """Fill missing land-check toolchains from the host command path."""
    entries = list(_entries())
    configured = {entry.get("name") for entry in entries}
    for name in ("cargo", "uv", "node", "npm", "php", "composer", "python3"):
        found = shutil.which(name)
        if name in configured or not found:
            continue
        binary = Path(found).absolute()
        entry = {"name": name, "bin": str(binary.parent)}
        if name == "cargo":
            paths = cargo_toolchain_paths(binary)
            if not paths:
                continue
            entry.update({key: str(path) for key, path in paths.items()})
        if name == "uv":
            python = uv_python_dir(binary)
            if not python:
                continue
            entry["python"] = str(python)
        entries.append(entry)
    return entries


@cache
def land_uv_cache():
    """Resolve the host cache independently of the gate's private HOME."""
    entry = next((entry for entry in land_entries() if entry.get("name") == "uv"), None)
    if entry is None:
        return None
    binary = Path(entry["bin"]) / "uv"
    env = {key: os.environ[key] for key in ("HOME", "PATH", "XDG_CACHE_HOME", "UV_CACHE_DIR")
           if key in os.environ}
    try:
        result = subprocess.run([str(binary), "--no-config", "cache", "dir"], env=env,
                                capture_output=True, text=True, timeout=15, stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired):
        return None
    path = Path(result.stdout.strip())
    return path.resolve() if result.returncode == 0 and path.is_absolute() and _grantable(path) else None


@cache
def discover(*, land=False):
    roots = []
    entries = land_entries() if land else _entries()
    cargo_bins = _cargo_bins(entries)
    for entry in entries:
        _validate_entry(entry)
        name, directory = entry["name"], Path(entry["bin"])
        if name == "uv":
            python = Path(entry["python"]).resolve()
            if _grantable(python):
                roots.append(python)
            continue
        if name == "cargo":
            for field in ("toolchain", "registry"):
                raw = entry.get(field)
                if not isinstance(raw, str):
                    continue
                path = Path(raw).resolve()
                if _grantable(path):
                    roots.append(path)
            rustup = Path(entry["rustup"]).resolve()
            if _grantable(rustup):
                roots.append(rustup)
            continue
        if name == "rustc" and entry["bin"] in cargo_bins:
            continue
        roots.extend((directory, directory.resolve()))
        roots.extend(binary_roots(directory / name))
    if land and (cache_root := land_uv_cache()) is not None:
        roots.append(cache_root)
    return tuple(dict.fromkeys(str(root) for root in roots if _grantable(Path(root))))


@cache
def discover_path_dirs(*, land=False):
    """Directories prepended to PATH; uv's parent is listed but not granted as a read subtree."""
    dirs = []
    entries = land_entries() if land else _entries()
    cargo_bins = _cargo_bins(entries)
    for entry in entries:
        _validate_entry(entry)
        name, directory = entry["name"], Path(entry["bin"])
        dirs.append(str(directory.resolve()))
        if name in ("uv", "cargo") or (name == "rustc" and entry["bin"] in cargo_bins):
            continue
        for root in binary_roots(directory / name):
            if _grantable(root):
                dirs.append(str(root.resolve()))
    return tuple(dict.fromkeys(dirs))


@cache
def discover_sandbox_links(*, land=False):
    """Literal read/exec grants (uv and Rust proxies; not whole bin directories)."""
    links = []
    for entry in land_entries() if land else _entries():
        name = entry.get("name")
        if name not in ("uv", "cargo"):
            continue
        _validate_entry(entry)
        tools = ("uv",) if name == "uv" else ("cargo", "rustc")
        if land and name == "cargo":
            tools += ("rustfmt", "cargo-fmt", "clippy-driver", "cargo-clippy")
        for tool in tools:
            called = Path(entry["bin"]) / tool
            binary = called.resolve()
            if binary.is_file():
                # rustup installs cargo and rustc as symlinks to rustup: the sandbox must allow the link a shell
                # calls (PATH lookup and exec) as well as its target, or the lane sees "command not found".
                if called.is_symlink():
                    links.append(called.absolute())
                links.append(binary)
    return tuple(dict.fromkeys(links))


def has_uv_toolchain():
    return any(isinstance(e, dict) and e.get("name") == "uv" for e in _entries())


def has_cargo_toolchain():
    return any(isinstance(e, dict) and e.get("name") == "cargo" for e in _entries())


def toolchain_lane_env(tmpdir, workspace=None):
    """uv/cargo lane variables merged into executor and native CLI child env."""
    env = uv_lane_env(tmpdir)
    if has_cargo_toolchain():
        env.update(cargo_lane_env(tmpdir, workspace))
    return env


def lane_path(path):
    fallback = (Path.home() / ".local/bin", Path("/opt/homebrew/bin"), Path("/usr/local/bin"))
    return os.pathsep.join((*discover_path_dirs(), path, *(str(p) for p in fallback if p.is_dir())))


@contextmanager
def harness_lane_env(workspace, path, *, sandboxed=False):
    """Give a harness lane the CLI toolchain environment with private scratch."""
    scratch_parent = workspace if sandboxed else None
    with tempfile.TemporaryDirectory(prefix=".cs-toolchains-", dir=scratch_parent) as scratch:
        env = toolchain_lane_env(scratch, workspace)
        env.update(PATH=lane_path(path),
                   GIT_OPTIONAL_LOCKS="0", GIT_TERMINAL_PROMPT="0",
                   GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_PAGER="cat")
        yield env


def uv_lane_env(tmpdir, *, land=False):
    """uv writes only under the lane's tmp; it finds Pythons in the granted read-only folder, never by HOME."""
    root = Path(tmpdir)
    env = {
        "UV_CACHE_DIR": str(root / "uv-cache"),
        "UV_NO_CONFIG": "1",
        "UV_OFFLINE": "1",
        "UV_PYTHON_INSTALL_DIR": str(root / "uv-python"),
        "UV_PYTHON_DOWNLOADS": "never",
    }
    for entry in land_entries() if land else _entries():
        if isinstance(entry, dict) and entry.get("name") == "uv" and isinstance(entry.get("python"), str):
            env["UV_PYTHON_INSTALL_DIR"] = str(Path(entry["python"]).resolve())
            break
    return env


def cargo_lane_env(tmpdir, workspace=None, *, land=False):
    """Lane-writable CARGO_HOME; host registry via symlink (read-only grant); offline fetches."""
    root = Path(tmpdir)
    cargo_home = root / "cargo-home"
    cargo_home.mkdir(parents=True, exist_ok=True)
    rustup_home = registry_home = toolchain_name = None
    for entry in land_entries() if land else _entries():
        if not (isinstance(entry, dict) and entry.get("name") == "cargo"):
            continue
        if isinstance(entry.get("rustup"), str):
            rustup_home = Path(entry["rustup"]).resolve()
        if isinstance(entry.get("registry"), str):
            registry_home = Path(entry["registry"]).resolve()
        if isinstance(entry.get("toolchain"), str):
            toolchain_name = Path(entry["toolchain"]).resolve().name
        break
    # Cargo keeps registry layout under CARGO_HOME; symlink the host cache instead of copying it.
    if registry_home and registry_home.is_dir():
        link = cargo_home / "registry"
        if not link.exists():
            link.symlink_to(registry_home, target_is_directory=True)
    (cargo_home / "git").mkdir(parents=True, exist_ok=True)
    env = {"CARGO_HOME": str(cargo_home), "CARGO_NET_OFFLINE": "true"}
    if rustup_home:
        env["RUSTUP_HOME"] = str(rustup_home)
    if toolchain_name:
        env["RUSTUP_TOOLCHAIN"] = toolchain_name
    if workspace:
        env["CARGO_TARGET_DIR"] = str(Path(workspace).resolve() / "target")
    return env


def doctor_findings(path=None):
    """Missing toolchain binaries for cs doctor."""
    path = Path(path) if path is not None else config_file()
    if not path.is_file():
        return []
    try:
        entries = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return [{"check": "toolchains.json", "ok": False, "detail": "%s is not valid JSON" % path,
                 "fix": "fix or delete %s" % path}]
    if not isinstance(entries, list):
        return [{"check": "toolchains.json", "ok": False, "detail": "toolchains.json must be a list",
                 "fix": "fix or delete %s" % path}]
    names = [entry.get("name") for entry in entries if isinstance(entry, dict) and isinstance(entry.get("name"), str)]
    out = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        name, bindir = entry.get("name"), entry.get("bin")
        if not isinstance(name, str) or not isinstance(bindir, str):
            continue
        binary = Path(bindir) / name
        check = "toolchain %s" % name if names.count(name) == 1 else "toolchain %s (%s)" % (name, bindir)
        if not binary.is_file():
            out.append({"check": check, "ok": False, "detail": "%s not found" % binary,
                        "fix": "edit %s to correct the configured toolchain" % path})
        elif name == "uv":
            python = entry.get("python")
            if not isinstance(python, str) or not Path(python).is_dir():
                out.append({"check": check, "ok": False, "detail": "python directory missing for uv",
                            "fix": "edit %s to correct the configured toolchain" % path})
        elif name == "cargo":
            for field, label in (("rustup", "rustup home"), ("toolchain", "toolchain directory")):
                value = entry.get(field)
                if not isinstance(value, str) or not Path(value).is_dir():
                    out.append({"check": check, "ok": False, "detail": "%s missing for cargo" % label,
                                "fix": "edit %s to correct the configured toolchain" % path})
            registry = entry.get("registry")
            if isinstance(registry, str) and not Path(registry).is_dir():
                out.append({"check": check, "ok": False, "detail": "registry directory missing for cargo",
                            "fix": "edit %s to correct the configured toolchain" % path})
    return out


def doctor_login_findings(path, service_path):
    try:
        _, added = login_toolchain_additions(path, service_path)
    except (OSError, ValueError) as error:
        return [{"check": "login shell toolchains", "ok": False, "warn": True,
                 "detail": str(error), "fix": "check the login shell and toolchain configuration"}]
    return [{"check": "toolchain " + entry["name"], "ok": True, "info": True,
             "detail": entry["bin"] + " (run cs init to add)"} for entry in added]


def _grantable(root):
    """A launcher may name any path; never widen the grant to HOME, its ancestors, or ContextStack's own state."""
    home = Path.home().resolve()
    root = root.resolve()
    if root == home or root in home.parents or root == Path("/"):
        return False
    sensitive = {path for name in (".contextstack", ".ssh", ".gnupg", ".aws", ".config", "Library/Keychains")
                 for path in (home / name, (home / name).resolve())}
    return not any(root == path or path in root.parents for path in sensitive)
