#!/usr/bin/env python3
"""Deterministic process ownership checks; live setsid attacks are in check."""
import io
import os
import subprocess
import sys
import threading
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "ops/bin"))
from _job_memory import JobProcesses

job = JobProcesses.__new__(JobProcesses)
job.pid, job.workspace = 900001, Path("/fixture/workspace")
job.marker, job.root_identity = "fixture-job", 100
job.known = {900001: 100, 999999: 999}
job._lock = threading.RLock()
identities = {900001: 100, 900002: 200, 900003: 300, 999999: 1000}

# Kernel tags find orphans even after setsid/chdir; cached PID integers alone
# never authorize ownership when their kernel birth identity has changed.
with patch("_job_memory.platform.system", return_value="Darwin"), \
     patch("_job_memory._marked_pids", return_value={900001: 100, 900002: 200, 900003: 300}), \
     patch("_job_memory._pid_identity", side_effect=identities.get), \
     patch("_job_darwin.owned_pids", return_value={}), \
     patch("_job_identity.suspect_pids", return_value=[]):
    assert job.pids() == {900001, 900002, 900003}
    assert 999999 not in job.known
    with patch("_job_memory._marked_pids", return_value={}):
        assert 900002 in job.pids()  # discovered identity remains authoritative
    identities[900002] = 201
    assert 900002 not in job.tracked_pids()

with patch("_job_memory._marked_pids", side_effect=PermissionError("blocked")), \
     patch("_job_memory._signal_pid") as kill:
    try:
        job.kill()
    except PermissionError:
        pass
    else:
        raise AssertionError("process enumeration failure must block host work")
    assert all(call.args[1] in (100, 200, 300) for call in kill.call_args_list)
print("check: tagged orphan ownership, birth-identity pruning and fail closed")

# RSS uses birth-validated cached ownership, never host-wide discovery itself.
from _job_memory import MemoryMonitor
job.known = {900001: 100, 900003: 300}
monitor = MemoryMonitor(100, job.pid, job)
with patch.object(job, "pids", side_effect=AssertionError("RSS enumerated processes")), \
     patch("_job_memory._pid_identity", side_effect=identities.get), \
     patch("_job_memory._pid_rss_kb", return_value=10):
    monitor.close()
    assert monitor.peak_kb == 10 * len(job.known)

monitor._stop = Mock()
monitor._stop.wait.side_effect = [False, True]
with patch.object(job, "pids", side_effect=AssertionError("RSS enumerated processes")), \
     patch.object(job, "kill") as kill, \
     patch("_job_memory._pid_identity", side_effect=identities.get), \
     patch("_job_memory._pid_rss_kb", return_value=200000):
    monitor._loop()
    assert monitor.exceeded
    kill.assert_called_once()

with patch.object(job, "_sweep") as sweep:
    job.kill()
    job.kill()
    sweep.assert_called_once()

# Force the stdout reader to execute before start() returns. A stop on the
# first line must kill before wait(), even on a very fast agent.
from _agent_stream import run_streaming
class InlineThread:
    def __init__(self, target, **kwargs):
        self.target = target
    def start(self):
        self.target()
    def join(self, *args):
        pass

proc = Mock(pid=900001, stdout=io.BytesIO(b"repeat\n"), stderr=io.BytesIO())
processes = Mock()
processes.evidence = [{"pid": 900099, "proc_pidpath": "/fixture/unknown"}]
observed = []
def waited(**kwargs):
    assert processes.kill.called, "early stuck stop was swallowed"
    return -9
proc.wait.side_effect = waited
with patch("_agent_stream.subprocess.Popen", return_value=proc), \
     patch("_agent_stream.threading.Thread", InlineThread), \
     patch("_job_memory.JobProcesses", return_value=processes):
    done = run_streaming(["fixture"], "/fixture/workspace", {}, 1,
                         lambda raw: {"stop": "stuck:repeat"}, on_evidence=observed.extend)
    assert done.forced_stop == "stuck:repeat"
    assert done.job_process_evidence == observed == processes.evidence
print("check: cached RSS, single cleanup and immediate streaming stop")

