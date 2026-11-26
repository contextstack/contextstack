"""Present base-commit graders beside the candidate code for a lane check."""
from contextlib import contextmanager, nullcontext
from dataclasses import dataclass
from pathlib import Path
import os
import re
import shlex
import shutil
import subprocess
import tarfile
import tempfile

from _estate_io import hardened_git_env, sandbox_runtime_code_root
from _lanes import workspace_out

REGRESSIONS = Path("cs/tasks/regressions")
SCRIPTS = Path("cs/scripts")
GRADER_PATHS = (REGRESSIONS, *(SCRIPTS / name for name in (
    "check", "check_timeout.py", "check-regression-coverage.py", "checklib.sh",
)))
LANE_NOTE = "New regressions run at land through the host gate."
GRADER_REPORT_KEYS = ("check_summary", "grader_base_sha", "host_only_skips",
                      "grader_note", "new_grader_paths", "grader_binaries")
RELEASE_BINARIES = ("cs-director", "cs-gateway", "cs-harness", "cs-sandbox", "cs-tools")
MISSING_FILE = re.compile(
    r"no such file or directory|:\s*not found\b|cannot open|can't open|FileNotFoundError", re.I
)


def _mentions_path(line, path, root):
    relative = Path(path).relative_to("cs")
    return any(re.search(r"(?<![\w./-])(?:\./)?" + re.escape(str(name)) + r"(?![\w./-])", line)
               for name in (path, relative, root / path, root / "cs" / relative))


@dataclass
class Grader:
    root: Path
    host_only: dict
    base_sha: str
    new_grader_paths: list
    binaries: dict

    def environment(self, env):
        return dict(env, CS_CODE_ROOT=str((self.root / "cs").resolve()))

    def report(self, output, returncode):
        skips = self.skips(output)
        summary = "pass" if returncode == 0 else "fail"
        if skips and returncode == 0:
            summary += f" with {len(skips)} host-only skips"
        if returncode > 0 and self.new_grader_paths and self.new_path_failure(output):
            summary = "pending land: new regression"
        return dict(grader_base_sha=self.base_sha, host_only_skips=skips,
                    check_summary=summary, grader_note=LANE_NOTE,
                    new_grader_paths=self.new_grader_paths, grader_binaries=self.binaries)

    def new_path_failure(self, output):
        def involves_addition(line):
            return any(_mentions_path(line, path, self.root)
                       and (MISSING_FILE.search(line)
                            or (line.startswith("check: FAIL — ")
                                and Path(path).match("cs/tasks/regressions/*/check")))
                       for path in self.new_grader_paths)

        # The full check prefixes each failed rule before printing its diagnostics.
        # If it reports several failed rules, each must involve an addition.
        prelude, *failures = re.split(r"(?m)(?=^check: FAIL — )", output)
        return all(any(involves_addition(line) for line in block.splitlines())
                   for block in failures or [prelude])

    def skips(self, output):
        lines = set(output.splitlines())
        return [{"regression": name, "outcome": "skipped", "reason": reason,
                 "message": skip_message(name, reason)}
                for name, reason in self.host_only.items() if skip_message(name, reason) in lines]


def skip_message(name, reason):
    return f"{name}: skipped: needs host ({reason})"


def lane_grader(lane, workspace, scratch):
    if not lane or lane.get("read_only"):
        return nullcontext()
    source = lane["main"]
    base = lane.get("grader_base_sha")
    if not base:
        # Older receipts stored the lane tip. Derive its common ancestor with
        # the owner checkout, since the lane tip may contain grader edits.
        base = subprocess.check_output(
            ["git", "--no-replace-objects", "-C", source, "merge-base", "HEAD", lane["base"]],
            env=hardened_git_env(), text=True,
        ).strip()
    return frozen_grader(source, base, scratch, workspace=workspace, lane=lane)


def _candidate_links(source, target, frozen):
    # Split only the directories needed by the overlay. Keep the rest in the
    # candidate workspace, including generated files and the linked Git admin.
    for entry in source.iterdir():
        dest = target / entry.name
        if dest.is_symlink():
            continue
        if dest.is_dir():
            if not entry.is_symlink() and entry.is_dir() and not dest.samefile(frozen):
                _candidate_links(entry, dest, frozen)
        elif not dest.exists():
            dest.symlink_to(entry)


