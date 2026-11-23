"""Shared file/process transport; callers retain lifecycle authority checks."""
import hashlib
import json
import os
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path


import stat

# Shared atomic write (the same code ships as cs/scripts/_atomic_write.py; a py-core-units test keeps them equal).

def _fsync_dir(path):
    fd = os.open(str(path), os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_atomic(path, data, *, mode=None, encoding=None):
    """Replace `path` atomically: temp write, fsync file, rename, fsync parent directory.

    `data` is bytes, or text when `encoding` is set (default utf-8). When `mode` is set the result is chmod'd;
    otherwise an existing file keeps its mode."""
    path = Path(path)
    if isinstance(data, str):
        payload = data.encode(encoding or "utf-8")
    else:
        payload = data
    path.parent.mkdir(parents=True, exist_ok=True)
    if mode is None and path.is_file():
        try:
            mode = stat.S_IMODE(path.stat().st_mode)
        except OSError:
            pass
    tmp = path.with_name(".%s.%d.tmp" % (path.name, os.getpid()))
    try:
        if mode is not None:
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
            fd = os.open(tmp, flags, mode)
            with os.fdopen(fd, "wb") as out:
                out.write(payload)
                out.flush()
                os.fsync(out.fileno())
        else:
            with open(tmp, "wb") as out:
                out.write(payload)
                out.flush()
                os.fsync(out.fileno())
        os.replace(tmp, path)
        if mode is not None:
            try:
                os.chmod(path, mode)
            except OSError:
                pass
        _fsync_dir(path.parent)
    except BaseException:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        raise


# Rows a shelf carries about the estate itself rather than its domain: estate-cutover appends one
# {"event": "estate_cutover", "from", "to", "aliases", ...} row to every shelf. Readers of a shelf's
# runs, schedules, jobs or judgments never see them; estate-cutover's own verify reads the raw bytes.
BOOKKEEPING_EVENTS = frozenset({"estate_cutover"})


def bookkeeping(row):
    """True for an estate bookkeeping row (BOOKKEEPING_EVENTS), which no domain reader may count or index."""
    return isinstance(row, dict) and row.get("event") in BOOKKEEPING_EVENTS


def read_jsonl(path, *, strict, with_bookkeeping=False):
    """Strict shelves preserve JSON values and fail; tolerant shelves keep objects. Bookkeeping rows are
    skipped unless asked for."""
    return list(iter_jsonl(path, strict=strict, errors="strict" if strict else "replace",
                           objects_only=not strict, with_bookkeeping=with_bookkeeping))


def iter_jsonl(path, *, strict, errors, objects_only, with_bookkeeping=False):
    """Decode the whole file, then parse rows on demand, including early-stop readers. Bookkeeping rows
    (estate_cutover) are skipped unless `with_bookkeeping` is true."""
    path = Path(path)
    if not path.is_file():
        return
    for line in path.read_text(encoding="utf-8", errors=errors).splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            if strict:
                raise
            continue
        if not with_bookkeeping and bookkeeping(value):
            continue
        if not objects_only or isinstance(value, dict):
            yield value


def state_dir(code_root):
    """The state root for a ContextStack checkout (CS phase 6 step 10): <code-root>/ledger exactly as given
    (byte for byte, no symlink resolution) unless CS_STATE_ROOT is set, then _roots' validated rule.
    Every reader of a state shelf goes through here, so a relocated state root moves all of them."""
    if not os.environ.get("CS_STATE_ROOT"):
        return Path(code_root) / "ledger"
    from _roots import roots
    return roots(code_root)["state"]


def code_path(code_root, path):
    """A code-root-relative or absolute path, lexically, with anything under <code-root>/ledger/ mapped into the state root
    Paths such as "ledger/setups/x" and a task's ../../../ledger/... setup link keep working once the ledger has
    left the checkout. With the default state root the path is unchanged."""
    code_root, path = Path(code_root), Path(path)
    path = Path(os.path.normpath(path if path.is_absolute() else code_root / path))
    try:
        rest = path.relative_to(os.path.normpath(code_root / "ledger"))
    except ValueError:
        return path
    return state_dir(code_root) / rest


def link_target(code_root, link):
    """Where a task's `setup` symlink points, through code_path; a non-link is returned as is."""
    link = Path(link)
    if not link.is_symlink():
        return link
    text = os.readlink(link)
    return code_path(code_root, text if os.path.isabs(text) else link.parent / text)


def estate_dir(code_root):
    """The directory whose ESTATE names this estate (P1): for a code_root code root (INVARIANTS.md) the
    config root when CS_CONFIG_ROOT is set (an install: config/ESTATE; releases ship no ESTATE), else the
    directory itself (a checkout's cs/, a config dir, a fixture), byte for byte as before."""
    code_root = Path(code_root)
    if os.environ.get("CS_CONFIG_ROOT") and (code_root / "INVARIANTS.md").is_file():
        from _roots import roots
        return roots(code_root)["config"]
    return code_root


def estate_id(code_root):
    value = (estate_dir(code_root) / "ESTATE").read_text().strip()
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,31}", value):
        raise ValueError("ESTATE must contain one short lowercase estate id")
    return value


