#!/usr/bin/env python3
"""Service load failure rolls back current and restores the director database."""
from contextlib import redirect_stderr
import importlib.machinery
import importlib.util
import io
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
    (rel / "gateway/models.json").write_text("{\"cli_executor\": \"/dev/null\"}\n")
    (rel / "bin").mkdir(parents=True)
    for name in ("cs-gateway", "cs-harness", "cs-director"):
        path = rel / "bin" / name
        path.write_text("#!/bin/sh\necho 0\n" if name == "cs-director" else "#!/bin/sh\n")
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
    env.pop("CS_STATE_ROOT", None)
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

    derive = mod.derive_models

    def broken_models(rid):
        if rid == NEW:
            raise OSError("fixture models write failed")
        return derive(rid)

    with patch.object(mod, "derive_models", side_effect=broken_models):
        try:
            mod.activate(NEW, {}, load=False)
        except (mod.Refused, OSError) as error:
            expect("fixture models write failed" in str(error), "activation reports its write failure")
    expect(mod.current_id() == OLD, "models write failure restores current")

    scripts = mod.RELEASES / NEW / "cs/scripts"
    (scripts / "cs_smoke.py").touch()
    def fail_check(name):
        report = {"ok": False, "checks": [{"name": name, "ok": False, "reason": "forced failure"}]}
        (scripts / "cs").write_text(f"""import sqlite3
with sqlite3.connect({str(db)!r}) as conn:
    conn.execute("UPDATE meta SET value='written during smoke'")
print({json.dumps(report)!r})
raise SystemExit(1)
""")

    with patch.object(mod, "unload_agent"), patch.object(mod, "load_agent", return_value="bootstrapped"):
        for name, default in (("claude-code-sonnet", {"default_agent": "claude"}),
                              ("claude-code-sonnet", {"default_model": "claude-code"}),
                              ("hosted-lead", {}), ("services", {})):
            fail_check(name)
            refused = None
            try:
                mod.activate(NEW, {"smoke": True, **default}, load=True)
            except mod.Refused as error:
                refused = str(error)
            expect(refused and f"{name}: forced failure" in refused, f"{name} failure refuses activation", refused)
            record = mod.load_record()
            expect(mod.current_id() == OLD, f"{name} failure restores current")
            expect(db_marker(db) == "v1", f"{name} failure restores the database")
            expect(NEW not in record["history"] and record["previous"] != NEW,
                   "failed smoke is excluded from rollback history")
            expect(NEW in record.get("failed_releases", []), "smoke failure is recorded")
            expect(not mod.rollback_target(record, OLD), "rollback cannot choose the failed release")

        fail_check("grok")
        limits = mod.ROOT / "state/provider-limits.json"
        for has_limits in (False, True):
            if has_limits:
                limits.write_text('{"templates": {}}')
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                out = mod.activate(NEW, {"smoke": True, "default_agent": "codex"}, load=True)
            expect(out["ok"] and out["warnings"] == [{"name": "grok", "reason": "forced failure"}],
                   "non-default failure succeeds with a JSON warning", out)
            expect("warning: grok: forced failure" in stderr.getvalue(), "non-default failure prints a warning")
            expect(mod.current_id() == NEW and db_marker(db) == "written during smoke",
                   "warning keeps the new release and database writes")
            expect(NEW not in mod.load_record().get("failed_releases", []), "warning clears prior failure")
            if has_limits:
                from _provider_limits import status_limits
                rows = status_limits(limits.parent).get("provider_limits", [])
                expect(any(r["template"] == "grok" and "forced failure" in r["message"] for r in rows),
                       "node status carries the smoke failure reason", rows)
            else:
                expect(not limits.exists(), "warning does not create a provider limits file")

if problems:
    print("node-release-load-rollback: %d failed" % len(problems))
    raise SystemExit(1)
print("node-release-load-rollback: ok")
