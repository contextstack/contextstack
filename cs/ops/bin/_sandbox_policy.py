"""Host sandbox policy and the tool prefix (CS phase 3a, Plan.CS.v2.1 §6). Not an op.

`policy/sandbox.json` {"version": 1, "runtime_read": [paths] | {"macos"?: [paths], "linux"?: [paths]}, "allow_net": [nets],
"write_env"?: {VAR: [roots]}, "profiles"?: {"browser": {"read": paths in runtime_read's shapes}},
"sockets"?: {name: absolute socket path}} (a list holds on every OS; phase 3b split it per OS, and the wrapper refuses an OS with no list)
is the host half of the `cs-sandbox` policy; the wrapper intersects it with the tool descriptor's "sandbox" request and gives each
call its own tmp directory outside the workspace. Authority sits here, as for the environment allowlist
(_env_policy) and the roots (_roots): the launcher (tick, run-spec) calls tool_prefix_value() after roots()
and the env resolution and hands the runner CS_TOOL_PREFIX; the runner only prepends its words to every
tool argv (run.go Tool.Run).

Decision rule, tool_prefix_value(lab, env) -> value to set, or None to set nothing:
  1. a policy file that is present is validated first; malformed is SandboxPolicyError (a config void);
  2. CS_TOOL_PREFIX already set: returned unchanged (an operator or a fixture chose it);
  3. no policy file: None (unconfined, nothing to enforce);
  4. policy and wrapper binary present: tool_prefix() joined; a wrapper or policy path holding
     whitespace is SandboxPolicyError, because the runner splits the prefix on whitespace;
  5. policy present, no wrapper: SandboxPolicyError (tick voids the cell, run-spec exits 2), unless
     CS_SANDBOX_DEV_UNCONFINED=1: None and one stderr line saying tools run unconfined.
Wrapper binary: $CS_SANDBOX_BIN, else <repo>/target/release/cs-sandbox, else <repo>/target/debug/
cs-sandbox, where <repo> is the directory holding the lab checkout; else none.

tool_argv(lab, name): what both runners exec after the prefix words: a descriptor naming "bin"
(runtime exe) is [bin, *args], bin relative to <repo> unless absolute; any other tool is [tools/bin/<name>].
cs-sandbox finds an exe tool's descriptor by that bin and args (sandbox/src/policy.rs descriptor_for).

cell_prefix(value, env, reports): the prefix one cell's runner inherits. A prefix the launcher built gains
`--report-dir <reports>/<cell id>` before its `--`, so every wrapped call of that cell writes its report into a dir no
other cell shares (the run id is minted later, by the runner); finalize-run moves the reports whose `cwd` is the run's
workspace into <run dir>/sandbox/. A preset CS_TOOL_PREFIX (in env), no prefix, or no `--` word: unchanged.

sweep_orphan_cells(reports, runs, workspace_of): tick's age sweep for cell dirs no finalize-run will claim (a runner that
died before terminal.json). A cell dir whose newest entry is ORPHAN_AGE_S old is a candidate; it is removed when every
entry is a readable report (*.json, a dict with a cwd) and no pending run (terminal.json, no finalized.json) has that
cwd as its workspace. A pending claim, a torn or foreign file, or an OSError keeps it. Counts candidates only.

browser_state_value(lab, env): LAB_BROWSER_STATE for the runner when the host policy lists it in write_env:
the value passed through when set, else $HOME/.contextstack/lab/browser-state (the state root); None when the
policy is absent or does not list it, so an estate without that grant sees no new variable.
"""
import json
import os
import re
import secrets
import shlex
import sys
import time
from pathlib import Path

LAB = Path(__file__).resolve().parent.parent.parent
POLICY = "policy/sandbox.json"
NETS = ("none", "gateway", "all")
KEYS = ("version", "runtime_read", "allow_net")
OPTIONAL = ("write_env", "profiles", "sockets")
SOCKET_NAME = re.compile(r"[a-z0-9][a-z0-9-]*")  # cs-sandbox policy.rs socket_name_ok
PROFILES = ("browser", "cli-executor")  # the names cs-sandbox knows (sandbox/src/profile.rs)
ROOTED = ("/", "$CODE_ROOT", "$REPO_ROOT", "$HOME")
READ_ROOTED = ROOTED + ("$TOOL_PYTHON",)  # read lists only: the venv interpreter's install (cs-sandbox policy.rs tool_python)
OSES = ("macos", "linux")
VAR = re.compile(r"[A-Z_][A-Z0-9_]*")
BROWSER_STATE = ".contextstack/lab/browser-state"  # under $HOME
ORPHAN_AGE_S = 24 * 3600  # every wrapped call writes a report, so a live cell's dir is never this old in practice
UNBUILT = "sandbox: wrapper not built; tools run unconfined (development mode)"
DEV_UNCONFINED = "CS_SANDBOX_DEV_UNCONFINED"


