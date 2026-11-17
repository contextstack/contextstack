"""Pinned native CLI profiles."""
import json
import os
import subprocess
from pathlib import Path


def stage(ws, profile, home_name="cli-home"):
    ws = ws.resolve()
    home = ws / ".lab-tmp" / home_name
    if home.exists() or home.is_symlink():
        raise ValueError("cli-home exists or is a symlink")
    rhome = home.resolve()
    if not rhome.is_relative_to(ws):
        raise ValueError("cli-home resolves outside workspace")
    home.mkdir(parents=True)
    home.joinpath("config").mkdir()
    home.joinpath("data").mkdir()
    home.joinpath("cache").mkdir()
    home.joinpath("tmp").mkdir()

    env = {
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(home / "config"),
        "XDG_DATA_HOME": str(home / "data"),
        "XDG_CACHE_HOME": str(home / "cache"),
        "TMPDIR": str(home / "tmp"),
        "TMP": str(home / "tmp"),
        "TEMP": str(home / "tmp"),
    }

    for key, val in profile.get("environment", {}).items():
        if key in {"HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "TMPDIR", "TMP", "TEMP"}:
            raise ValueError(f"environment override disallowed for {key}")
        env[key] = val.replace("{{home}}", str(home))

    protected_paths = []
    for rel, content in profile.get("files", {}).items():
        if not rel or rel.startswith("/") or rel == "." or ".." in Path(rel).parts:
            raise ValueError(f"invalid path segment in {rel}")
        dest = home / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        if not dest.resolve().is_relative_to(home):
            raise ValueError(f"path escapes home {rel}")
        if isinstance(content, (dict, list)):
            text = json.dumps(content, sort_keys=True) + "\n"
        else:
            text = content
        dest.write_text(text)
        dest.chmod(0o400)
        protected_paths.append(dest)

    return env, protected_paths


# stage_git lives in _cli_native (frozen executor copies always include it).
from _cli_native import stage_git  # noqa: E402,F401
