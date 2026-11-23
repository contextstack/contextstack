#!/usr/bin/env python3

"""Smoke-failure rollback restores the director database snapshot."""
import json
import os
import select
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
INSTALL = KIT / "scripts/node-install"
SCRIPTS = KIT / "scripts"
OLD, NEW = "20990110-db-old", "20990111-db-new"
problems = []


def expect(cond, msg, detail=""):
    print(("ok   " if cond else "FAIL ") + msg + ("" if cond else "  " + str(detail)[:800]))
    cond or problems.append(msg)


def write_release(home, rid, schema, fail_smoke=False):
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
    (rel / "gateway/models.json").write_text("{\"cli_executor\": \"/dev/null\"}\n")
    (rel / "bin").mkdir(parents=True)
    for name in ("cs-gateway", "cs-harness", "cs-director"):
        p = rel / "bin" / name
        p.write_text(f"#!/bin/sh\necho {schema}\n" if name == "cs-director" else "#!/bin/sh\n")
        p.chmod(0o755)
    (kit / ".venv/bin/python3").symlink_to(sys.executable)
    if fail_smoke:
        smoke = kit / "scripts/cs"
        smoke.write_text("""import json, os, sqlite3
from pathlib import Path
db = Path.home() / ".contextstack/state/director/director.sqlite"
with sqlite3.connect(db) as conn:
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA user_version=7")
    conn.execute("UPDATE meta SET value='v2-migrated'")
if os.environ.get("MISMATCH_SNAPSHOT"):
    snapshot, = db.parent.parent.glob("director-snapshots/director.sqlite.pre-*")
    with sqlite3.connect(snapshot) as conn:
        conn.execute("PRAGMA user_version=5")
print(json.dumps({"ok": False, "checks": [{"name": "fixture", "ok": False, "reason": "forced"}]}))
""")
        smoke.chmod(0o755)
        (kit / "scripts/cs_smoke.py").write_text("# present\n")
    (rel / ".complete").write_text("")