class SandboxPolicyError(ValueError):
    """The host policy is malformed, or a prefix word would not survive the runner's whitespace split."""


def _strings(value):
    return isinstance(value, list) and all(isinstance(v, str) and v for v in value)


def load_policy(lab=LAB):
    """The validated policy dict; None when the file is absent; SandboxPolicyError when malformed."""
    try:
        raw = (Path(lab) / POLICY).read_text(encoding="utf-8")
    except FileNotFoundError:
        return None
    try:
        pol = json.loads(raw)
        if not isinstance(pol, dict) or not set(KEYS) <= set(pol) <= set(KEYS + OPTIONAL) or pol["version"] != 1:
            raise ValueError("want exactly %s (optional %s) with version 1" % (", ".join(KEYS), ", ".join(OPTIONAL)))
        _paths("runtime_read", pol["runtime_read"])
        if not _strings(pol["allow_net"]) or not set(pol["allow_net"]) <= set(NETS):
            raise ValueError("allow_net: want a subset of %s, got %r" % ("|".join(NETS), pol["allow_net"]))
        write_env = pol.get("write_env", {})
        if not isinstance(write_env, dict):
            raise ValueError("write_env: want {VAR: [absolute roots]}, got %r" % (write_env,))
        for name, roots in write_env.items():
            if not VAR.fullmatch(name) or not _strings(roots) or not roots or not all(r.startswith(ROOTED) for r in roots):
                raise ValueError("write_env: want {VAR: [absolute roots]} (or $CODE_ROOT, $HOME), got %r: %r" % (name, roots))
        profiles = pol.get("profiles", {})
        if not isinstance(profiles, dict) or not set(profiles) <= set(PROFILES):
            raise ValueError("profiles: want {%s: {\"read\": paths}}, got %r" % ("|".join(PROFILES), profiles))
        for name, grant in profiles.items():
            if not isinstance(grant, dict) or set(grant) != {"read"}:
                raise ValueError("profiles.%s: want exactly {\"read\": paths}, got %r" % (name, grant))
            _paths("profiles.%s.read" % name, grant["read"])
        sockets = pol.get("sockets", {})
        if not isinstance(sockets, dict) or not all(
                isinstance(n, str) and SOCKET_NAME.fullmatch(n) and isinstance(p, str) and p.startswith(ROOTED)
                for n, p in sockets.items()):
            raise ValueError("sockets: want {name ([a-z0-9][a-z0-9-]*): absolute path}, got %r" % (sockets,))
    except (ValueError, KeyError) as error:
        raise SandboxPolicyError("%s: %s" % (POLICY, error))
    return pol


def _paths(where, rr):
    """A runtime_read-shaped value: absolute paths as a list, or one list per OS."""
    lists = list(rr.values()) if isinstance(rr, dict) and rr and set(rr) <= set(OSES) else [rr]
    if not all(_strings(l) and all(p.startswith(READ_ROOTED) for p in l) for l in lists):
        raise ValueError("%s: want absolute paths (or $CODE_ROOT, $REPO_ROOT, $HOME, $TOOL_PYTHON), as a list or {%s: [...]}, got %r"
                         % (where, ", ".join(OSES), rr))


def wrapper_path(lab=LAB, env=None):
    """The cs-sandbox binary as a Path, or None when none is built."""
    env = os.environ if env is None else env
    if env.get("CS_SANDBOX_BIN"):
        given = Path(env["CS_SANDBOX_BIN"])
        return given if given.is_file() else None
    repo = Path(lab).resolve().parent
    for build in ("release", "debug"):
        for name in ("cs-sandbox",):
            candidate = repo / "target" / build / name
            if candidate.is_file():
                return candidate
    return None


