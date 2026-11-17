"""Hash-check explicit operator inputs before staging them into a CLI workspace."""
from __future__ import annotations

import hashlib
import os
import tempfile
from pathlib import Path


MAX_INPUTS = 12
MAX_BYTES = 4 * 1024 * 1024


def stage_frozen_inputs(items, workspace):
    if items is None:
        return []
    if not isinstance(items, list) or len(items) > MAX_INPUTS:
        raise ValueError("inputs must be an array of at most %d items" % MAX_INPUTS)
    root = Path(workspace).resolve()
    staged, destinations = [], set()
    for item in items:
        if not isinstance(item, dict) or set(item) != {"source", "dest", "sha256"}:
            raise ValueError("each frozen input needs exactly source, dest, and sha256")
        source = Path(str(item["source"])).expanduser()
        if not source.is_absolute() or source.is_symlink() or not source.is_file():
            raise ValueError("frozen input source must be an absolute regular file")
        dest = Path(str(item["dest"]))
        if (dest.is_absolute() or not dest.parts or ".." in dest.parts
                or dest.parts[0] == ".git" or any(part.startswith(".lab-") for part in dest.parts)
                or str(dest) in destinations):
            raise ValueError("frozen input destination is unsafe or duplicate")
        expected = str(item["sha256"])
        if len(expected) != 64 or any(char not in "0123456789abcdef" for char in expected):
            raise ValueError("frozen input sha256 is invalid")
        raw = source.read_bytes()
        if not raw or len(raw) > MAX_BYTES:
            raise ValueError("frozen input is empty or exceeds %d bytes" % MAX_BYTES)
        actual = hashlib.sha256(raw).hexdigest()
        if actual != expected:
            raise ValueError("frozen input hash differs for " + str(source))
        target = root / dest
        target.parent.mkdir(parents=True, exist_ok=True)
        if (target.is_symlink() or target.exists()
                or not target.parent.resolve().is_relative_to(root)):
            raise ValueError("frozen input destination already exists or escapes workspace")
        handle = tempfile.NamedTemporaryFile(prefix="frozen-", dir=target.parent, delete=False)
        try:
            handle.write(raw); handle.flush(); os.fsync(handle.fileno()); handle.close()
            Path(handle.name).replace(target)
        finally:
            try:
                Path(handle.name).unlink(missing_ok=True)
            except OSError:
                pass
        destinations.add(str(dest))
        staged.append({"source": str(source), "dest": str(dest),
                       "sha256": actual, "bytes": len(raw)})
    return staged
