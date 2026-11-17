#!/usr/bin/env python3
"""Host-independent tests for the Linux subreaper cleanup boundary."""
import signal
from pathlib import Path
import time
import unittest
from unittest.mock import patch

import _job_linux as jobs


class LinuxBoundaryTest(unittest.TestCase):
    def test_adopted_child_remains_owned_without_environment_marker(self):
        rows = {10: (1, "100", "S"), 11: (10, "101", "S"),
                12: (11, "102", "S"), 13: (1, "103", "S"),
                14: (10, "104", "Z")}
        def identity(pid):
            row = rows.get(pid)
            return int(row[1]) if row and row[2] != "Z" else None
        entries = [Path("/proc") / str(pid) for pid in rows]
        with patch.object(jobs.Path, "iterdir", return_value=entries), \
             patch.object(jobs, "_stat", side_effect=rows.get), \
             patch.object(jobs, "_pid_identity", side_effect=identity):
            self.assertEqual(jobs.owned_pids(10, 100, time.monotonic() + 1),
                             {10: 100, 11: 101, 12: 102})
            # Intermediate parent exits; the kernel adopts the grandchild.
            del rows[11]
            rows[12] = (10, "102", "S")
            self.assertEqual(jobs.owned_pids(10, 100, time.monotonic() + 1),
                             {10: 100, 12: 102})
            self.assertEqual(jobs.owned_pids(10, 99, time.monotonic() + 1), {})

    def test_stale_parent_does_not_own_replacement_children(self):
        # The first /proc snapshot saw A, then A exited and PID 11 was reused
        # by unrelated U before U's child 12 was read.
        cached = {10: (1, "100", "S"), 11: (10, "101", "S"),
                  12: (11, "102", "S")}
        live = {10: (1, "100", "S"), 11: (1, "201", "S"),
                12: (11, "102", "S")}
        reads = {}
        def stat(pid):
            reads[pid] = reads.get(pid, 0) + 1
            return cached[pid] if reads[pid] == 1 else live[pid]
        with patch.object(jobs.Path, "iterdir", return_value=[Path(str(p)) for p in cached]), \
             patch.object(jobs, "_stat", side_effect=stat), \
             patch.object(jobs, "_pid_identity", side_effect=lambda p: int(live[p][1])):
            self.assertEqual(jobs.owned_pids(10, 100, time.monotonic() + 1), {10: 100})

    def test_supervisor_retries_drain_failure_without_relinquishing_boundary(self):
        from unittest.mock import Mock
        child = Mock(returncode=-signal.SIGTERM)
        child.poll.return_value = -signal.SIGTERM
        libc = Mock()
        libc.prctl.return_value = 0
        with patch.object(jobs.ctypes, "CDLL", return_value=libc), \
             patch.object(jobs, "_delegated_cgroup", return_value=None), \
             patch.object(jobs.subprocess, "Popen", return_value=child), \
             patch.object(jobs.signal, "signal"), \
             patch.object(jobs.time, "sleep") as pause, \
             patch.object(jobs, "_drain", side_effect=[RuntimeError("deadline"), None]) as drain:
            self.assertEqual(jobs.supervise(["fixture"]), -signal.SIGTERM)
        self.assertEqual(drain.call_count, 2)
        pause.assert_called_with(0.05)
        child.wait.assert_called_once()

    def test_finish_keeps_supervisor_alive_while_killing_descendants(self):
        # A new adopted child appears after the first cleanup pass. It has no
        # environment marker; the still-living subreaper remains its boundary.
        snapshots = [{10: 100, 11: 101}, {10: 100, 12: 102}, {10: 100}]
        with patch.object(jobs, "_pid_identity", side_effect=[100, 100, 100, 100, None]), \
             patch.object(jobs, "owned_pids", side_effect=snapshots), \
             patch.object(jobs, "_signal_pid", return_value=True) as send, \
             patch.object(jobs.time, "sleep"):
            jobs.finish(10, 100, time.monotonic() + 1)
        self.assertEqual(send.call_args_list, [
            unittest.mock.call(10, 100, signal.SIGTERM),
            unittest.mock.call(11, 101, signal.SIGSTOP),
            unittest.mock.call(11, 101, signal.SIGKILL),
            unittest.mock.call(12, 102, signal.SIGSTOP),
            unittest.mock.call(12, 102, signal.SIGKILL),
        ])

    def test_reused_supervisor_is_never_signaled(self):
        with patch.object(jobs, "_pid_identity", return_value=200), \
             patch.object(jobs, "_signal_pid") as send:
            jobs.finish(10, 100, time.monotonic() + 1)
        send.assert_not_called()

    def test_stalled_supervisor_fails_closed_without_killing_boundary(self):
        with patch.object(jobs, "_pid_identity", return_value=100), \
             patch.object(jobs, "_signal_pid", return_value=True) as send:
            with self.assertRaisesRegex(RuntimeError, "deadline"):
                jobs.finish(10, 100, time.monotonic() - 1)
        self.assertEqual(send.call_args_list, [unittest.mock.call(10, 100, signal.SIGTERM)])


if __name__ == "__main__":
    unittest.main()