def seed_db(path, value, schema=6):
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.unlink()
    conn = sqlite3.connect(str(path))
    conn.execute(f"PRAGMA user_version={schema}")
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
    (shim / "launchctl").write_text('#!/bin/sh\n[ "$1" = print ] && exit 113\nexit 0\n')
    (shim / "launchctl").chmod(0o755)
    write_release(home, OLD, 6)
    write_release(home, NEW, 7, fail_smoke=True)
    db = home / ".contextstack/state/director/director.sqlite"
    seed_db(db, "v1")
    (home / ".contextstack/state/director").chmod(0o700)
    env = {**hermetic_env(), "HOME": str(home), "NODE_INSTALL_PLATFORM": "macos",
           "PATH": "%s:/usr/bin:/bin" % shim, "NODE_INSTALL_USER_PATH": "/usr/bin:/bin"}
    env.pop("CS_STATE_ROOT", None)
    env.pop("CSGATEWAY_ENDPOINT", None)
    sys.path.insert(0, str(SCRIPTS))
    os.environ.update(env)
    from _node_layout import director_db_schema, switch_current  # noqa: E402

    def release(*args, **overrides):
        return subprocess.run([sys.executable, "-I", str(RELEASE), *args],
                              capture_output=True, text=True, env={**env, **overrides}, timeout=30)

    r1 = release("activate", OLD, "--no-load", "--serve-director-rs")
    expect(r1.returncode == 0, "schema-6 release activates on schema 6", r1.stdout + r1.stderr)

    root = home / ".contextstack"
    bundle = home / "bundle"
    (bundle / "uv").mkdir(parents=True)
    (bundle / "uv/uv").write_text("#!/bin/sh\nexit 0\n")
    (bundle / "uv/uv").chmod(0o755)
    (bundle / "release/cs").mkdir(parents=True)
    system = "macos" if sys.platform == "darwin" else "linux"
    arch = "aarch64" if os.uname().machine in ("arm64", "aarch64") else "x86_64"
    (bundle / "bundle.json").write_text(json.dumps({"target": f"{system}-{arch}", "release": NEW}, indent=2))
    # Exercise the native bootstrap when the full check has built it.
    native = KIT.parent / "target/release/cs-tools"
    if native.is_file():
        (bundle / "release/bin").mkdir()
        (bundle / "release/bin/cs-tools").symlink_to(native)
    install = ["sh", str(INSTALL), "install", str(bundle)]
    dispatch = root / "state/dispatch.jsonl"
    dispatch.write_text('{"id":"fixture","state":"pending"}\n')
    with subprocess.Popen([*install, "--wait", "15"], env=env, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True) as first:
        try:
            ready, _, _ = select.select([first.stderr], [], [], 5)
            line = first.stderr.readline() if ready else ""
            assert "waiting for active jobs" in line, line
            second = subprocess.run(install, env=env, capture_output=True, text=True, timeout=5)
            expect(second.returncode == 1 and f"pid {first.pid} (node-install install)" in second.stderr,
                   "concurrent install refuses and names the holder", second.stdout + second.stderr)
            for args in (("switch", NEW), ("select",), ("rollback",)):
                blocked = subprocess.run(["sh", str(INSTALL), *args], env=env,
                                         capture_output=True, text=True, timeout=5)
                expect(blocked.returncode == 1 and f"pid {first.pid} (node-install install)" in blocked.stderr,
                       f"{args[0]} shares the install lock", blocked.stdout + blocked.stderr)
            switch_current(NEW)  # Simulate an older installer or a manual symlink change.
            dispatch.write_text("")
            stdout, stderr = first.communicate(timeout=10)
            expect(first.returncode == 1 and "current changed" in stdout and (root / "current").resolve().name == NEW,
                   "install refuses activation when current changes during drain", stdout + stderr)
        finally:
            if first.poll() is None:
                first.kill()
    switch_current(OLD)

    r2 = release("activate", NEW)
    expect(r2.returncode == 1 and "director database restored" in r2.stdout,
           "smoke failure rolls back to the old release", r2.stdout + r2.stderr)
    expect(db_marker(db) == "v1" and director_db_schema() == 6 and (root / "current").resolve().name == OLD,
           "rollback restored the pre-migration database and schema")

    expect(release("switch", NEW, "--no-load").returncode == 0, "new release switches on schema 6")
    seed_db(db, "v2-migrated", schema=7)
    # A successful install normally clears the trial failure before entering history.
    record_path = root / "config/install.json"
    record = json.loads(record_path.read_text())
    record.update(history=[OLD, NEW], failed_releases=[])
    record_path.write_text(json.dumps(record))
    for args in (("rollback",), ("switch", OLD)):
        refused = release(*args, "--no-load")
        expect(refused.returncode == 1 and "schema 6" in refused.stdout and "schema 7" in refused.stdout
               and (root / "current").resolve().name == NEW,
               f"{args[0]} to an older Director refuses on schema 7", refused.stdout + refused.stderr)

    snaps = list((root / "state/director-snapshots").glob("director.sqlite.pre-*"))
    expect(1 <= len(snaps) <= 2, "the pre-migration database snapshot is retained", [p.name for p in snaps])

    seed_db(db, "v1")
    expect(release("switch", OLD, "--no-load").returncode == 0, "old release can switch back on schema 6")
    r3 = release("activate", NEW, MISMATCH_SNAPSHOT="1")
    expect(r3.returncode == 1 and "snapshot schema 5 does not match" in r3.stdout
           and (root / "current").resolve().name == NEW and director_db_schema() == 7
           and db_marker(db) == "v2-migrated",
           "mismatched snapshot leaves the schema-compatible trial active", r3.stdout + r3.stderr)
    expect(NEW in json.loads(record_path.read_text())["failed_releases"],
           "the retained smoke-failed release is excluded from rollback history")

if problems:
    print("node-release-db-rollback: %d failed" % len(problems))
    raise SystemExit(1)
print("node-release-db-rollback: ok")
