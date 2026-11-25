"""Per-job resident memory cap for executor CLI process groups."""
import ctypes
import os
import platform
import re
import signal
import subprocess
import threading
import time
import uuid

from _job_identity import MARKER, _marked_pids, _pid_identity, _signal_pid, forget_marker
from pathlib import Path

from _vitals import load_resources_policy, read_vitals

HERE = Path(__file__).resolve().parent
_GRACE_S = 5.0
_POLL_S = 3.0
_SAMPLE_DEADLINE_S = 5.0  # one background sample
_CLEANUP_DEADLINE_S = 10.0  # the final sweep, measured after the sampler has stopped
_SUSPECT_DEADLINE_S = 5.0  # the evidence-only suspect scan, once in close()
_MAX_SAMPLE_FAILURES = 10  # consecutive failed samples before tracking is declared broken
_RSS_LINE = re.compile(r"^\s*(\d+)\s*$")


def _run(argv, timeout=1.0):
    try:
        return subprocess.run(
            argv, capture_output=True, text=True, timeout=timeout, check=False,
        ).stdout or ""
    except (OSError, subprocess.TimeoutExpired):
        return ""


def _pids_in_group(pgid):
    out = _run(["pgrep", "-g", str(pgid)])
    pids = [int(part) for part in (out or "").split() if part.strip().isdigit()]
    return pids