CUTOVER_V2 = "estate-cutover-v2.json"  # in the state root: estate, aliases, the prior record and shelf prefixes


def cutover_record(state):
    """The state root's estate-cutover-v2 record (ops/bin/estate-cutover), or {}."""
    path = Path(state) / CUTOVER_V2
    try:
        record = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return record if isinstance(record, dict) and record.get("schema") == "estate-cutover-v2" else {}


def estate_aliases(code_root):
    """Earlier names this estate continues (D6: dev -> workstation): the v2 record's aliases when its estate is this one."""
    record = cutover_record(state_dir(code_root))
    try:
        ident = estate_id(code_root)
    except (OSError, ValueError):
        return ()
    aliases = record.get("aliases") if record.get("estate") == ident else None
    return tuple(a for a in aliases or () if isinstance(a, str))


def estate_matches(code_root, value):
    """True when `value` names this estate: its ESTATE or an alias the cutover record carries forward. Grants,
    cohort templates and schedules issued as dev stay valid on workstation without being rewritten."""
    try:
        return value == estate_id(code_root) or value in estate_aliases(code_root)
    except (OSError, ValueError):
        return False


def relocated_estate(path):
    """The config root holding ESTATE when `path` is a shelf directly in a relocated state root (CS_STATE_ROOT),
    so rows there are stamped exactly as rows in <code-root>/ledger are; else None."""
    if not os.environ.get("CS_STATE_ROOT"):
        return None
    try:
        from _roots import roots
        found = roots()
    except ValueError:
        return None
    if os.path.realpath(str(Path(path).parent)) == os.path.realpath(str(found["state"])) \
            and (found["config"] / "ESTATE").exists():
        return found["config"]
    return None


def estate_row(path, row):
    path = Path(path)
    config = relocated_estate(path)
    if config is not None:
        ident = estate_id(config)
        record = cutover_record(path.parent)
        if record and record.get("estate") != ident:  # the dev checkout pointed at workstation's state root, or the reverse
            raise ValueError("state root %s belongs to estate %s, this is %s: set CS_STATE_ROOT to this estate's own"
                             % (path.parent, record.get("estate"), ident))
        aliases = record.get("aliases") or () if record.get("estate") == ident else ()
        if row.get("estate", ident) != ident and row.get("estate") not in aliases:
            raise ValueError("cannot append a row for another estate")
        return {**row, "estate": ident}  # a copied-forward row of an alias (dev) is re-stamped as this estate
    if path.parent.name == "ledger" and ((path.parent.parent / "ESTATE").exists() or (path.parent / "estate-cutover.json").exists()):
        ident = estate_id(path.parent.parent)
        if row.get("estate", ident) != ident:
            raise ValueError("cannot append a row for another estate")
        return {**row, "estate": ident}
    return row  # Generic transport and pre-cutover fixtures have no estate.


def append_jsonl(path, row, *, sort_keys, compact, sync, encoding):
    """Append with the caller's byte/durability contract; caller owns directory and lock."""
    with Path(path).open("a", encoding=encoding) as out:
        out.write(json.dumps(estate_row(path, row), sort_keys=sort_keys,
                             separators=(",", ":") if compact else None) + "\n")
        out.flush()
        if sync:
            os.fsync(out.fileno())


def pid_live(pid):
    if not pid:
        return False
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def utc_time(value):
    if not isinstance(value, str) or not value:
        return None
    raw = value.strip()
    if len(raw) == 13 and raw[10] == "T":
        raw += ":00:00Z"
    try:
        stamp = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError:
        return None
    if stamp.tzinfo is None:
        stamp = stamp.replace(tzinfo=timezone.utc)
    return stamp.astimezone(timezone.utc)


def hardened_git_env(extra=None):
    """Isolate Git from inherited overrides and user/system configuration."""
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
               GIT_TERMINAL_PROMPT="0", GIT_PAGER="cat", LC_ALL="C")
    if extra:
        env.update(extra)
    return env


