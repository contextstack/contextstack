"""Per-machine pins for profiled CLI binaries (Demanded-by: cli-pins-per-machine).

The repo holds paths and launch profiles; each machine records sha256 and --version output under
<config>/cli-pins.json. First use pins; later jobs refuse when the binary changes until `cs doctor --repin`.
"""
import hashlib
import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path

from _estate_io import write_atomic as estate_write_atomic
from _roots import roots

PINS_NAME = "cli-pins.json"
SHA_RE = re.compile(r"^[0-9a-f]{64}$")
VERSION_RE = re.compile(r"v?(\d+)\.(\d+)\.(\d+)")


def pins_path(env=None):
    return roots(env=env)["config"] / PINS_NAME


def load_pins(env=None):
    path = pins_path(env)
    if not path.is_file():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return {}
    return data if isinstance(data, dict) else {}


def save_pins(pins, env=None):
    path = pins_path(env)
    estate_write_atomic(path, (json.dumps(pins, indent=2, sort_keys=True) + "\n").encode("utf-8"), mode=0o600)


def file_sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def package_sha256(folder):
    digest = hashlib.sha256()
    root = Path(folder)
    for path in sorted(root.rglob("*")):
        if path.is_file():
            digest.update(str(path.relative_to(root)).encode() + b"\0" + bytes.fromhex(file_sha256(path)))
    return digest.hexdigest()


def resolve_binary(raw, real_home):
    binary = real_home / raw[2:] if isinstance(raw, str) and raw.startswith("~/") else Path(raw)
    if not binary.is_absolute() or binary.is_symlink() or not binary.is_file():
        raise ValueError("CLI binary missing or not a regular file: %s" % raw)
    return binary


def version_tuple(text):
    match = VERSION_RE.search(text or "")
    return tuple(int(x) for x in match.groups()) if match else None


def min_version_ok(min_version, actual):
    if not min_version:
        return True
    want, got = version_tuple(min_version), version_tuple(actual)
    if want is None or got is None:
        return (actual or "").strip() == (min_version or "").strip()
    return got >= want


def changed_message(template, old_version, new_version):
    return ("%s: the installed CLI changed since it was pinned (%s -> %s); "
            "run `cs doctor --repin %s` after checking the new version"
            % (template, old_version or "?", new_version or "?", template))


def ensure_pin(template, binary, version, *, env=None, repin=False, package_sha=None):
    """Record or verify a machine pin. Raises ValueError when the binary changed since the pin."""
    digest = file_sha256(binary)
    pins = load_pins(env)
    entry = pins.get(template)
    if repin or entry is None:
        record = {
            "binary_path": str(binary),
            "version": (version or "").strip(),
            "sha256": digest,
            "pinned_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        }
        if package_sha is not None:
            record["package_sha256"] = package_sha
        pins[template] = record
        save_pins(pins, env)
        return record
    if not SHA_RE.fullmatch(str(entry.get("sha256") or "")) or entry["sha256"] != digest:
        raise ValueError(changed_message(template, entry.get("version"), version))
    if package_sha is not None and entry.get("package_sha256") != package_sha:
        raise ValueError("%s: the installed CLI runtime package changed since it was pinned; "
                         "run `cs doctor --repin %s` after checking the new package"
                         % (template, template))
    return entry


def pin_state(template, binary, env=None):
    """ok, changed, or unpinned — for doctor (hash only; version text is not re-probed)."""
    pins = load_pins(env)
    entry = pins.get(template)
    if entry is None:
        return "unpinned", None
    try:
        digest = file_sha256(binary)
    except OSError:
        return "missing", entry
    if not SHA_RE.fullmatch(str(entry.get("sha256") or "")) or entry["sha256"] != digest:
        return "changed", entry
    return "ok", entry


def profiled_templates(folder):
    """Executor template names that use cli-profiles.json or muse-profile.json."""
    folder = Path(folder)
    templates = json.loads((folder / "executor-templates.json").read_text(encoding="utf-8"))
    profiles = json.loads((folder / "cli-profiles.json").read_text(encoding="utf-8"))
    out = []
    for name, tmpl in templates.items():
        if tmpl.get("profile") == "cli-profiles.json" and name in profiles:
            out.append((name, profiles[name]))
        elif name == "muse-code" and (folder / "muse-profile.json").is_file():
            out.append((name, json.loads((folder / "muse-profile.json").read_text(encoding="utf-8"))))
    return out


def repin_template(template, folder, real_home, version, *, env=None, package_sha=None):
    profile = dict(next((p for n, p in profiled_templates(folder) if n == template), None) or ())
    if not profile:
        raise ValueError("unknown profiled template: %s" % template)
    binary = resolve_binary(profile["binary"], real_home)
    return ensure_pin(template, binary, version, env=env, repin=True, package_sha=package_sha)