def _darwin_pid_rss_kb(pid):
    class ProcTaskInfo(ctypes.Structure):
        _fields_ = [
            ("pti_virtual_size", ctypes.c_uint64),
            ("pti_resident_size", ctypes.c_uint64),
            ("pti_total_user", ctypes.c_uint64),
            ("pti_total_system", ctypes.c_uint64),
            ("pti_threads_user", ctypes.c_uint64),
            ("pti_threads_system", ctypes.c_uint64),
            ("pti_policy", ctypes.c_int32),
            ("pti_faults", ctypes.c_int32),
            ("pti_pageins", ctypes.c_int32),
            ("pti_cow_faults", ctypes.c_int32),
            ("pti_messages_sent", ctypes.c_int32),
            ("pti_messages_received", ctypes.c_int32),
            ("pti_syscalls_mach", ctypes.c_int32),
            ("pti_syscalls_unix", ctypes.c_int32),
            ("pti_csw", ctypes.c_int32),
            ("pti_threadnum", ctypes.c_int32),
            ("pti_numrunning", ctypes.c_int32),
            ("pti_priority", ctypes.c_int32),
        ]

    libc = ctypes.CDLL("/usr/lib/libc.dylib")
    info = ProcTaskInfo()
    if libc.proc_pidinfo(int(pid), 4, 0, ctypes.byref(info), ctypes.sizeof(info)) <= 0:
        return 0
    return int(info.pti_resident_size // 1024)


def _linux_pid_rss_kb(pid):
    path = Path("/proc/%d/status" % int(pid))
    try:
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    except (OSError, ValueError, IndexError):
        return 0
    return 0


def _pid_rss_kb(pid):
    system = platform.system()
    if system == "Darwin":
        rss = _darwin_pid_rss_kb(pid)
        if rss > 0:
            return rss
    elif system == "Linux":
        rss = _linux_pid_rss_kb(pid)
        if rss > 0:
            return rss
    out = _run(["ps", "-o", "rss=", "-p", str(pid)])
    lines = [ln.strip() for ln in (out or "").splitlines() if ln.strip()]
    if not lines:
        return 0
    match = _RSS_LINE.match(lines[0])
    return int(match.group(1)) if match else 0


def process_group_rss_kb(pgid):
    """Resident memory (KiB) for every process in the group, or 0 when unknown."""
    if not pgid or pgid <= 0:
        return 0
    return sum(_pid_rss_kb(pid) for pid in _pids_in_group(pgid))


def terminate_process_group(pgid, leader_identity, grace_s=_GRACE_S):
    """Snapshot a group while its original leader still anchors it."""
    if leader_identity is None or _pid_identity(pgid) != leader_identity:
        return
    members = {}
    for pid in _pids_in_group(pgid):
        birth = _pid_identity(pid)
        try:
            in_group = os.getpgid(pid) == pgid
        except ProcessLookupError:
            continue
        if in_group and birth is not None and _pid_identity(pid) == birth:
            members[pid] = birth
    if _pid_identity(pgid) != leader_identity:
        return
    for pid, birth in members.items():
        _signal_pid(pid, birth, signal.SIGTERM)
    deadline = time.monotonic() + grace_s
    while time.monotonic() < deadline:
        if not any(birth is not None and _pid_identity(pid) == birth
                   for pid, birth in members.items()):
            return
        time.sleep(0.2)
    for pid, birth in members.items():
        _signal_pid(pid, birth, signal.SIGKILL)


def prepare_job(command, env):
    """Attach independent env and kernel ownership before the first fork.

    Darwin retains a disclaimed responsible supervisor through descendant
    cleanup; Linux uses a subreaper. The env marker is an additional signal.
    """
    env = dict(env)
    marker = uuid.uuid4().hex
    env[MARKER] = marker
    if platform.system() == "Darwin":
        from _job_darwin import prepare
        command, env = prepare(command, env)
    elif platform.system() == "Linux":
        from _job_linux import prepare
        command, env = prepare(command, env)
    return command, env, marker


class JobProcesses:
    """Discover inherited job tags independently of ancestry, cwd and fork timing."""

    def __init__(self, pid, workspace, marker=None):
        self.pid, self.workspace = pid, Path(workspace).resolve()
        self.marker = marker
        identity = _pid_identity(pid)
        self.root_identity = identity
        self.known = {pid: identity} if identity is not None else {}
        self._stop = threading.Event()
        self._lock = threading.RLock()
        self.error = None
        self.evidence = []
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def tracked_pids(self):
        with self._lock:
            return {pid for pid, identity in self.known.items()
                    if identity is not None and _pid_identity(pid) == identity}

    def pids(self, deadline=None, sweep=True):
        deadline = deadline if deadline is not None else time.monotonic() + 2.0
        # Scan outside the cache lock: a slow background sample must not consume
        # cleanup's entire deadline merely waiting for that same sample.
        owned = _marked_pids(self.marker, deadline) if self.marker else {}
        if platform.system() == "Darwin" and self.root_identity is not None:
            from _job_darwin import owned_pids
            owned.update(owned_pids(self.pid, self.root_identity, deadline))
        if platform.system() == "Linux":
            from _job_linux import owned_pids
            owned.update(owned_pids(self.pid, self.root_identity, deadline))
        with self._lock:
            if getattr(self, "_cleaned", False):
                return set()
            owned.update({pid: birth for pid, birth in self.known.items()
                          if birth is not None and _pid_identity(pid) == birth})
            owned.pop(os.getpid(), None)
            self.known = owned
            return set(owned)

    def _loop(self):
        failures = 0
        while not self._stop.wait(0.2):
            try:
                if getattr(self, "_cleaned", False):
                    return
                self.pids(deadline=time.monotonic() + _SAMPLE_DEADLINE_S)
                failures = 0
            except Exception as error:
                failures += 1
                if failures >= _MAX_SAMPLE_FAILURES:
                    self.error = error
                    return

    def kill(self, deadline=None):
        # Early sweeps (kill_group, the memory cap) do the real cleanup: close() then
        # finds the job cleaned, so they need close()'s budget, not a 2 s default.
        deadline = deadline if deadline is not None else time.monotonic() + _CLEANUP_DEADLINE_S
        if not self._lock.acquire(timeout=max(0, deadline - time.monotonic())):
            raise RuntimeError("job process tracking busy")
        try:
            if getattr(self, "_cleaned", False):
                return
            self._sweep(deadline)
            self._cleaned = True
        finally:
            self._lock.release()

    def _sweep(self, deadline=None):
        deadline = deadline if deadline is not None else time.monotonic() + _CLEANUP_DEADLINE_S
        if platform.system() == "Darwin":
            from _job_darwin import finish
            finish(self.pid, self.root_identity, deadline)
        elif platform.system() == "Linux":
            from _job_linux import finish
            finish(self.pid, self.root_identity, deadline)
        for attempt in range(3):
            try:
                pids = self.pids(deadline=deadline, sweep=attempt == 0)
            except Exception:
                # Discovery failure is fail-closed, but stale cached PIDs are
                # never safe to signal without checking their birth identity.
                for pid, birth in self.known.items():
                    _signal_pid(pid, birth, signal.SIGKILL)
                raise
            if not pids:
                return
            for sig in (signal.SIGSTOP, signal.SIGKILL):
                for pid in pids:
                    _signal_pid(pid, self.known.get(pid), sig)
            if attempt < 2:
                time.sleep(min(0.01, max(0, deadline - time.monotonic())))
        raise RuntimeError("job processes survived cleanup")

    def _record_suspects(self):
        """Evidence only, once per job, on its own budget: never inside the sweep deadline."""
        if platform.system() != "Darwin" or self.root_identity is None:
            return
        from _job_identity import suspect_pids
        try:
            with self._lock:
                owned = dict(self.known)
            suspects = suspect_pids(self.root_identity, owned, time.monotonic() + _SUSPECT_DEADLINE_S)
        except Exception as error:  # evidence must never fail a finished job
            suspects = [{"reason": "evidence_failed", "pid": None, "identity": [], "error": str(error)[:200]}]
        with self._lock:
            evidence = getattr(self, "evidence", None)
            if evidence is None:
                evidence = []
            seen = {(row["pid"], tuple(row["identity"])) for row in evidence}
            for row in suspects:
                if (row["pid"], tuple(row["identity"])) not in seen:
                    evidence.append(row)
            self.evidence = evidence

    def close(self):
        # The background sample may hold a scan for up to _SAMPLE_DEADLINE_S on a busy machine.
        # Cleanup gets its own budget after that join, or a slow last sample leaves the sweep no
        # time and a finished job ends void ('job process cleanup deadline exceeded', 2026-11-12).
        self._stop.set()
        self._thread.join(timeout=_SAMPLE_DEADLINE_S)
        try:
            self.kill(deadline=time.monotonic() + _CLEANUP_DEADLINE_S)
        finally:
            self._record_suspects()
            forget_marker(self.marker)
        if self.error is not None:
            raise RuntimeError("job process tracking failed") from self.error


def _memory_mb_from_budget(spec, limits):
    for source in (limits, (spec or {}).get("budget") if isinstance(spec, dict) else None):
        if not isinstance(source, dict):
            continue
        raw = source.get("memory_mb")
        if type(raw) is int and raw > 0:
            return raw
    return None


def job_memory_cap_bytes(spec=None, limits=None, config_root=None):
    """Cap in bytes, or None when no limit applies."""
    mb = _memory_mb_from_budget(spec, limits)
    if mb is not None:
        return mb * 1024 * 1024
    config_root = Path(config_root or os.environ.get("CS_CONFIG_ROOT") or HERE.parent.parent)
    policy = load_resources_policy(config_root)
    frac = policy.get("job_memory_fraction")
    try:
        frac = float(frac) if frac is not None else 0.25
    except (TypeError, ValueError):
        frac = 0.25
    if frac <= 0:
        return None
    total = (read_vitals(config_root=config_root) or {}).get("memory_total_bytes")
    if not total or total <= 0:
        return None
    return int(total * frac)


class MemoryMonitor:
    """Poll RSS for a process group; terminate when over cap_mb."""

    def __init__(self, cap_mb, pgid, processes=None):
        self.cap_kb = int(cap_mb) * 1024
        self.pgid = pgid
        self.processes = processes
        self.leader_identity = _pid_identity(pgid) if processes is None and pgid else None
        self.peak_kb = 0
        self.exceeded = False
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        if self.cap_kb <= 0 or not self.pgid:
            return
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def _loop(self):
        while not self._stop.wait(_POLL_S):
            rss = (sum(_pid_rss_kb(pid) for pid in self.processes.tracked_pids()) if self.processes
                   else process_group_rss_kb(self.pgid))
            if rss > self.peak_kb:
                self.peak_kb = rss
            if rss > self.cap_kb:
                self.exceeded = True
                if self.processes:
                    self.processes.kill()
                else:
                    terminate_process_group(self.pgid, self.leader_identity)
                return

    def close(self):
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=_POLL_S + 1)
        rss = (sum(_pid_rss_kb(pid) for pid in self.processes.tracked_pids()) if self.processes
                   else process_group_rss_kb(self.pgid))
        if rss > self.peak_kb:
            self.peak_kb = rss

    @property
    def peak_rss_bytes(self):
        return self.peak_kb * 1024

    def forced_meta(self):
        if not self.exceeded:
            return None
        mb = self.cap_kb // 1024
        peak_mb = round(self.peak_kb / 1024.0, 2)
        return {
            "stop": "memory_cap",
            "reason": "job RSS %.2f MiB exceeded cap %d MiB" % (peak_mb, mb),
            "peak_rss_bytes": self.peak_rss_bytes,
            "memory_cap_bytes": self.cap_kb * 1024,
        }
