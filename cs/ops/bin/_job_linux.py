"""Linux job boundary: a dedicated subreaper, optionally in a delegated cgroup v2.

The supervisor is outside the agent sandbox and never execs the agent itself.
Orphans therefore remain its children even if they discard the job environment.
"""
import ctypes
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from _job_identity import _pid_identity, _signal_pid


def prepare(command, env):
    """Wrap a job before Popen; no preexec_fn in the multithreaded executor."""
    return [sys.executable, str(Path(__file__).resolve()), "--supervise", *command], env


def _stat(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return int(fields[1]), fields[19], fields[0]
    except (OSError, IndexError, ValueError):
        return None


def owned_pids(supervisor_pid, identity, deadline):
    """Kernel adoption keeps all living descendants under this stable boundary."""
    if identity is None or _pid_identity(supervisor_pid) != identity:
        return {}
    rows = {}
    for entry in Path("/proc").iterdir():
        if time.monotonic() >= deadline:
            raise RuntimeError("job process cleanup deadline exceeded")
        if entry.name.isdigit():
            pid = int(entry.name)
            stat = _stat(pid)
            if stat and stat[2] != "Z":
                rows[pid] = (stat[0], int(stat[1]))
    # Do not derive authority from a supervisor PID reused during this scan.
    if _pid_identity(supervisor_pid) != identity:
        return {}
    def edge_matches(pid, parent, start, parent_start):
        current = _stat(pid)
        return (current is not None and current[2] != "Z"
                and current[0] == parent and int(current[1]) == start
                and _pid_identity(parent) == parent_start)

    def closure(candidates):
        owned = {supervisor_pid: identity}
        while True:
            additions = {pid: start for pid, (parent, start) in candidates.items()
                         if parent in owned and pid not in owned
                         and edge_matches(pid, parent, start, owned[parent])}
            if not additions:
                return owned
            owned.update(additions)

    owned = closure(rows)
    # Rebuild the entire ownership closure, rather than merely filtering dead
    # PIDs: a stale parent's numeric PID must not confer ownership on a new
    # process's children read later in the same /proc scan.
    candidates = {pid: rows[pid] for pid in owned if pid != supervisor_pid}
    owned = closure(candidates)
    return owned if _pid_identity(supervisor_pid) == identity else {}


def finish(supervisor_pid, identity, deadline):
    """Drain a live job without removing its adoption boundary prematurely.

    TERM asks the trusted supervisor to stop its root and reap adopted orphans.
    Host cleanup helps kill descendants, but never KILLs the subreaper: a child
    could otherwise fork with a cleared environment and be adopted by init.
    """
    if identity is None or _pid_identity(supervisor_pid) != identity:
        return
    _signal_pid(supervisor_pid, identity, signal.SIGTERM)
    while _pid_identity(supervisor_pid) == identity:
        if time.monotonic() >= deadline:
            raise RuntimeError("Linux job supervisor cleanup deadline exceeded")
        descendants = owned_pids(supervisor_pid, identity, deadline)
        descendants.pop(supervisor_pid, None)
        for sig in (signal.SIGSTOP, signal.SIGKILL):
            for pid, start in descendants.items():
                _signal_pid(pid, start, sig)
        # Keep the supervisor running so orphan adoption, waitpid and cgroup
        # removal can finish before the caller considers the job closed.
        time.sleep(min(0.005, max(0, deadline - time.monotonic())))


def _children():
    # All direct children belong to this dedicated supervisor. After a parent
    # dies the kernel adopts its orphans here, without a discovery window.
    parent = os.getpid()
    result = {}
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit():
            identity = _stat(int(entry.name))
            if identity and identity[0] == parent:
                result[int(entry.name)] = int(identity[1])
    return result


def _signal_children(children, sig):
    for pid, start in children.items():
        # A child may exit between enumeration and signaling; never signal a
        # PID that has since been assigned to a different process.
        current = _stat(pid)
        if current and current[0] == os.getpid() and int(current[1]) == start:
            _signal_pid(pid, start, sig)


def _drain():
    deadline = time.monotonic() + 2
    while True:
        children = _children()
        _signal_children(children, signal.SIGSTOP)
        _signal_children(children, signal.SIGKILL)
        while True:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                return
            if not pid:
                break
        if time.monotonic() >= deadline:
            raise RuntimeError("Linux job descendants survived cleanup")
        time.sleep(0.005)


def _delegated_cgroup():
    """Use a child of the current delegated v2 cgroup, never another estate's."""
    try:
        relative = next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines()
                        if line.startswith("0::"))
        parent = Path("/sys/fs/cgroup") / relative.lstrip("/")
        group = parent / f"cs-job-{os.getpid()}"
        group.mkdir(mode=0o700)
        # Enter before launching the job, so no descendant can miss enrollment.
        (group / "cgroup.procs").write_text(str(os.getpid()))
        return group, parent
    except (OSError, StopIteration):
        if "group" in locals():
            try:
                group.rmdir()
            except OSError:
                pass
        return None


def supervise(command):
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0):  # PR_SET_CHILD_SUBREAPER
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    cgroup = _delegated_cgroup()
    child = None
    signal_error = None
    cancelled = False
    def cancel(_sig, _frame):
        nonlocal cancelled
        cancelled = True
    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGINT, cancel)
    try:
        # Preserve the CLI/check's own session and process group; subreaper
        # adoption still catches descendants that detach from that group.
        child = subprocess.Popen(command, start_new_session=True)
        while child.poll() is None and not cancelled:
            time.sleep(0.01)
        code = child.returncode if child.returncode is not None else -signal.SIGTERM
        return code
    finally:
        if child is not None:
            # Losing this boundary with children alive makes them init's
            # children. Cleanup failures therefore retry here indefinitely;
            # the host may fail its own deadline, but adoption remains intact.
            while True:
                try:
                    _drain()
                    break
                except Exception as error:
                    # Keep ownership while retrying, but a denied signal must
                    # remain a failure even if a later drain succeeds.
                    if isinstance(error, OSError) and signal_error is None:
                        signal_error = error
                        print(f"job cleanup failed: {error}", file=sys.stderr, flush=True)
                    time.sleep(0.05)
            child.wait()
        # The supervisor itself must leave the cgroup before removing it.
        if cgroup:
            group, parent = cgroup
            try:
                (parent / "cgroup.procs").write_text(str(os.getpid()))
                group.rmdir()
            except OSError:
                pass

        if signal_error is not None:
            raise signal_error


if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] != "--supervise":
        raise SystemExit("usage: _job_linux.py --supervise command ...")
    code = supervise(sys.argv[2:])
    if code < 0:
        # Preserve Popen's negative-signal return code after trusted cleanup.
        sig = -code
        if sig not in (signal.SIGKILL, signal.SIGSTOP):
            signal.signal(sig, signal.SIG_DFL)
        _signal_pid(os.getpid(), _pid_identity(os.getpid()), sig)
        code = 128 + sig  # only reached if delivery unexpectedly fails
    raise SystemExit(code)
