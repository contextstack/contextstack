#!/usr/bin/env python3
"""Per-job memory cap and pi harness cost tests. Run: python ops/bin/_job_memory_test.py"""
import json
import os
import signal
import struct
import subprocess
import sys
import tempfile
import time
import unittest
import threading
from unittest.mock import patch
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
ALLOC_STUB = """
import sys, time
chunks = []
while True:
    chunks.append(b"x" * (4 * 1024 * 1024))
    sys.stdout.write(".")
    sys.stdout.flush()
    time.sleep(0.1)
"""


class JobOwnershipTest(unittest.TestCase):
    def test_same_timestamp_replacement_loses_cached_marker(self):
        import _job_identity as identity

        pid, marker = 123, "reused-pid"
        self.addCleanup(identity.forget_marker, marker)
        deadline = time.monotonic() + 2

        def procinfo(_pid, flavor, _arg, buf, size):
            struct.pack_into("=QQ", buf, 120, 100, 1)
            if flavor == 18:
                struct.pack_into("=QQI", buf, 152, unique_id, 0, 1)
            return size

        unique_id = 200
        with patch.object(identity.platform, "system", return_value="Darwin"), \
             patch.object(identity, "_darwin_lib") as lib, \
             patch.object(identity, "_darwin_pids", return_value=[pid]), \
             patch.object(identity, "_darwin_environment", return_value=[b"CS_JOB_MARKER=reused-pid"]) as env:
            lib.return_value.proc_pidinfo.side_effect = procinfo
            owned = identity._marked_pids(marker, deadline)
            self.assertIn(pid, owned)
            unique_id += 1
            env.return_value = []
            self.assertEqual(identity._marked_pids(marker, deadline), {})
            self.assertFalse(identity._signal_pid(pid, owned[pid], signal.SIGKILL))

    def test_reused_responsibility_anchor_excludes_older_process(self):
        import _job_darwin as darwin

        births = {11: (100, 1, 200), 12: (100, 1, 199), 13: (100, 1, 201)}
        with patch.object(darwin, "_pid_identity", side_effect=births.get), \
             patch.object(darwin, "_darwin_pids", return_value=list(births)), \
             patch.object(darwin, "responsible_pid", return_value=11):
            self.assertEqual(darwin.owned_pids(11, births[11], time.monotonic() + 2),
                             {11: births[11], 13: births[13]})

    @unittest.skipUnless(sys.platform == "darwin", "Darwin audit-token signals")
    def test_kernel_rejects_replacement_between_identity_check_and_signal(self):
        import _job_identity as identity

        with subprocess.Popen(["/bin/sleep", "30"], start_new_session=True) as decoy:
            try:
                birth, version = identity._darwin_identity(decoy.pid)
                # Model a checked PID being recycled in the interval between
                # userspace lookup and the kernel signal syscall.
                with patch.object(identity, "_darwin_identity", side_effect=[(birth, version ^ 1), (birth, version)]):
                    self.assertFalse(identity._signal_pid(decoy.pid, birth, signal.SIGKILL))
                self.assertIsNone(decoy.poll())
                self.assertTrue(identity._signal_pid(decoy.pid, birth, signal.SIGTERM))
                self.assertEqual(decoy.wait(timeout=5), -signal.SIGTERM)
            finally:
                if decoy.poll() is None:
                    decoy.kill()

    def test_group_lookup_failure_cannot_authorize_a_bare_pid(self):
        import _job_memory as jobs

        with patch.object(jobs, "_run", return_value=""), \
             patch.object(jobs, "_pid_identity", return_value=100), \
             patch.object(jobs, "_signal_pid") as send:
            jobs.terminate_process_group(123, 100, grace_s=0)
            send.assert_not_called()

    @unittest.skipUnless(sys.platform == "darwin", "Darwin responsibility")
    def test_same_uid_sibling_spawned_during_finish_survives(self):
        import _job_darwin as darwin

        command, env = darwin.prepare(
            ["/bin/sh", "-c", "echo $$; exec /bin/sleep 30"], dict(os.environ))
        decoys = []
        send = darwin._signal_pid

        def signal_with_decoy(pid, birth, sig):
            if sig == signal.SIGTERM:
                decoys.append(subprocess.Popen(["/bin/sleep", "30"], start_new_session=True))
            return send(pid, birth, sig)

        with subprocess.Popen(command, env=env, stdout=subprocess.PIPE) as supervisor:
            try:
                child_pid = int(supervisor.stdout.readline())
                birth = darwin._pid_identity(supervisor.pid)
                child_birth = darwin._pid_identity(child_pid)
                self.assertIsNotNone(child_birth)
                deadline = time.monotonic() + 5
                with patch.object(darwin, "_signal_pid", side_effect=signal_with_decoy):
                    darwin.finish(supervisor.pid, birth, deadline)
                self.assertNotEqual(darwin._pid_identity(child_pid), child_birth)
                self.assertEqual(len(decoys), 1)
                self.assertNotEqual(darwin.responsible_pid(decoys[0].pid), supervisor.pid)
                self.assertIsNone(decoys[0].poll())
                self.assertIsNotNone(supervisor.wait(timeout=max(0, deadline - time.monotonic())))
            finally:
                if supervisor.poll() is None:
                    supervisor.terminate()
                for decoy in decoys:
                    if decoy.poll() is None:
                        decoy.kill()
                    decoy.wait()