def git(cwd, *args, env, errors):
    return subprocess.run(["git", "-C", str(cwd), *args], capture_output=True,
                          text=True, env=env, errors=errors)


def run_op(code_root, name, payload, *, env=None):
    result = subprocess.run([str(code_root / "ops/bin" / name)], input=json.dumps(payload),
                            capture_output=True, text=True, cwd=str(code_root), env=env)
    if result.returncode != 0:
        return None, (result.stderr or result.stdout or name + " failed").strip()
    text = (result.stdout or "").strip()
    try:
        return json.loads(text.splitlines()[-1]), None
    except json.JSONDecodeError:
        return {"ok": True, "output": text}, None


def sha256_bytes(raw):
    return hashlib.sha256(raw).hexdigest()


def digest(value):
    return sha256_bytes(json.dumps(value, sort_keys=True, separators=(",", ":")).encode())


def verify_frozen_stimulus(run_dir, stimulus):
    if stimulus["manifest"] != "stimulus/manifest.json":
        raise ValueError("invalid stimulus reference")
    manifest = json.loads((run_dir / stimulus["manifest"]).read_text())
    frozen_sha = manifest.pop("stimulus_sha")
    if manifest["run_id"] != run_dir.name or stimulus["sha256"] != frozen_sha or digest(manifest) != frozen_sha:
        raise ValueError("receipt does not bind the frozen stimulus manifest")
    for item in manifest["inputs"]:
        blob = (run_dir / "stimulus" / item["path"]).resolve()
        if not blob.is_relative_to(run_dir / "stimulus") or sha256_bytes(blob.read_bytes()) != item["sha256"]:
            raise ValueError("frozen stimulus input differs from its hash")


def call_op(code_root, name, payload, *, failure=None):
    result = subprocess.run([str(code_root / "ops/bin" / name)], input=json.dumps(payload),
                            text=True, capture_output=True, cwd=str(code_root))
    if result.returncode:
        raise RuntimeError((result.stderr or result.stdout or failure or name + " failed").strip())
    return json.loads(result.stdout.splitlines()[-1])


def basename(value):
    text = str(value or "").strip().rstrip("/")
    return text.split("/")[-1] if text else ""


def sandbox_request(*, workspace, tmpdir, home, executable, lookup_path, ssh_auth_sock, **fields):
    """Build v1 inputs without discovering state or composing Seatbelt rules."""
    request = dict(version=1, variant="native", workspace=str(Path(workspace).resolve()),
                   tmpdir=str(Path(tmpdir).resolve()), home=str(Path(home).resolve()),
                   # Absolute but unresolved: Rust walks launcher symlink hops (e.g. ~/.local/bin/claude)
                   # and grants each one, exactly as Python's shutil.which-based renderer did.
                   executable=os.path.abspath(str(executable)),
                   lookup_path=[os.path.abspath(str(p)) for p in lookup_path],
                   # A relative SSH_AUTH_SOCK must not block every run; Rust also denies its resolved form.
                   ssh_auth_sock=os.path.abspath(str(ssh_auth_sock)) if ssh_auth_sock else None)
    request.update(fields)
    return request


