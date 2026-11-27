"""Hermetic hosted release checks using a fake installer and smoke result."""
from contextlib import closing
import fcntl
import importlib.util
from importlib.machinery import SourceFileLoader
import json
import os
import subprocess
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

CS = Path(__file__).resolve().parents[3]
sys.path[:0] = [str(CS / "ops/bin"), str(CS / "scripts")]
HOME = tempfile.TemporaryDirectory(prefix="lead-install-home-")
TEST_ROOT = Path(HOME.name)
with patch.dict(os.environ, HOME=str(TEST_ROOT), CS_STATE_ROOT=str(TEST_ROOT / "state")):
    # Other modules in unittest discovery may already have imported these with a different HOME.
    with patch.dict(sys.modules):
        for module in ("_lead_install", "_node_layout", "_node_services"):
            sys.modules.pop(module, None)
        import _lead_install as install
        loader = SourceFileLoader("unit_node_release", str(CS / "scripts/node-release"))
        release_spec = importlib.util.spec_from_loader(loader.name, loader)
        release_script = importlib.util.module_from_spec(release_spec)
        loader.exec_module(release_script)
for path in (install.layout.CONFIG, install.layout.RECORD, install.layout.DIRECTOR_RS_DATA,
             release_script.CONFIG, release_script.RECORD, release_script.DIRECTOR_RS_DATA):
    assert path.resolve().is_relative_to(TEST_ROOT.resolve()), path


