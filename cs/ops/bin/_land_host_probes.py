"""Owner policy and host execution for the sandboxed land gate."""
import hashlib
import json
import os
import re
import shlex
import stat
import sys
from pathlib import Path
from tempfile import TemporaryDirectory

from _check_grader import REGRESSIONS, frozen_grader
from _land_check import SEATBELT, SEATBELT_PROBE, detached_checkout, tool_environment

NAME = r"[a-z0-9][a-z0-9_.-]*"
DIRECT = ("sandbox/", "cs/policy/", "cs/ops/bin/_executor.py",
          "cs/ops/bin/_sandbox_policy.py", "cs/ops/bin/_cli_native.py", "cs/ops/bin/_estate_io.py",
          "cs/ops/bin/_land_check.py", "cs/ops/bin/_toolchains.py", "cs/ops/bin/_land_host_probes.py",
          "cs/ops/bin/_check_grader.py", "cs/ops/bin/_lanes.py", "cs/ops/bin/_lane_policy.py",
          "cs/scripts/check-red-gate")
TEMPLATE = SEATBELT_PROBE.with_name("land-host-probes.json")
SKIP = re.compile(
    r"(?:^[ \t]*(?:[\w./-]+: )?|;\s*)skip(?:ped)?\b"
    r"|^check: host-only uncovered: |nested (?:sandbox|seatbelt).*(?:refused|unavailable)",
    re.I | re.M,
)


def config_path(estate):
    root = Path(os.environ.get("CS_STATE_ROOT") or estate)
    if not root.is_absolute():
        raise ValueError("CS_STATE_ROOT must be an absolute path")
    return root / "config/land-host-probes.json"


def policy_repair(template=TEMPLATE):
    return f"Re-seed the owner probe list from {template} with mode 0600, or rerun cs init"


def policy_drift(filename, template=TEMPLATE):
    probes, triggers = load_policy(filename)
    shipped_probes, shipped_triggers = load_policy(template, shipped=True)
    missing = sorted(name for name, args in shipped_probes.items() if probes.get(name) != args)
    paths = sorted(set(shipped_triggers) - set(triggers))
    details = []
    if missing:
        details.append("missing or changed probes: " + ", ".join(missing))
    if paths:
        details.append("missing triggers: " + ", ".join(paths))
    return "; ".join(details)


def load_policy(filename, *checkouts, shipped=False):
    try:
        if any(filename.resolve().is_relative_to(path.resolve()) for path in checkouts):
            raise ValueError("the owner probe list must be outside the checkouts")
        fd = os.open(filename, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd) as source:
            info = os.fstat(source.fileno())
            if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_mode & 0o022
                    or (not shipped and info.st_uid != os.getuid())):
                raise ValueError("expected an owner-controlled regular file")
            policy = json.load(source)
        if not isinstance(policy, dict) or set(policy) != {"probes", "trigger_paths"}:
            raise ValueError("expected probes and trigger_paths")
        probes, triggers = policy["probes"], policy["trigger_paths"]
        if not isinstance(probes, dict) or not probes or any(
            not re.fullmatch(f"{NAME}(?:/{NAME})?", name)
            or any(part in (".", "..") for part in name.split("/"))
            or args not in ([], ["--darwin-probe"]) for name, args in probes.items()
        ):
            raise ValueError("invalid probe rules")
        if not isinstance(triggers, list) or any(
            not isinstance(path, str) or not path
            or not all(re.fullmatch(r"[A-Za-z0-9_.-]+", part) and part not in (".", "..")
                       for part in path.rstrip("/").split("/"))
            for path in triggers
        ):
            raise ValueError("invalid trigger paths")
        required = {*DIRECT, *(str(REGRESSIONS / name.split("/")[0]) + "/" for name in probes)}
        if not required.issubset(triggers):
            raise ValueError("missing required trigger paths: " + ", ".join(sorted(required - set(triggers))))
    except (OSError, ValueError) as error:
        raise ValueError(f"invalid or missing owner probe list {filename}: {error}") from error
    return probes, triggers