def render_sandbox_request(binary, policy, request, *, timeout=10):
    """Return Rust's bytes or fail closed; this helper never launches the executor."""
    try:
        result = subprocess.run(
            [str(binary), "--policy", str(policy), "--profile", "cli-executor", "--request", "-"],
            input=json.dumps(request).encode(), capture_output=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("sandbox request failed: %s" % error) from error
    if result.returncode:
        raise RuntimeError("sandbox request failed (%d): %s" % (
            result.returncode, result.stderr.decode("utf-8", errors="replace").strip()))
    if not result.stdout.startswith(b"(version 1)\n"):
        raise RuntimeError("sandbox request returned an incompatible profile")
    return result.stdout


def native_caller(template, home, run, workspace, tmpdir, lane=None):
    """Discover and validate caller grants; Rust alone composes their rules."""
    home, run = Path(home), Path(run).resolve()
    denies = []
    for raw in template.get("deny_write", []):
        path = home / raw[2:] if raw.startswith("~/") else Path(raw)
        if not path.is_absolute():
            raise ValueError("deny_write path is not absolute: %s" % raw)
        denies.append(str(path))
    caller = dict(deny_write_roots=denies,
                  run_protection=dict(run_root=str(run), workspace=str(Path(workspace).resolve()),
                                      tmpdir=str(Path(tmpdir).resolve()), refresh_write_paths=[]))
    read_roots = ()
    if lane is not None:
        import _lanes
        main, name = lane["main"], lane["lane"]
        read_roots = _lanes.sandbox_read_roots(main, name, workspace)
        common = _lanes.git_common_dir(main)
        gitdir = _lanes.assert_lane_git_pointer(workspace, main, name)
        _lanes.prepare_lane_refs(main, name, gitdir)
        caller["lane"] = dict(common_git_dir=str(common), own_worktree=str(Path(workspace).resolve()),
                              own_git_dir=str(gitdir), other_worktrees=list(_lanes.worktrees(main)),
                              ref_paths=[str(common / "refs/heads/lane" / name),
                                         str(common / "logs/refs/heads/lane" / name)])
    return caller, read_roots


def native_sandbox_request(binary, workspace, tmpdir, home, *, protected=None, read_links=(),
                           login=None, read_roots=(), home_reads=(), env=None, caller=None):
    from _cli_native import declared_home_reads, _login_targets
    from _toolchains import discover, discover_sandbox_links, has_cargo_toolchain, has_uv_toolchain
    env = os.environ if env is None else env
    login = {} if login is None else login
    caller = {} if caller is None else caller
    import shutil
    if not Path(binary).is_absolute():
        # Like the Python renderer: an unresolvable launcher grants only its absolute path.
        found = shutil.which(binary, path=env.get("PATH", "")) or os.path.abspath(binary)
        binary = found
    roots, proxies = discover(), discover_sandbox_links()
    uv_caches = []
    if has_uv_toolchain():
        uv_caches = [Path(home) / ".cache/uv", Path(home) / "Library/Caches/uv"]
        if env.get("XDG_CACHE_HOME"):
            uv_caches.append(Path(env["XDG_CACHE_HOME"]) / "uv")
        if env.get("UV_CACHE_DIR"):
            uv_caches.append(Path(env["UV_CACHE_DIR"]))
        lane_cache = (Path(tmpdir) / "uv-cache").resolve()
        uv_caches = list(dict.fromkeys(str(path.resolve()) for path in uv_caches
                                      if path.is_absolute() and not lane_cache.is_relative_to(path.resolve())))
    return sandbox_request(
        workspace=workspace, tmpdir=tmpdir, home=home, executable=binary,
        lookup_path=[p for p in env.get("PATH", "").split(os.pathsep) if p],
        ssh_auth_sock=env.get("SSH_AUTH_SOCK"),
        read_links=[str(Path(p).resolve()) for p in (*read_links, *proxies)],
        read_roots=[str(Path(p).resolve()) for p in (*read_roots, *roots, *((protected,) if protected is not None else ()))],
        home_reads=[str(p) for p in (*home_reads, *declared_home_reads(login, home, tmpdir))],
        protected=str(Path(protected).resolve()) if protected is not None else None,
        interpreters=login.get("interpreters", []), sockets_allow=login.get("sockets_allow", []),
        login_writes=login.get("login_writes", []),
        login_targets=[dict(path=str(path), allow_directory=allow_dir)
                       for path, allow_dir in _login_targets(login, home)],
        toolchains=dict(cargo_configured=has_cargo_toolchain(), read_roots=[str(p) for p in roots],
                        proxy_paths=[str(p) for p in proxies], uv_cache_roots=uv_caches), **caller)


def native_sandbox_binary(code_root, *, env=None):
    """Resolve only this release's binary, or an explicit test/development override."""
    env = os.environ if env is None else env
    release = Path(code_root).resolve().parent
    if env.get("CS_SANDBOX_BIN"):
        return Path(env["CS_SANDBOX_BIN"])
    installed = release / "bin/cs-sandbox"
    if installed.is_file():
        return installed
    return release / "target/release/cs-sandbox"


def sandbox_runtime_code_root():
    """Use the installed runtime even when Muse is loaded from a frozen bundle."""
    return Path(__file__).resolve().parents[2]


def render_native_sandbox(code_root, policy, request):
    return render_sandbox_request(native_sandbox_binary(code_root), policy, request)


def spec_intent(spec):
    """Read the optional Director task link, refusing retired and malformed identifiers."""
    value = spec.get("intent")
    if value is None:
        return {}
    if isinstance(value, str) and "-" in value:
        raise ValueError("intent: retired format; use a Director ULID")
    if not isinstance(value, str) or not re.fullmatch(r"[0-7][0-9A-HJKMNP-TV-Z]{25}", value):
        raise ValueError("intent must be a Director ULID")
    return {"intent": value}
