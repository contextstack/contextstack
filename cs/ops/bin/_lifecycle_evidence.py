#!/usr/bin/env python3
"""One verifier for a run's published evidence. Not an op.

`capture-stimulus`, `publish-evidence`, and pruning import this, so "is this evidence real?" has exactly one answer
in exactly one place.

A manifest is a set of claims. Verification means every claim is re-derived:
the stimulus it names exists and binds, the manifest hashes to its own
evidence_sha, and the diff blob is present and hashes to what was declared.
Checking that fields are non-empty, or that two strings match each other, is
what let a deleted blob and a fabricated hash pass.
"""
from _estate_io import sha256_bytes as _sha, digest as _digest
import hashlib
import json
import os
import gzip, io, tarfile, tempfile
from contextlib import contextmanager
from pathlib import Path

CONTRACT = "lifecycle-v1"
SKIP_DIRS = frozenset({
    ".git", "vendor", "node_modules", "cache", "caches", ".cache", "__pycache__",
})
SECRET_SUFFIXES = (".key", ".pem", ".p12", ".pfx")
SECRET_NAMES = frozenset({"id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"})


def skip_workspace_member(rel):
    """Secrets and caches never enter tree_hash or the inspection archive."""
    path = Path(rel)
    name = path.name.lower()
    if any(part in SKIP_DIRS or part.startswith(".cs-") or part.startswith(".env")
           for part in path.parts):
        return True
    if name.startswith(".env") or ".sqlite" in name:
        return True
    if name.endswith(SECRET_SUFFIXES) or name in SECRET_NAMES:
        return True
    return "cookie" in name or "-token." in name or "_token." in name or name.endswith(".token")


