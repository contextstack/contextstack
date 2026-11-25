"""Materialize a run workspace: git worktree, sparse-checkout, tagged installs."""
import os
import shutil
from pathlib import Path

from _estate_io import state_dir
from _harvest_git import host_git as git

from _workspace_setup import (INSTALLS, clone_tree, install_tag, patterns,
                               seed_installs, thaw_tree)

HERE = Path(__file__).resolve().parent
CODE_ROOT = HERE.parent.parent
FRAMEWORK = ("app", "bootstrap", "config", "database", "routes", "tests", "resources", "storage", "public")
RUNTIME_DIRS = (
    "storage/framework/sessions",
    "storage/framework/views",
    "storage/framework/cache/data",
    "storage/logs",
    "bootstrap/cache",
)


def clone_installs(workspace, setup, code_root=CODE_ROOT):
    tag = seed_installs(setup, code_root)
    cache = state_dir(code_root) / "setups/installs" / tag
    for name in INSTALLS:
        dest = Path(workspace) / name
        if dest.exists() or dest.is_symlink():
            continue
        source = cache / name
        if not source.exists() and (Path(setup) / name).is_dir():
            seed_installs(setup, code_root)
        if source.exists() or source.is_symlink():
            clone_tree(source, dest)
            thaw_tree(dest)


def cone_dirs(task, setup):
    writes = patterns(task) if task else []
    if not writes or "." in writes:
        return []
    dirs = []
    for item in writes:
        path = item.replace("/**", "").replace("/*", "").rstrip("/")
        if path and path != "." and "*" not in path:
            dirs.append(path)
    if (Path(setup) / "composer.json").is_file() or (Path(setup) / "artisan").is_file():
        dirs.extend(name for name in FRAMEWORK if (Path(setup) / name).exists())
    return sorted(set(dirs))


def _copy_tree(setup, workspace):
    shutil.copytree(setup, workspace, dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns(*INSTALLS, ".git"),
                    symlinks=True)


def materialize(setup, workspace, task=None, code_root=CODE_ROOT, env=None):
    setup, workspace = Path(setup).resolve(), Path(workspace)
    if workspace.exists():
        if any(workspace.iterdir()):
            raise ValueError("workspace is not empty")
        workspace.rmdir()
    workspace.parent.mkdir(parents=True, exist_ok=True)
    # An ordinary setup directory must never discover a containing checkout.
    rev = git(setup, "rev-parse", "refs/cs/base", env=env) if (setup / ".git").exists() else None
    if rev is not None and rev.returncode:
        rev = git(setup, "rev-parse", "HEAD", env=env)
    commit = rev.stdout.strip() if rev is not None else ""
    added = git(setup, "worktree", "add", "--detach", "--no-checkout",
                str(workspace), commit, env=env) if commit else None
    if not added or added.returncode:
        workspace.mkdir(parents=True, exist_ok=True)
        _copy_tree(setup, workspace)
    else:
        dirs = cone_dirs(task, setup)
        if dirs:
            git(workspace, "sparse-checkout", "init", "--cone", env=env)
            git(workspace, "sparse-checkout", "set", "--cone", *dirs, env=env)
        checked = git(workspace, "checkout", "--force", commit, env=env)
        if checked.returncode:
            raise ValueError((checked.stderr or checked.stdout or "checkout failed").strip())
    from _harvest_git import initialize
    host_repo = initialize(workspace, setup if commit else None, commit)
    # Keep sparse exclusions chosen by the host, before the agent can edit them.
    if added and added.returncode == 0:
        admin = git(workspace, "rev-parse", "--absolute-git-dir", env=env).stdout.strip()
        sparse = Path(admin) / "info/sparse-checkout"
        if sparse.is_file():
            shutil.copyfile(sparse, host_repo / "info/sparse-checkout")
            from _harvest_git import git as harvest_git
            harvest_git(workspace, "config", "core.sparseCheckout", "true")
            harvest_git(workspace, "read-tree", "-m", commit)
    clone_installs(workspace, setup, code_root)
    if (workspace / "artisan").is_file() or (workspace / "composer.json").is_file():
        for rel in RUNTIME_DIRS:
            (workspace / rel).mkdir(parents=True, exist_ok=True)
        src = setup / "public"
        if src.is_dir():
            shutil.copytree(src, workspace / "public", dirs_exist_ok=True, symlinks=True)
    return {"ok": True, "workspace": str(workspace), "setup": str(setup),
            "tag": install_tag(setup)}


def forget_worktree(workspace, env=None):
    workspace = Path(workspace)
    if (workspace / ".git").is_file():
        git(workspace, "worktree", "remove", "--force", str(workspace), env=env)
        git(workspace, "worktree", "prune", env=env)
    if workspace.exists():
        shutil.rmtree(workspace, ignore_errors=True)