class JobMemoryTest(unittest.TestCase):
    def setUp(self):
        # Ownership-unit fixtures use synthetic births, not real Darwin pids.
        for target in ("_job_darwin.owned_pids", "_job_identity.suspect_pids"):
            mocked = patch(target, return_value={} if target.endswith("owned_pids") else [])
            mocked.start()
            self.addCleanup(mocked.stop)

    def tracker(self):
        from _job_memory import JobProcesses
        tracker = JobProcesses.__new__(JobProcesses)
        tracker.pid = 987654
        tracker.workspace = HERE
        tracker.marker = "fixture-job"
        tracker.root_identity = 100
        tracker.known = {tracker.pid: 100}
        tracker._lock = threading.RLock()
        return tracker

    def test_zombies_and_reused_pids_are_absent_from_snapshot(self):
        import _job_memory as jobs
        tracker = self.tracker()
        with patch.object(jobs.platform, "system", return_value="Darwin"), patch.object(
            jobs, "_marked_pids", return_value={}
        ) as scan, patch.object(jobs, "_pid_identity", return_value=None):
            self.assertEqual(tracker.pids(), set())
            scan.assert_called_once()

    def test_marked_orphan_is_found_without_parent_group_or_cwd(self):
        import _job_memory as jobs
        tracker = self.tracker()
        identities = {987655: 200}
        with patch.object(jobs.platform, "system", return_value="Darwin"), patch.object(
            jobs, "_marked_pids", return_value=identities
        ), patch.object(jobs, "_pid_identity", side_effect=identities.get):
            self.assertEqual(tracker.pids(), {987655})
            self.assertEqual(tracker.known, identities)

    def test_transient_sample_failures_do_not_fail_tracking(self):
        import _job_memory as jobs
        tracker = self.tracker()
        tracker._stop = threading.Event(); tracker.error = None; tracker._cleaned = False
        calls = []
        def flaky(deadline=None, sweep=True):
            calls.append(1)
            if len(calls) <= 3:
                raise subprocess.TimeoutExpired("ps", 2)
            if len(calls) >= 6:
                tracker._stop.set()
            return set()
        with patch.object(tracker, "pids", side_effect=flaky), patch.object(jobs.time, "monotonic", return_value=0.0):
            tracker._loop()
        self.assertIsNone(tracker.error)
        self.assertGreaterEqual(len(calls), 4)

    def test_persistent_sample_failures_fail_tracking(self):
        import _job_memory as jobs
        tracker = self.tracker()
        tracker._stop = threading.Event(); tracker.error = None; tracker._cleaned = False
        with patch.object(tracker, "pids", side_effect=subprocess.TimeoutExpired("ps", 2)):
            tracker._loop()
        self.assertIsInstance(tracker.error, subprocess.TimeoutExpired)

    def test_sweep_stops_after_three_snapshots(self):
        import _job_memory as jobs
        tracker = self.tracker()
        with patch.object(tracker, "pids", return_value={tracker.pid}) as snapshot, patch.object(
            jobs, "_signal_pid"
        ), patch.object(jobs.time, "sleep"):
            with self.assertRaisesRegex(RuntimeError, "survived"):
                tracker.kill()
            self.assertEqual(snapshot.call_count, 3)

    def test_live_process_then_zombie_finishes_on_second_pass(self):
        import _job_memory as jobs
        tracker = self.tracker()
        identities = {tracker.pid: 100}
        snapshots = [identities, {}]
        def sample(*args, **kwargs):
            result = snapshots.pop(0)
            if not result:
                identities.clear()
            return result.copy()
        with patch.object(jobs.platform, "system", return_value="Darwin"), patch.object(
            jobs, "_marked_pids", side_effect=sample
        ) as scan, patch.object(jobs, "_signal_pid") as kill, patch.object(
            jobs, "_pid_identity", side_effect=identities.get
        ):
            tracker.kill()
            self.assertEqual(scan.call_count, 2)
            self.assertEqual(kill.call_args_list, [unittest.mock.call(tracker.pid, 100, signal.SIGSTOP),
                                                  unittest.mock.call(tracker.pid, 100, signal.SIGKILL)])

    def test_sweep_passes_one_total_deadline(self):
        import _job_memory as jobs
        tracker = self.tracker()
        def slow_scan(*, deadline, sweep):
            time.sleep(max(0, deadline - time.monotonic()))
            raise subprocess.TimeoutExpired("ps", 0.6)
        with patch.object(tracker, "pids", side_effect=slow_scan), patch.object(jobs, "_signal_pid"):
            started = time.monotonic()
            with self.assertRaises(subprocess.TimeoutExpired):
                tracker.kill(deadline=started + 0.1)
            self.assertLess(time.monotonic() - started, 0.5)

    def test_close_gives_cleanup_its_own_budget_after_a_slow_sample(self):
        import _job_memory as jobs
        tracker = self.tracker()
        seen = []
        def slow_join(timeout=None):
            time.sleep(1.5)  # the last background sample used most of the old 2 s-total window
        def kill(deadline):
            seen.append(deadline - time.monotonic())
        tracker._stop, tracker.error = threading.Event(), None
        tracker._thread = type("SlowThread", (), {"join": staticmethod(slow_join)})()
        with patch.object(tracker, "kill", side_effect=kill), \
                patch.object(jobs, "_CLEANUP_DEADLINE_S", 1.0):
            tracker.close()
        self.assertEqual(len(seen), 1)
        self.assertGreater(seen[0], 0.9)

    def test_job_memory_fraction_default(self):
        from _vitals import load_resources_policy

        policy = load_resources_policy(HERE.parent.parent)
        self.assertEqual(policy.get("job_memory_fraction"), 0.25)

    def test_cap_from_memory_mb_budget(self):
        from _job_memory import job_memory_cap_bytes

        spec = {"budget": {"memory_mb": 512}}
        self.assertEqual(job_memory_cap_bytes(spec=spec, config_root=HERE.parent.parent), 512 * 1024 * 1024)

    def test_pi_subscription_cost_and_usage(self):
        from _est import parse_usage
        from _harness_usage import resolve_harness_cost

        tmpl = json.loads((HERE / "executor-templates.json").read_text())["pi"]
        cost_class, unmetered = resolve_harness_cost(tmpl, tmpl.get("args"))
        self.assertEqual(cost_class, "subscription")
        self.assertFalse(unmetered)
        text = (HERE.parent.parent / "tasks/regressions/cli-providers/fixtures/pi.jsonl").read_text()
        row = parse_usage(text, "fixture", 0.0)
        self.assertEqual(row.get("input_tokens"), 10)
        self.assertEqual(row.get("output_tokens"), 2)
        self.assertEqual(row.get("est_basis"), "rate")

    def test_process_stopped_on_memory_cap(self):
        from _job_memory import process_group_rss_kb
        from _muse import run_process

        with tempfile.TemporaryDirectory() as raw:
            ws = Path(raw)
            script = ws / "alloc.py"
            script.write_text(ALLOC_STUB, encoding="utf-8")
            env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(ws)}
            started = time.time()
            try:
                done = run_process(
                    [sys.executable, str(script)],
                    str(ws),
                    env,
                    120,
                    memory_cap_mb=32,
                )
            except subprocess.TimeoutExpired:
                self.fail("allocator should be stopped by memory cap, not wall timeout")
            self.assertLess(time.time() - started, 60)
            meta = getattr(done, "forced_meta", None)
            self.assertIsInstance(meta, dict)
            self.assertEqual(meta.get("stop"), "memory_cap")
            self.assertGreater(meta.get("peak_rss_bytes", 0), 32 * 1024 * 1024 * 0.5)
            time.sleep(0.5)
            # Confirm the stub's process group did not survive.
            probe = subprocess.run(
                ["pgrep", "-f", str(script)],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(probe.returncode, 0, msg=(probe.stdout or probe.stderr))


class JobLaunchTest(unittest.TestCase):
    def test_check_keeps_its_own_group_and_workspace(self):
        from _muse import run_process
        with tempfile.TemporaryDirectory() as raw:
            ws = Path(raw).resolve()
            command = [sys.executable, "-c", (
                "import os; from pathlib import Path; "
                "assert os.getpid() == os.getpgrp(); "
                "Path('answer.txt').write_text('done\\n')"
            )]
            done = run_process(command, ws, dict(os.environ), 10)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual((ws / "answer.txt").read_text(), "done\n")
            checked = run_process(["/bin/sh", "-c", '[ "$(cat answer.txt)" = done ]'],
                                  ws, dict(os.environ), 10)
            self.assertEqual(checked.returncode, 0, checked.stderr)


if __name__ == "__main__":
    unittest.main()