def confirm(assume_yes):
    if assume_yes:
        return
    if not sys.stdin.isatty():
        raise RuntimeError("needs owner host probes: use an interactive terminal or --yes-host-probes")
    print("Build and run the merged candidate on the host with frozen probe drivers? [y/N] ",
          end="", flush=True, file=sys.stderr)
    if sys.stdin.readline().strip().lower() not in ("y", "yes"):
        raise RuntimeError("needs owner host probes: owner declined")


def touched_paths(files, triggers):
    return [file for file in files if any(
        file == trigger.rstrip("/") or (trigger.endswith("/") and file.startswith(trigger))
        for trigger in triggers)]


def report(gate, uncovered, covered=()):
    gate.update(host_only_covered=sorted(covered), uncovered=sorted(uncovered),
                coverage_complete=not uncovered)


def build_sandbox(candidate, target, env, run):
    from _land_check import error_tail

    rc, output = run(["cargo", "build", "--locked", "--release", "-p", "cs-sandbox",
                      "--target-dir", str(target)], candidate,
                     {**env, "HOME": str(Path.home()), "RUSTUP_AUTO_INSTALL": "0"})
    if rc:
        raise RuntimeError(f"candidate cs-sandbox build failed ({rc}): {error_tail(output)}")
    binary = target / "release/cs-sandbox"
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError("candidate cs-sandbox build did not produce an executable")
    return binary


def run_probes(main, base, tree, scratch_root, probes, run, gate, git, out):
    if sys.platform != "darwin":
        raise RuntimeError("needs owner host probes: macOS Seatbelt is unavailable")
    env = tool_environment()
    env["PATH"] = os.pathsep.join((str(Path(sys.executable).parent), env["PATH"]))
    # An allow-all profile shipped as policy (Python never emits Seatbelt rules): proves this process can apply Seatbelt.
    on_host, _ = run([SEATBELT, "-f", str(SEATBELT_PROBE), "/usr/bin/true"], main, env)
    if on_host:
        raise RuntimeError("needs owner host probes: cannot apply Seatbelt from this process")
    gate["host_probe_base"] = base
    # A new checkout excludes files generated by the sandboxed gate, including its binary and build cache.
    with TemporaryDirectory(prefix="land-", dir=scratch_root) as scratch_name:
        scratch = Path(scratch_name)
        with detached_checkout(main, base, tree, scratch / "wt", git, out) as candidate:
            binary = build_sandbox(candidate, scratch / "target", env, run)
            gate["host_probe_sandbox_sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
            return verify_probes(main, base, candidate, binary, scratch, probes, run, gate, env)


def verify_probes(main, base, candidate, binary, scratch, probes, run, gate, env):
    pending = set(gate["uncovered"])
    covered = set()
    failures = []
    with frozen_grader(main, base, scratch, workspace=candidate, run_host_only=True,
                       sandbox_binary=binary) as grader:
        if grader is None:
            raise RuntimeError("needs owner host probes: no probe drivers in the frozen base")
        for name, args in probes.items():
            path = grader.root / REGRESSIONS / (name if "/" in name else name + "/check")
            if not path.is_file():
                failures.append(f"FAIL host probe {name}: missing from frozen base")
                continue
            argv = [sys.executable, str(path)] if path.suffix == ".py" else [str(path)]
            probe_env = {**env, "CS_SANDBOX_BIN": str(binary)}
            if name == "tool-escapes-workspace":
                prefix = [str(grader.root / "target/release/cs-sandbox"), "--policy",
                          str(grader.root / "cs/policy/sandbox.json"), "--"]
                probe_env["CS_TOOL_PREFIX"] = shlex.join(prefix)
            rc, output = run([*argv, *args], grader.root / "cs", probe_env)
            if rc:
                failures.append(f"FAIL host probe {name} (exit {rc})\n{output}")
            elif not SKIP.search(output):
                covered.add(name)
                pending.discard(name)
                print(f"check: host-only covered: {name}", file=sys.stderr)
            report(gate, pending, covered)
    return failures
