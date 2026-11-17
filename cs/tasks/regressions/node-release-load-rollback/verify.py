#!/usr/bin/env python3
"""Service load failure rolls back current and restores the director database."""
import importlib.machinery
import importlib.util
import json
import os
import shutil
import sqlite3
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(next(p / "tasks/regressions/_lib" for p in Path(__file__).resolve().parents
                            if (p / "tasks/regressions/_lib").is_dir())))
from hermetic_env import hermetic_env

KIT = Path(__file__).resolve().parents[3]
RELEASE = KIT / "scripts/node-release"
OLD, NEW = "20990110-load-old", "20990111-load-new"
problems = []


def expect(cond, msg, detail=""):
    print(("ok   " if cond else "FAIL ") + msg + ("" if cond else "  " + str(detail)[:800]))
    cond or problems.append(msg)


def write_release(home, rid):
    rel = home / ".contextstack/releases" / rid
    kit = rel / "cs"
    (kit / "policy").mkdir(parents=True)
    (kit / "scripts").mkdir(parents=True)
    (kit / ".venv/bin").mkdir(parents=True)
    shutil.copy2(KIT / "policy/spool.json", kit / "policy/spool.json")
    env_src = KIT / "policy/env.json"
    if env_src.is_file():
        shutil.copy2(env_src, kit / "policy/env.json")
    (rel / "gateway").mkdir(parents=True, exist_ok=True)
    (rel / "gateway/models.json").write_text("{\"lab_executor\": \"/dev/null\"}\n")
    (rel / "bin").mkdir(parents=True)
    for name in ("cs-gateway", "cs-harness", "cs-director"):
        path = rel / "bin" / name
        path.write_text("#!/bin/sh\n")
        path.chmod(0o755)
    (kit / ".venv/bin/python3").write_text("#!/bin/sh\n")
    (kit / ".venv/bin/python3").chmod(0o755)
    (rel / ".complete").write_text("")


def seed_db(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.unlink()
    conn = sqlite3.connect(str(path))
    conn.execute("CREATE TABLE meta (value TEXT)")
    conn.execute("INSERT INTO meta VALUES (?)", (value,))
    conn.commit()
    conn.close()
    os.chmod(path, 0o600)


def db_marker(path):
    conn = sqlite3.connect(str(path))
    try:
        return conn.execute("SELECT value FROM meta").fetchone()[0]
    finally:
        conn.close()


def load_modules():
    sys.path.insert(0, str(KIT / "scripts"))
    loader = importlib.machinery.SourceFileLoader("node_release_load_rb", str(RELEASE))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    import _node_services as ns  # noqa: E402
    return mod, ns


with tempfile.TemporaryDirectory(prefix="node-release-load-") as raw:
    home = Path(raw)
    write_release(home, OLD)
    write_release(home, NEW)
    db = home / ".contextstack/state/director/director.sqlite"
    (home / ".contextstack/state/director").mkdir(parents=True, exist_ok=True)
    (home / ".contextstack/state/director").chmod(0o700)
    seed_db(db, "v1")
    env = {**hermetic_env(), "HOME": str(home), "NODE_INSTALL_PLATFORM": "macos", "NODE_INSTALL_USER_PATH": "/usr/bin:/bin"}
    env.pop("LAB_STATE_ROOT", None)
    env.pop("CSGATEWAY_ENDPOINT", None)
    os.environ.update(env)
    mod, ns = load_modules()
    mod.activate(OLD, {"serve_director_rs": True, "smoke": False}, load=False)
    seed_db(db, "v1")
    calls = {"n": 0}

    def flaky(label, path, changed, restart):
        if mod.current_id() != NEW:
            return "bootstrapped"
        calls["n"] += 1
        if calls["n"] >= 2:
            raise ns.Refused("fixture service loader failed")
        return "bootstrapped"

    refused = None
    with patch.object(mod, "unload_agent", lambda *a, **k: None), patch.object(mod, "reload_units", lambda: None),\
         patch.object(mod, "load_agent", side_effect=flaky):
        try:
            mod.activate(NEW, {"serve_director_rs": True, "smoke": False}, load=True)
        except mod.Refused as error:
            refused = str(error)
    expect(refused is not None, "load failure refuses", refused)
    expect(refused and "is back" in refused and "director database" in refused,
           "message says load failure rolled back including the database", refused)
    current = os.readlink(home / ".contextstack/current")
    expect(current.endswith(OLD), "current symlink returned to the previous release", current)
    expect(db_marker(db) == "v1", "director database restored after load failure")

if problems:
    print("node-release-load-rollback: %d failed" % len(problems))
    raise SystemExit(1)
print("node-release-load-rollback: ok")