def _link_release_binaries(workspace, root):
    runtime = sandbox_runtime_code_root().parent
    release = root / "target/release"
    release.mkdir(parents=True)
    binaries = {}
    for name in RELEASE_BINARIES:
        installed = next((path for path in (runtime / "bin" / name, runtime / "target/release" / name)
                          if path.is_file() and os.access(path, os.X_OK)), None)
        candidate = workspace / "target/release" / name
        chosen = installed
        if candidate.is_file() and os.access(candidate, os.X_OK):
            if installed is None or candidate.stat().st_mtime_ns > installed.stat().st_mtime_ns:
                chosen = candidate
        if chosen is not None:
            chosen = chosen.resolve()
            (release / name).symlink_to(chosen)
            binaries[name] = str(chosen)
    return binaries


@contextmanager
def frozen_grader(source, base_sha, scratch, *, workspace=None, lane=None):
    if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", base_sha):
        raise ValueError("invalid grader base commit")
    workspace = workspace or source
    git = ["git", "--no-replace-objects", "-C", str(source)]
    env = hardened_git_env()
    paths = [str(path) for path in GRADER_PATHS]
    listed = subprocess.check_output([*git, "ls-tree", "-z", "--name-only", base_sha, "--", *paths], env=env)
    selected = [path for path in listed.decode().split("\0") if path]
    if not selected and not (workspace / "cs").is_dir():
        yield None
        return

    base_paths = subprocess.check_output(
        [*git, "ls-tree", "-rz", "--name-only", base_sha, "--", *paths], env=env,
    ).decode(errors="surrogateescape").split("\0")
    added = workspace_out(workspace, "diff", "--no-renames",
                          "--diff-filter=A", "--name-only", "-z", base_sha, "--", *paths, lane_ctx=lane)
    untracked = workspace_out(workspace, "ls-files", "--others", "--exclude-standard", "-z",
                              "--", *paths, lane_ctx=lane)
    new_grader_paths = sorted((set(added.split("\0")) | set(untracked.split("\0"))) - set(base_paths))

    with tempfile.TemporaryDirectory(prefix="check-grader-", dir=scratch) as directory:
        root = Path(directory)
        archive = root / "grader.tar"
        if selected:
            with archive.open("wb") as handle:
                subprocess.run([*git, "archive", base_sha, "--", *selected], env=env, stdout=handle, check=True)
            with tarfile.open(archive) as contents:
                # The grader tree is plain files and directories. In particular,
                # a base symlink must never resolve to candidate grader code.
                for member in contents:
                    if not (member.isdir() or member.isfile()) or ".." in Path(member.name).parts or Path(member.name).is_absolute():
                        raise ValueError(f"invalid grader archive entry: {member.name}")
                    contents.extract(member, root, filter="data")
            archive.unlink()
        (root / REGRESSIONS).mkdir(parents=True, exist_ok=True)
        (root / SCRIPTS).mkdir(parents=True, exist_ok=True)
        ops = Path("cs/ops/bin")
        if (workspace / ops).is_dir() and not any((workspace / path).is_symlink()
                                                   for path in (ops, *ops.parents)):
            # Python entry points resolve __file__. Keep those roots in the
            # overlay alongside the pinned regression files.
            shutil.copytree(workspace / ops, root / ops, symlinks=True,
                            ignore=shutil.ignore_patterns("__pycache__"))
        # Preserve the namespace even when a grader path is missing at base.
        for path in GRADER_PATHS[1:]:
            if not (root / path).exists():
                (root / path).symlink_to("/dev/null/grader-path-absent-at-base")
        host_only = {}
        for goal in sorted((root / REGRESSIONS).glob("*/goal.md")):
            parts = goal.read_text(encoding="utf-8").split("---\n", 2)
            marker = re.search(r"^host_only: ([^\n]+)$", parts[1], re.M) if len(parts) == 3 and not parts[0] else None
            if marker and (goal.parent / "check").is_file():
                name, reason = goal.parent.name, marker[1].strip()
                if not reason:
                    raise ValueError(f"empty host_only reason in {goal}")
                host_only[name] = reason
                check = goal.parent / "check"
                check.chmod(0o700)
                check.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(skip_message(name, reason)) + "\n")
        binaries = _link_release_binaries(workspace, root)
        _candidate_links(workspace, root, root / REGRESSIONS)
        directories = [root]
        for path in root.rglob("*"):
            if not path.is_symlink():
                if path.is_dir():
                    directories.append(path)
                path.chmod(path.stat().st_mode & ~0o222)
        root.chmod(0o500)
        try:
            yield Grader(root, host_only, base_sha, new_grader_paths, binaries)
        finally:
            for directory in directories:
                directory.chmod(0o700)