def verify(run_dir, require_ready=True):
    """(evidence, error). `evidence` is None whenever anything fails to verify."""
    run_dir = Path(run_dir)
    manifest = run_dir / "evidence/manifest.json"
    if not manifest.is_file():
        return None, "no published evidence for %s" % run_dir.name
    try:
        data = json.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        return None, "evidence for %s is unreadable: %s" % (run_dir.name, exc)
    if not isinstance(data, dict):
        return None, "evidence for %s is not an object" % run_dir.name

    # The stimulus is half of what evidence_sha binds. Without it the run has
    # no record of what it consumed, so its outputs mean nothing.
    stim = run_dir / "stimulus/manifest.json"
    if not stim.is_file():
        return None, "evidence for %s has no stimulus to bind to" % run_dir.name
    try:
        frozen = json.loads(stim.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        return None, "stimulus for %s is unreadable: %s" % (run_dir.name, exc)
    declared_stim = str(data.get("stimulus_sha") or "")
    if not declared_stim or declared_stim != str(frozen.get("stimulus_sha") or ""):
        return None, "evidence for %s does not bind its stimulus" % run_dir.name
    # Recompute the stimulus identity rather than trusting the number written
    # beside it: otherwise editing the frozen grant, task, or base leaves a
    # self-consistent-looking manifest that describes something else.
    if declared_stim != _digest({k: v for k, v in frozen.items()
                                 if k != "stimulus_sha"}):
        return None, "stimulus manifest for %s has been altered since capture" % run_dir.name
    # And the inputs it names must still be the bytes it recorded.
    for item in frozen.get("inputs") or []:
        rel = str(item.get("path") or "")
        blob = run_dir / "stimulus" / rel
        if not rel or not blob.is_file():
            return None, "frozen input %s is missing from %s" % (rel or "?", run_dir.name)
        if _sha(blob.read_bytes()) != str(item.get("sha256") or ""):
            return None, "frozen input %s in %s no longer matches its hash" % (rel, run_dir.name)

    # The manifest must hash to the identity it publishes.
    declared = str(data.get("evidence_sha") or "")
    recomputed = _digest({k: v for k, v in data.items() if k != "evidence_sha"})
    if not declared or declared != recomputed:
        return None, "evidence manifest for %s has been altered since publication" % run_dir.name

    if require_ready:
        if not (run_dir / "ready_for_inspection").is_file():
            return None, "evidence for %s is not ready for inspection (%s)" % (
                run_dir.name, "; ".join(data.get("incomplete_reasons") or ["incomplete"]))
        if data.get("capture_state") != "complete":
            return None, "evidence for %s is incomplete" % run_dir.name

    # The blob it points at must exist and be the blob it described.
    block = data.get("diff") or {}
    blob = run_dir / "evidence" / str(block.get("path") or "")
    if not blob.is_file():
        return None, "evidence for %s names a diff blob that is missing" % run_dir.name
    if str(block.get("sha256") or "") != _sha(blob.read_bytes()):
        return None, "evidence diff for %s does not match its recorded hash" % run_dir.name

    archive = data.get("candidate_archive")
    if archive is not None:
        blob = run_dir / "evidence" / str(archive.get("path") or "")
        if archive.get("path") != "candidate.tar.gz" or not blob.is_file() or _sha(blob.read_bytes()) != archive.get("sha256"):
            return None, "candidate archive is missing or altered"
        if not isinstance(archive.get("source_bytes"), int) or not 0 <= archive["source_bytes"] <= 256 << 20:
            return None, "candidate archive size is invalid"

    # A recorded transcript that has since vanished is not whole evidence.
    ref = data.get("transcript") or {}
    if isinstance(ref, dict) and ref.get("present"):
        tpath = run_dir / str(ref.get("path") or "transcript.md")
        if not tpath.is_file():
            return None, "evidence for %s records a transcript that is gone" % run_dir.name
        if ref.get("sha256") and _sha(tpath.read_bytes()) != str(ref["sha256"]):
            return None, "transcript for %s no longer matches its hash" % run_dir.name

    # The evidence must be about this run, not some other one.
    named = str(data.get("run_id") or "")
    if named and named != run_dir.name:
        return None, "evidence in %s claims to describe run %s" % (run_dir.name, named)
    return data, None


VERIFIED = "verified"
INCOMPLETE = "incomplete"
MISSING = "missing"
UNVERIFIABLE = "unverifiable"


def state(run_dir):
    """(status, detail, evidence) — one answer for display and for gating.

    Inspection renders every status; admission, finalization, scoring
    and pruning require VERIFIED. Nothing may re-derive its own opinion.
    """
    run_dir = Path(run_dir)
    if not (run_dir / "evidence/manifest.json").is_file():
        if (run_dir / "stimulus/manifest.json").is_file():
            return MISSING, "captured but never published", None
        return MISSING, "no evidence published", None
    data, error = verify(run_dir, require_ready=False)
    if error:
        return UNVERIFIABLE, error, None
    ready, why = _readiness(run_dir, data)
    if not ready:
        return INCOMPLETE, why, data
    return VERIFIED, "", data


def _readiness(run_dir, data):
    if not (run_dir / "ready_for_inspection").is_file():
        return False, "; ".join(data.get("incomplete_reasons") or ["not ready"])
    if data.get("capture_state") != "complete":
        return False, "capture is incomplete"
    return True, ""


def require(run_dir):
    """(evidence, error) for a boundary that ACTS. VERIFIED or nothing."""
    status, detail, data = state(run_dir)
    if status != VERIFIED:
        return None, "evidence for %s is %s: %s" % (Path(run_dir).name, status, detail)
    return data, None


def task_sha(task):
    p = Path(task) if task else None
    if not p or not p.is_dir():
        return ""
    h = hashlib.sha256()
    for f in sorted(p.rglob("*")):
        if not f.is_file() or f.name == "notes.md" or f.parent.name == "solution":
            continue
        h.update(f.read_bytes())
    return h.hexdigest()


def read_gzip(blob, declared_size):
    """A compressed snapshot is bounded by its recorded size and the envelope."""
    if type(declared_size) is not int or not 0 <= declared_size <= 256 << 20:
        raise ValueError("archive source_bytes is outside the evidence envelope")
    with gzip.open(blob, "rb") as source:
        raw = source.read(declared_size + 1)
    if len(raw) != declared_size:
        raise ValueError("archive differs from its declared size")
    return raw


def workspace_members(ws):
    """Files and symlinks a prune reconstruct must restore. Never follow links."""
    root = Path(ws)
    for path in sorted(root.rglob("*")):
        if not (path.is_file() or path.is_symlink()):
            continue
        rel = path.relative_to(root)
        if skip_workspace_member(rel):
            continue
        yield path, rel.as_posix()


def _add_member(archive, path, rel):
    info = archive.gettarinfo(str(path), arcname=rel)
    if path.is_symlink():
        info.type = tarfile.SYMTYPE
        info.linkname = os.readlink(path)
        info.size = 0
        archive.addfile(info)
        return
    raw = path.read_bytes()
    info.size = len(raw)
    info.mode = path.stat().st_mode & 0o777
    archive.addfile(info, io.BytesIO(raw))


def pack_workspace(ws):
    """Inspection tar of the live walk: untracked, ignored, modes, symlinks."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        for path, rel in workspace_members(ws):
            _add_member(archive, path, rel)
    return buffer.getvalue()


def pack_workspace_paths(ws, paths):
    """Inspection tar of named live paths only (envelope fallback)."""
    root = Path(ws)
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        for rel in paths:
            if skip_workspace_member(rel):
                continue
            path = root / rel
            if path.is_file() or path.is_symlink():
                _add_member(archive, path, rel)
    return buffer.getvalue()


@contextmanager
def inspection_workspace(run_dir, evidence, *, evidence_dir=None):
    """An isolated view of published bytes; links must resolve inside it."""
    archive = evidence.get("candidate_archive") or {}
    if not archive:
        raise ValueError("historical evidence has no frozen inspection archive")
    raw = read_gzip((evidence_dir or Path(run_dir) / "evidence") / archive["path"], archive["source_bytes"])
    with tempfile.TemporaryDirectory(prefix="candidate-inspection-") as tmp:
        root = Path(tmp).resolve()
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as bundle:
            seen = set()
            links = []
            for entry in bundle:
                relative = Path(entry.name)
                if relative.is_absolute() or ".." in relative.parts or relative.as_posix() in seen:
                    raise ValueError("unsafe or duplicate candidate archive path")
                seen.add(relative.as_posix())
                if entry.size < 0 or entry.size > len(raw):
                    raise ValueError("candidate member size exceeds archive")
                if entry.issym():
                    links.append((relative, entry.linkname))
                elif not entry.isdir() and not entry.isfile():
                    raise ValueError("unsupported special file in candidate archive")
                if not entry.isfile():
                    continue
                target = root / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                with bundle.extractfile(entry) as source:
                    target.write_bytes(source.read())
                target.chmod(0o500 if entry.mode & 0o111 else 0o400)
        for relative, target in links:
            path = root / relative
            if Path(target).is_absolute() or not (path.parent / target).resolve().is_relative_to(root):
                raise ValueError("candidate symlink escapes archive")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.symlink_to(target)
        for relative, _ in links:
            if not (root / relative).resolve().is_relative_to(root):
                raise ValueError("candidate symlink chain escapes archive")
        yield root
