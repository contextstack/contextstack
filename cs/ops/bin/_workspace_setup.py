#!/usr/bin/env python3
"""Workspace notes metadata and lock-tagged dependency install cache."""
import hashlib
import os
import shutil
import stat
import subprocess
import sys
from pathlib import Path

from _estate_io import state_dir

INSTALLS = ("vendor", "node_modules")
LOCKS = ("composer.lock", "package-lock.json", "pnpm-lock.yaml", "yarn.lock")


def metadata(task):
    path = Path(task) / "notes.md"
    out = {}
    if not path.is_file():
        return out
    for line in path.read_text(encoding="utf-8").splitlines():
        if ":" not in line or line[:1].isspace():
            continue
        key, value = line.split(":", 1)
        if key.replace("_", "").isalnum():
            out[key] = value.strip()
    return out


def patterns(task):
    raw = metadata(task).get("writes", "")
    return [item.strip() for item in raw.split(",") if item.strip()]


def install_tag(root):
    digest = hashlib.sha256()
    found = False
    for name in LOCKS:
        path = Path(root) / name
        if path.is_file():
            digest.update(name.encode() + b"\0" + path.read_bytes())
            found = True
    return digest.hexdigest()[:16] if found else Path(root).name


def freeze_tree(root):
    root = Path(root)
    paths = [*sorted(root.rglob("*"), key=lambda p: len(p.parts), reverse=True), root]
    for path in paths:
        if path.is_symlink() or not path.exists():
            continue
        path.chmod(stat.S_IMODE(path.stat().st_mode) & ~0o222)


def thaw_tree(root):
    """Workspace clone of a frozen cache is writable; writes are copy-on-write."""
    root = Path(root)
    for path in (root, *root.rglob("*")):
        if path.is_symlink() or not path.exists():
            continue
        path.chmod(stat.S_IMODE(path.stat().st_mode) | 0o222)


def clone_tree(src, dst):
    """APFS clone: real paths, no extra blocks until written."""
    src, dst = Path(src), Path(dst)
    dst.parent.mkdir(parents=True, exist_ok=True)
    copied = subprocess.run(["cp", "-c", "-R", str(src), str(dst)],
                            capture_output=True, text=True)
    if copied.returncode != 0:
        raise RuntimeError((copied.stderr or copied.stdout or "cp -c -R failed").strip())


def _copy_install(src, dst):
    dst.parent.mkdir(parents=True, exist_ok=True)
    try:
        clone_tree(src, dst)
        return
    except RuntimeError:
        if sys.platform == "darwin":
            raise
    shutil.copytree(src, dst, symlinks=True)


def seed_installs(source, lab):
    """Copy dest vendor/node_modules into a frozen lock-hash cache."""
    tag = install_tag(source)
    cache = state_dir(lab) / "setups/installs" / tag
    for name in INSTALLS:
        src, dst = Path(source) / name, cache / name
        if not src.is_dir():
            continue
        if dst.is_symlink() or (dst.exists() and os.access(dst, os.W_OK)):
            if dst.is_dir() and not dst.is_symlink():
                shutil.rmtree(dst)
            else:
                dst.unlink()
        if dst.exists():
            continue
        staging = cache / (name + ".staging")
        if staging.exists() or staging.is_symlink():
            if staging.is_dir() and not staging.is_symlink():
                shutil.rmtree(staging)
            else:
                staging.unlink()
        _copy_install(src, staging)
        freeze_tree(staging)
        staging.rename(dst)
    return tag
