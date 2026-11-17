#!/usr/bin/env python3

"""Smoke-failure rollback restores the director database snapshot."""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(next(p / "tasks/regressions/_lib" for p in Path(__file__).resolve().parents
                            if (p / "tasks/regressions/_lib").is_dir())))
from hermetic_env import hermetic_env

KIT = Path(__file__).resolve().parents[3]
RELEASE = KIT / "scripts/node-release"
SCRIPTS = KIT / "scripts"
OLD, NEW = "20990110-db-old", "20990111-db-new"
problems = []


def expect(cond, msg, detail=""):
    print(("ok   " if cond else "FAIL ") + msg + ("" if cond else "  " + str(detail)[:800]))
    cond or problems.append(msg)


def write_release(home, rid, fail_smoke):
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
        p = rel / "bin" / name
        p.write_text("#!/bin/sh\n")
        p.chmod(0o755)
    (kit / ".venv/bin/python3").write_text("#!/bin/sh\necho python3\n")
    (kit / ".venv/bin/python3").chmod(0o755)
    if fail_smoke:
        smoke = kit / "scripts/cs"
        smoke.write_text("#!/usr/bin/env python3\nimport json\nprint(json.dumps({\"ok\": False, \"checks\": [{\"name\": \"fixture\", \"ok\": False, \"reason\": \"forced\"}]}))\n")
        smoke.chmod(0o755)
        (kit / "scripts/cs_smoke.py").write_text("# present\n")
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


with tempfile.TemporaryDirectory(prefix="node-release-db-") as raw:
    home = Path(raw)
    shim = home / "shim"
    shim.mkdir()
    (shim / "launchctl").write_text("#!/bin/sh\nexit 0\n")
    (shim / "launchctl").chmod(0o755)
    write_release(home, OLD, False)
    write_release(home, NEW, True)
    db = home / ".contextstack/state/director/director.sqlite"
    seed_db(db, "v1")
    (home / ".contextstack/state/director").chmod(0o700)
    env = {**hermetic_env(), "HOME": str(home), "NODE_INSTALL_PLATFORM": "macos",
           "PATH": "%s:/usr/bin:/bin" % shim, "NODE_INSTALL_USER_PATH": "/usr/bin:/bin"}
    env.pop("LAB_STATE_ROOT", None)
    env.pop("CSGATEWAY_ENDPOINT", None)
    sys.path.insert(0, str(SCRIPTS))
    os.environ.update(env)
    from _node_layout import restore_director_db, snapshot_director_db  # noqa: E402

    snapshot_director_db(NEW)
    seed_db(db, "v2-migrated")
    expect(db_marker(db) == "v2-migrated", "migration marker planted")
    expect(restore_director_db(NEW) and db_marker(db) == "v1", "restore_director_db brings back the snapshot")

    r1 = subprocess.run([sys.executable, "-I", str(RELEASE), "activate", OLD, "--no-load", "--serve-director-rs"],
                        capture_output=True, text=True, env=env)
    expect(r1.returncode == 0, "old release activates", (r1.stdout or r1.stderr)[-300:])
    seed_db(db, "v1")
    r2 = subprocess.run([sys.executable, "-I", str(RELEASE), "activate", NEW, "--serve-director-rs"],
                        capture_output=True, text=True, env=env)
    expect(r2.returncode == 1, "smoke failure refuses", (r2.stdout or r2.stderr)[-400:])
    expect(db_marker(db) == "v1", "smoke rollback restored the pre-upgrade database")
    snaps = list((home / ".contextstack/state/director-snapshots").glob("director.sqlite.pre-*"))
    expect(1 <= len(snaps) <= 2, "only the newest two director snapshots are kept", [p.name for p in snaps])

if problems:
    print("node-release-db-rollback: %d failed" % len(problems))
    raise SystemExit(1)
print("node-release-db-rollback: ok")