# TERM can arrive after wait() returns, during process cleanup. It must still
# become Cancelled so run_cli closes the turn with a discard, rather than a
# bare KeyboardInterrupt bypassing that close. Repeated TERM must be harmless.
import signal
from _muse import Cancelled
proc = Mock(pid=900001, stdout=io.BytesIO(b"partial\n"), stderr=io.BytesIO())
proc.wait.return_value = 0
processes = Mock()
processes.evidence = [{"pid": 900099, "proc_pidpath": "/fixture/unknown"}]
observed = []
def cancel_during_cleanup():
    signal.raise_signal(signal.SIGTERM)
    signal.raise_signal(signal.SIGTERM)
processes.close.side_effect = cancel_during_cleanup
with patch("_agent_stream.subprocess.Popen", return_value=proc), \
     patch("_agent_stream.threading.Thread", InlineThread), \
     patch("_job_memory.JobProcesses", return_value=processes):
    try:
        run_streaming(["fixture"], "/fixture/workspace", {}, 1, lambda raw: None, on_evidence=observed.extend)
    except Cancelled as error:
        assert error.stdout == "partial\n"
        assert observed == processes.evidence
    else:
        raise AssertionError("cleanup TERM lost cancellation")
print("check: TERM during stream cleanup preserves cancellation and drained output")


# Loaded-host cleanup can fail after TERM (ps/cwd discovery has a deadline).
# That failure must not replace Cancelled before the caller writes the discard.
import tempfile
from _agent_stream import Events
for fail_cleanup in (True, False):
    with tempfile.TemporaryDirectory() as directory:
        events = Events(directory)
        proc = Mock(pid=900001, stdout=io.BytesIO(b"partial\n"), stderr=io.BytesIO())
        proc.wait.return_value = 0
        processes = Mock()
        def close_failed():
            signal.raise_signal(signal.SIGTERM)
            if fail_cleanup:
                raise RuntimeError("job process cleanup deadline exceeded")
        processes.close.side_effect = close_failed
        with patch("_agent_stream.subprocess.Popen", return_value=proc), \
             patch("_agent_stream.threading.Thread", InlineThread), \
             patch("_job_memory.JobProcesses", return_value=processes):
            previous = signal.signal(signal.SIGTERM, signal.SIG_DFL)
            try:
                try:
                    run_streaming(["fixture"], directory, {}, 1,
                                  lambda raw: events.write(1, "Hello "))
                except Cancelled:
                    # A second gateway/owner TERM may land after stream cleanup,
                    # before the executor's emit() installs its ignore handler.
                    signal.raise_signal(signal.SIGTERM)
                    events.write(1, discard=True)
                else:
                    raise AssertionError("cancel lost")
            finally:
                signal.signal(signal.SIGTERM, previous)
        import json
        records = [json.loads(s) for s in events.path.read_text().splitlines()]
        assert records == [
            {"kind": "model.output", "payload": {"delta": "Hello ", "partial": True, "turn": 1}, "seq": 1},
            {"kind": "model.output", "payload": {"delta": "", "partial": False, "turn": 1, "discard": True}, "seq": 2},
        ], records
print("check: cleanup failure and repeated TERM preserve the durable discard")

# Discard syncs all preceding deltas and handles short writes without losing
# the tail of the JSON line. A disk failure rolls back the incomplete record.
from _agent_stream import append
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "events.jsonl"
    real_write = os.write
    def short_write(fd, data):
        return real_write(fd, data[:3])
    with patch("_agent_stream.os.write", side_effect=short_write), \
         patch("_agent_stream.os.fsync", wraps=os.fsync) as sync:
        events = Events(directory)
        events.write(1, "Hello ")
        sync.assert_not_called()
        events.write(1, discard=True)
        sync.assert_called_once()
    records = path.read_text().splitlines()
    assert len(records) == 2 and json.loads(records[-1])["payload"]["discard"]
    before = path.read_bytes()
    calls = 0
    def fail_after_partial(fd, data):
        global calls
        calls += 1
        if calls == 1:
            return real_write(fd, data[:2])
        raise OSError("disk full")
    with patch("_agent_stream.os.write", side_effect=fail_after_partial):
        try:
            append(path, {"broken": True}, durable=True)
        except OSError:
            pass
        else:
            raise AssertionError("write failure swallowed")
    assert path.read_bytes() == before
print("check: discard fsync, short writes and append rollback")