class HostedRelease(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(dir=TEST_ROOT)
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.releases = self.root / "releases"
        self.releases.mkdir()
        self.current = self.root / "current"
        self.current.symlink_to(self.releases / "old")
        self.bundle = self.root / "bundles" / "stable"
        (self.bundle / "release").mkdir(parents=True)
        self.commit = "a" * 40
        (self.bundle / "bundle.json").write_text(json.dumps({
            "release": "new", "code_commit": self.commit, "files": {}}))
        (self.bundle / "runtime").write_text("approved")
        self.row = {"kind": "install", "commit": self.commit, "bundle": "stable",
                    "target_hash": install.bundle_digest(self.bundle)}
        self.pin = {"before": "old", "schema_current": None, "serve_director_rs": False}
        mocks = patch.multiple(install.layout, ROOT=self.root, RELEASES=self.releases, CURRENT=self.current,
                               INSTALL_LOCK=self.releases / "install.lock", DIRECTOR_DB=self.root / "no-db")
        env = patch.dict(os.environ, HOME=str(TEST_ROOT), CS_STATE_ROOT=str(TEST_ROOT / "state"))
        env.start()
        self.addCleanup(env.stop)
        mocks.start()
        self.addCleanup(mocks.stop)

    def test_changed_bundle_is_refused_before_any_execution(self):
        (self.bundle / "runtime").write_text("changed")
        with self.assertRaisesRegex(ValueError, "changed after inspection"):
            install.stage(self.row, self.pin, self.root)

    def test_install_wait_times_out_without_switching(self):
        path = self.root / "timeout.json"
        install.write_json(path, {"row": self.row, "pin": self.pin})
        fake = self.root / "node-install"
        fake.write_text("#!/bin/sh\nexit 2\n")
        fake.chmod(0o700)
        def check_frozen(_):
            with (self.root / "state/restart-intake.lock").open() as lock:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(lock, fcntl.LOCK_SH | fcntl.LOCK_NB)

        with patch.object(install, "WAIT_TIMEOUT", 7), \
                patch.object(install.time, "monotonic", side_effect=[0, 1, 1, 2, 7]), \
                patch.object(install.time, "sleep", side_effect=check_frozen) as sleep:
            outcome = install.execute(path, fake)
        sleep.assert_called_once_with(5)
        self.assertEqual(outcome["status"], "timed_out")
        self.assertEqual(outcome["release"], "old")
        self.assertIn("timed out", outcome["reason"])

    def test_database_copy_and_backup_require_integrity(self):
        live, snapshots = self.root / "director.sqlite", self.root / "snapshots"
        with closing(sqlite3.connect(live)) as db, db:
            db.executescript("PRAGMA user_version=13; CREATE TABLE parent(id INTEGER PRIMARY KEY);"
                             "CREATE TABLE child(id INTEGER REFERENCES parent(id)); INSERT INTO parent VALUES (1);")
        backups = self.root / "backups"
        with patch.multiple(install.layout, DIRECTOR_DB=live, DIRECTOR_DB_SNAPS=snapshots,
                            DIRECTOR_DB_BACKUPS=backups):
            snapshot = install.layout.snapshot_director_db("new", checked=True)
            for path in (snapshot, backups / ("backup-" + snapshot.name)):
                with closing(sqlite3.connect(path)) as copy:
                    self.assertEqual(copy.execute("PRAGMA user_version").fetchone(), (13,))
                    self.assertEqual(copy.execute("SELECT id FROM parent").fetchone(), (1,))
            with closing(sqlite3.connect(live)) as db, db:
                db.execute("INSERT INTO child VALUES (2)")
            with self.assertRaisesRegex(install.layout.Refused, "foreign-key check"):
                install.layout.snapshot_director_db("broken", checked=True)

    def test_busy_then_smoke_and_receipt_survive_replay(self):
        for smoke in ("green", "red"):
            with self.subTest(smoke=smoke):
                self.current.unlink()
                self.current.symlink_to(self.releases / "old")
                release = self.releases / "new"
                if release.exists():
                    (release / "bundle.json").unlink()
                    release.rmdir()
                path = self.root / f"{smoke}.json"
                install.write_json(path, {"row": self.row, "pin": self.pin})
                fake_smoke = self.root / f"smoke-{smoke}"
                fake_smoke.write_text("#!/bin/sh\nexit " + ("0\n" if smoke == "green" else "1\n"))
                fake_smoke.chmod(0o700)
                calls = self.root / f"calls-{smoke}"
                fake = self.root / "node-install"
                fake.write_text(
                    "#!/bin/sh\n"
                    f"if [ ! -e '{calls}' ]; then\n"
                    f"  mkdir '{release}' && cp \"$2/bundle.json\" '{release}' && touch '{calls}' || exit 98\n"
                    "  exit 2\n"
                    "fi\n"
                    "test \"$1\" = install && test \"$3\" = --no-wait && test \"$4\" = --hosted-trial || exit 99\n"
                    f"if '{fake_smoke}'; then\n"
                    f"  ln -snf '{self.releases / 'new'}' '{self.current}'\n"
                    "  echo '{\"release\":\"new\",\"smoke\":{\"ok\":true}}'\n"
                    "else\n"
                    "  echo '{\"refused\":\"hosted post-install smoke failed; old release old is back\"}'\n"
                    "  exit 1\n"
                    "fi\n"
                )
                fake.chmod(0o700)
                with patch.object(install.time, "sleep") as sleep:
                    install.execute(path, fake)
                    sleep.assert_called_once_with(5)
                outcome = install.read_json(path)["outcome"]
                self.assertEqual(outcome["status"], "installed" if smoke == "green" else "rolled_back")
                self.assertEqual(outcome["release"], "new" if smoke == "green" else "old")
                fake.unlink()
                install.execute(path, fake)
                self.assertEqual(install.read_json(path)["outcome"], outcome)

    def test_inspect_staged_and_rollback_images(self):
        previous = self.releases / "previous"
        previous.mkdir()
        (previous / "bundle.json").write_text(json.dumps({"release": "previous", "code_commit": self.commit}))
        (previous / "generated").write_text("first")
        with patch.object(install.layout, "load_record", return_value={"previous": "previous"}):
            pin = install.inspect(self.row, self.root)
            self.assertEqual(pin["commit_provenance"], "bundle_json")
            self.assertEqual(pin["target_hash"], self.row["target_hash"])
            self.assertEqual(pin["backup"], "local_only")
            rollback = {"kind": "rollback", **install.inspect({"kind": "rollback"}, self.root)}
        self.assertEqual(install.stage(rollback, rollback, self.root), previous)
        (previous / "generated").write_text("changed")
        with self.assertRaisesRegex(ValueError, "rollback image changed"):
            install.stage(rollback, rollback, self.root)

    def test_interrupted_recovery_reports_live_unsmoked_release(self):
        path = self.root / "interrupted.json"
        self.current.unlink()
        self.current.symlink_to(self.releases / "new")
        install.write_json(path, {"started": True, "row": self.row, "pin": self.pin})
        outcome = install.execute(path, self.root / "must-not-run")
        self.assertEqual(outcome["status"], "interrupted")
        self.assertEqual(outcome["release"], "new")
        self.assertIs(outcome["switched_without_smoke"], True)

    def test_poll_continues_after_conflict_and_skips_active_worker(self):
        ids = ["act_" + digit * 26 for digit in "123"]
        rows = [{"id": rid, "status": "pending", **self.row} for rid in ids[:2]]
        rows.append({"id": ids[2], "status": "approved", **self.row})
        client = Mock()
        client.call.side_effect = [{"release_requests": rows},
                                   install.DirectorUnreachable("raced", status=409), {}]
        directory = self.root / "node/release-requests"
        directory.mkdir(parents=True)
        path = directory / (ids[2] + ".json")
        install.write_json(path, {"row": self.row, "pin": self.pin})
        with path.with_suffix(".lock").open("a") as lock, \
                patch.object(install, "inspect", return_value=self.pin), patch.object(install, "launch") as launch:
            fcntl.flock(lock, fcntl.LOCK_EX)
            install.poll(client, self.root, CS, self.root)
        self.assertIn(ids[1], client.call.call_args.args[1])
        launch.assert_not_called()

    def test_launch_uses_distinct_units(self):
        with patch.object(install.sys, "platform", "linux"), patch.object(install.subprocess, "run") as run:
            for _ in range(2):
                install.launch(self.root / "approval.json", CS / "scripts/node-install", self.root)
        units = [next(arg for arg in call.args[0] if arg.startswith("--unit=")) for call in run.call_args_list]
        self.assertEqual(len(set(units)), 2)

    def test_build_bounds_archive_and_reports_stderr(self):
        pin = {**self.pin, "commit": self.commit, "repo": str(self.root), "serve_client_web": False}
        proc = Mock()
        proc.wait.side_effect = subprocess.TimeoutExpired("node-bundle", install.BUILD_TIMEOUT)

        def child(*_, stderr, **kwargs):
            stderr.write(b"fixture build error")
            return release_script.nullcontext(proc)

        with patch.object(install.subprocess, "run") as run, patch.object(install.tarfile, "open"), \
                patch.object(install.subprocess, "Popen", side_effect=child), patch.object(install, "stop_group") as stop:
            with self.assertRaisesRegex(ValueError, "timed out.*fixture build error"):
                install.build(pin, self.root / "bundle-output")
        self.assertEqual(run.call_args.kwargs["timeout"], 30)
        proc.wait.assert_called_once_with(timeout=install.BUILD_TIMEOUT)
        stop.assert_called_once_with(proc)

    def test_rollback_window_retries_and_times_out(self):
        busy = install.layout.DrainRefused({"active": [{"id": "fixture"}]})
        with patch.object(release_script, "restart_window", side_effect=[busy, release_script.nullcontext()]), \
                patch.object(release_script.time, "sleep") as sleep:
            with release_script.rollback_window(True, True):
                sleep.assert_called_once_with(2)
        with patch.object(release_script, "ROLLBACK_WAIT_TIMEOUT", 2), \
                patch.object(release_script.time, "monotonic", side_effect=[0, 0, 1, 2]), \
                patch.object(release_script.time, "sleep"), \
                patch.object(release_script, "restart_window", side_effect=busy), \
                patch.object(release_script, "active_jobs", return_value=[{"id": "fixture"}]):
            with self.assertRaisesRegex(release_script.RollbackWaiting, "rollback waiting on 1 jobs"):
                with release_script.rollback_window(True, True):
                    self.fail("rollback window entered after timeout")

    def test_red_smoke_timeout_keeps_intake_hold_and_failure_details(self):
        report = {"ok": False, "checks": [{"name": "services", "ok": False, "reason": "fixture"}]}
        error = release_script.SmokeFailed("hosted post-install smoke failed: services", report)
        state = self.root / "state"
        state.mkdir()
        with patch.object(release_script, "ROOT", self.root), patch.object(release_script, "save_record"), \
                patch.object(release_script, "rollback_window", side_effect=release_script.RollbackWaiting(3)):
            with self.assertRaisesRegex(release_script.RollbackWaiting, "rollback waiting on 3 jobs") as caught:
                release_script.rollback_failed_activation("old", "new", {}, {}, True, None, error, hosted=True)
        self.assertEqual(caught.exception.__cause__.report, report)
        self.assertEqual((state / "release-rollback-pending").read_text().strip(), "new")

    def test_red_smoke_rolls_back_across_schema_change(self):
        live = self.root / "director.sqlite"
        saved = self.root / "pre-upgrade.sqlite"
        for path, version in ((live, 14), (saved, 13)):
            with closing(sqlite3.connect(path)) as db, db:
                db.execute(f"PRAGMA user_version={version}")
        error = release_script.SmokeFailed("hosted post-install smoke failed", {"ok": False})
        with patch.multiple(install.layout, DIRECTOR_DB=live), patch.multiple(
            release_script, ROOT=self.root, complete=Mock(return_value=True), current_id=Mock(return_value="new"),
            director_db_schema=lambda path=live: install.layout.director_db_schema(path),
            director_supported_schema=Mock(return_value=13), unload_agent=Mock(), activate=Mock()
        ):
            with self.assertRaisesRegex(release_script.Refused, "old release old is back"):
                release_script.restore_failed_trial("old", "new", {}, True, saved, error, True, True, {})
            release_script.activate.assert_called_once()
        self.assertEqual(install.layout.director_db_schema(live), 13)