def tool_prefix(lab=LAB, env=None):
    """[wrapper, --policy, <lab>/policy/sandbox.json, --]; static per spec (cell_prefix adds the per-cell report dir)."""
    wrapper = wrapper_path(lab, env)
    if wrapper is None:
        return []
    words = [str(wrapper), "--policy", str(Path(lab).resolve() / POLICY), "--"]
    for word in words:
        if word != "".join(word.split()):
            raise SandboxPolicyError("%r: a prefix path may not contain whitespace (the runner splits on it)" % word)
    return words


def prefix_value(words):
    """CS_TOOL_PREFIX as the runner reads it (strings.Fields): shell words, none containing whitespace."""
    return shlex.join(words)


def tool_prefix_value(lab=LAB, env=None, stderr=None):
    """The decision rule in the module docstring."""
    env = os.environ if env is None else env
    policy = load_policy(lab)
    if "CS_TOOL_PREFIX" in env:
        return env["CS_TOOL_PREFIX"]
    if policy is None:
        return None
    words = tool_prefix(lab, env)
    if words:
        return prefix_value(words)
    if env.get(DEV_UNCONFINED) == "1":
        print(UNBUILT, file=stderr or sys.stderr, flush=True)
        return None
    raise SandboxPolicyError(UNBUILT)


def cell_prefix(value, env, reports):
    """The decision in the module docstring: `value` with a fresh per-cell `--report-dir`, or unchanged."""
    words = shlex.split(value) if value else []
    if "CS_TOOL_PREFIX" in env or "--" not in words or "--report-dir" in words:
        return value
    cell = "%d-%d-%s" % (time.time_ns(), os.getpid(), secrets.token_hex(4))
    cut = words.index("--")
    words[cut:cut] = ["--report-dir", str(Path(reports) / cell)]
    if any(word != "".join(word.split()) for word in words):
        raise SandboxPolicyError("%r: a report dir may not contain whitespace (the runner splits on it)" % str(reports))
    return prefix_value(words)


def tool_argv(lab, name):
    """[exec path, *args] for tool `name`, as the runners build it after the prefix (module docstring)."""
    lab = Path(lab)
    desc = lab / "tools/bin" / (name + ".json")
    d = json.loads(desc.read_text(encoding="utf-8")) if desc.is_file() else {}
    d = d if isinstance(d, dict) else {}
    if not d.get("bin"):
        return [str(lab / "tools/bin" / name)] + [str(a) for a in d.get("args") or []]
    return [str(lab.resolve().parent / d["bin"])] + [str(a) for a in d.get("args") or []]


def browser_state_value(lab=LAB, env=None):
    """LAB_BROWSER_STATE for the runner's children, or None to set nothing (module docstring)."""
    env = os.environ if env is None else env
    policy = load_policy(lab)
    if policy is None or "LAB_BROWSER_STATE" not in policy.get("write_env", {}):
        return None
    if env.get("LAB_BROWSER_STATE"):
        return env["LAB_BROWSER_STATE"]
    return str(Path(env.get("HOME") or Path.home()) / BROWSER_STATE)


def _report_cwd(path):
    """The real cwd of one readable report file, else None (ambiguous: the cell is kept)."""
    if path.suffix != ".json" or path.is_symlink() or not path.is_file():
        return None
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    cwd = report.get("cwd") if isinstance(report, dict) else None
    return os.path.realpath(cwd) if isinstance(cwd, str) and cwd else None


def sweep_orphan_cells(reports, runs, workspace_of, now=None, max_age=ORPHAN_AGE_S):
    """The rule in the module docstring; returns {"removed": n, "kept": n} over cells past max_age."""
    now, counts, claims = time.time() if now is None else now, {"removed": 0, "kept": 0}, None
    for cell in sorted(p for p in Path(reports).iterdir() if p.is_dir()) if Path(reports).is_dir() else ():
        try:
            entries = list(cell.iterdir())
            if now - max([cell.stat().st_mtime] + [e.lstat().st_mtime for e in entries]) < max_age:
                continue
            if claims is None:  # only computed when a candidate exists
                claims = {os.path.realpath(str(workspace_of(d))) for d in Path(runs).iterdir()
                          if (d / "terminal.json").is_file() and not (d / "finalized.json").exists()} \
                    if Path(runs).is_dir() else set()
            if any(cwd is None or cwd in claims for cwd in map(_report_cwd, entries)):
                counts["kept"] += 1
                continue
            for entry in entries:
                entry.unlink()
            cell.rmdir()
            counts["removed"] += 1
        except OSError:
            counts["kept"] += 1
    return counts
