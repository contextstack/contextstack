"""One runner selector for launch, accounting, rebuild and freshness."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def default_runner(lab):
    """The checkout's Rust runner, <repo>/target/release/cs-harness (ADR 0011: Go is archived, never a default)."""
    return Path(lab).parent / "target" / "release" / "cs-harness"


def go_root(lab):
    """The archived Go runner's source: <repo>/lab, beside the code root wherever that lives (ADR 0011)."""
    return Path(lab).parent / "lab"


def runner_path(lab, env=None):
    """CS_RUNNER_BIN wins; LAB_RUNNER_BIN remains compatible; default is the Rust runner (default_runner)."""
    env = os.environ if env is None else env
    value = env.get("CS_RUNNER_BIN") or env.get("LAB_RUNNER_BIN")
    path = Path(value).expanduser() if value else default_runner(lab)
    return (path if path.is_absolute() else Path(lab) / path).absolute()


def built_runner(lab):
    """Regressions: $CS_RUNNER_BIN when set, else the default Rust runner, which scripts/check builds first."""
    path = runner_path(lab, {"CS_RUNNER_BIN": os.environ.get("CS_RUNNER_BIN", "")})
    if not path.is_file():
        raise SystemExit("check: runner %s is not built: cargo build --release -p cs-harness at the repo root" % path)
    return path


def identity(lab, path=None):
    """{kind, path, sha256} of the executable a run would launch (kind None for an unknown name)."""
    path = runner_path(lab) if path is None else Path(path)
    try:
        kind = runner_kind(lab, path)
    except ValueError:
        kind = None
    try:
        sha = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        sha = None
    return {"kind": kind, "path": str(path), "sha256": sha}


def runner_kind(lab, path=None):
    """Only known Go/Rust runner artifacts can be rebuilt automatically."""
    path = runner_path(lab) if path is None else Path(path)
    if path.name in ("lab", "cs-go"):  # archived Go runner artifacts
        return "go"
    if path.name in ("cs-harness",):
        return "rust"
    raise ValueError("cannot rebuild unknown runner %s (expected lab, cs-go or cs-harness)" % path)


def source_paths(lab, path=None):
    """Include build metadata as well as every first-party source file."""
    lab = Path(lab)
    if runner_kind(lab, path) == "go":
        go = go_root(lab)
        return [p for d in ("cmd", "internal") for p in (go / d).rglob("*.go")
                if not p.name.endswith("_test.go")] + [go / "go.mod", go / "go.sum"]
    repo = lab.parent
    return list((repo / "harness").rglob("*.rs")) + [repo / name for name in
           ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "harness/Cargo.toml")]


def freshness(lab):
    """Return a diagnostic or None; an absent unconfigured default is allowed (scripts/check builds it)."""
    path = runner_path(lab)
    if not path.is_file() or not os.access(path, os.X_OK):
        return "configured runner is missing or not executable: %s" % path if (
            os.environ.get("CS_RUNNER_BIN") or os.environ.get("LAB_RUNNER_BIN")) else None
    newer = [str(p) for p in source_paths(lab, path) if p.is_file() and p.stat().st_mtime > path.stat().st_mtime]
    return "%s is older than runner source — rebuild the configured runner" % path if newer else None


def rebuild(lab):
    """Build the selected implementation, atomically install it at the selected path."""
    lab = Path(lab)
    path = runner_path(lab)
    kind = runner_kind(lab, path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".runner-build-", dir=path.parent) as raw:
        output = Path(raw) / path.name
        if kind == "go":
            command, cwd = ["go", "build", "-o", str(output), "./cmd/lab"], go_root(lab)  # only when named
        else:
            target = path.parent / ".cs-build"
            command = ["cargo", "build", "--locked", "--manifest-path", str(lab.parent / "Cargo.toml"),
                       "--package", "cs-harness", "--bin", "cs-harness", "--target-dir", str(target)]
            if path.parent.name == "release":
                command.append("--release")
            output = target / ("release" if "--release" in command else "debug") / "cs-harness"
            cwd = lab.parent
        result = subprocess.run(command, cwd=cwd, capture_output=True, text=True)
        if result.returncode == 0:
            staged = Path(raw) / "installed"
            shutil.copy2(output, staged)
            os.replace(staged, path)
        return {"ok": result.returncode == 0, "runner": str(path), "implementation": kind,
                "output": ((result.stdout or "") + (result.stderr or "")).strip()}


def running_count(lab):
    """Count native runner executable paths, not unrelated processes named lab/cs."""
    lab = Path(lab)
    paths = {runner_path(lab).resolve(), (go_root(lab) / "lab").resolve()}
    for stem in ("cs-harness",):
        paths.update((lab.parent / "target" / mode / stem).resolve() for mode in ("debug", "release"))
    result = subprocess.run(["ps", "-axo", "pid=,comm="], capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError("cannot inspect running runner processes: " + (result.stderr or "").strip())
    count = 0
    for line in result.stdout.splitlines():
        fields = line.strip().split(None, 1)
        if len(fields) != 2 or not fields[0].isdigit():
            continue
        executable = Path("/proc") / fields[0] / "exe"
        try:
            path = executable.resolve(strict=True) if executable.exists() else Path(fields[1]).resolve()
        except OSError:
            continue
        count += path in paths
    return count


if __name__ == "__main__":
    import sys
    root = Path(__file__).resolve().parents[2]
    if os.environ.get("LAB_RUNNER_BIN") and not os.environ.get("CS_RUNNER_BIN"):
        print("warning: LAB_RUNNER_BIN is a compatibility alias; use CS_RUNNER_BIN", file=sys.stderr)
    try:
        error = freshness(root)
    except (ValueError, OSError) as exc:
        error = str(exc)
    if error:
        print(error)
        sys.exit(1)
