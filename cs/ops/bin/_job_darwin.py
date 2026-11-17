"""Darwin job boundary: a live, disclaimed responsibility supervisor.

The private SPI is resolved with dlsym (ctypes). Host experiments show identity
survives fork, setsid and exec with an empty environment, including /bin/sleep.
It disappears when the responsible process exits, so that process must drain
its descendants before exiting. Audit sessions and coalitions are inherited
but shared with the launcher; neither supplies per-job ownership authority.
"""
import ctypes
from functools import lru_cache
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from _job_identity import _darwin_lib, _darwin_pids, _pid_identity, _signal_pid


@lru_cache(maxsize=1)
def _responsibility_api():
    lib = _darwin_lib()
    try:
        query = lib.responsibility_get_pid_responsible_for_pid
        disclaim = lib.responsibility_spawnattrs_setdisclaim
    except AttributeError as error:
        raise RuntimeError("Darwin job responsibility SPI unavailable") from error
    query.argtypes, query.restype = [ctypes.c_int], ctypes.c_int
    disclaim.argtypes, disclaim.restype = [ctypes.POINTER(ctypes.c_void_p), ctypes.c_int], ctypes.c_int
    return query, disclaim


def responsible_pid(pid):
    """A nonpositive result is unknown, never ownership authority."""
    try:
        query, _ = _responsibility_api()
    except RuntimeError:
        return None
    responsible = query(int(pid))
    return responsible if responsible > 0 else None


def prepare(command, env):
    """No preexec_fn: establish the boundary in a trusted fresh interpreter."""
    _responsibility_api()  # refuse unsupported hosts before launching any job
    return [sys.executable, str(Path(__file__).resolve()), "--disclaim", *command], env


def _exec_disclaimed(command):
    """SETEXEC keeps Popen's PID and birth identity while disclaiming inheritance."""
    lib = _darwin_lib()
    _, disclaim = _responsibility_api()
    attr = ctypes.c_void_p()
    def checked(result, operation):
        if result:
            raise OSError(result, operation)
    checked(lib.posix_spawnattr_init(ctypes.byref(attr)), "posix_spawnattr_init")
    try:
        checked(disclaim(ctypes.byref(attr), 1), "responsibility_spawnattrs_setdisclaim")
        # Darwin POSIX_SPAWN_SETEXEC, an exec replacement rather than a new pid.
        checked(lib.posix_spawnattr_setflags(ctypes.byref(attr), 0x0040), "POSIX_SPAWN_SETEXEC")
        argv = [sys.executable, str(Path(__file__).resolve()), "--supervise", *command]
        encoded = [os.fsencode(part) for part in argv]
        environment = [os.fsencode(key) + b"=" + os.fsencode(value) for key, value in os.environ.items()]
        cargv = (ctypes.c_char_p * (len(encoded) + 1))(*encoded, None)
        cenv = (ctypes.c_char_p * (len(environment) + 1))(*environment, None)
        pid = ctypes.c_int()
        checked(lib.posix_spawn(ctypes.byref(pid), encoded[0], None,
                               ctypes.byref(attr), cargv, cenv), "posix_spawn SETEXEC")
        raise RuntimeError("Darwin SETEXEC unexpectedly returned")
    finally:
        lib.posix_spawnattr_destroy(ctypes.byref(attr))


def owned_pids(supervisor_pid, identity, deadline):
    """Query only same-uid pids and revalidate both ends of each kernel match."""
    if identity is None or _pid_identity(supervisor_pid) != identity:
        return {}
    if responsible_pid(supervisor_pid) != supervisor_pid:
        return {}  # SETEXEC may not have run yet; no descendants exist then
    owned = {supervisor_pid: identity}
    for pid in _darwin_pids():
        if time.monotonic() >= deadline:
            raise RuntimeError("Darwin job process cleanup deadline exceeded")
        birth = _pid_identity(pid)
        if birth is not None and responsible_pid(pid) == supervisor_pid:
            if _pid_identity(pid) == birth:
                owned[pid] = birth
    return owned if _pid_identity(supervisor_pid) == identity else {}


def finish(supervisor_pid, identity, deadline):
    """Cancel and assist cleanup without destroying its responsibility anchor."""
    if identity is None or _pid_identity(supervisor_pid) != identity:
        return
    _signal_pid(supervisor_pid, identity, signal.SIGTERM)
    while _pid_identity(supervisor_pid) == identity:
        if time.monotonic() >= deadline:
            raise RuntimeError("Darwin job supervisor cleanup deadline exceeded")
        descendants = owned_pids(supervisor_pid, identity, deadline)
        descendants.pop(supervisor_pid, None)
        for sig in (signal.SIGSTOP, signal.SIGKILL):
            for pid, birth in descendants.items():
                _signal_pid(pid, birth, sig)
        time.sleep(min(0.005, max(0, deadline - time.monotonic())))


def _drain(identity):
    deadline = time.monotonic() + 2
    while True:
        descendants = owned_pids(os.getpid(), identity, deadline)
        descendants.pop(os.getpid(), None)
        if not descendants:
            return
        for sig in (signal.SIGSTOP, signal.SIGKILL):
            for pid, birth in descendants.items():
                _signal_pid(pid, birth, sig)
        if time.monotonic() >= deadline:
            raise RuntimeError("Darwin job descendants survived cleanup")
        time.sleep(0.005)


def supervise(command):
    if responsible_pid(os.getpid()) != os.getpid():
        raise RuntimeError("Darwin job did not acquire its own responsibility")
    identity = _pid_identity(os.getpid())
    if identity is None:
        raise RuntimeError("Darwin job supervisor birth identity unavailable")
    cancelled = False
    def cancel(_sig, _frame):
        nonlocal cancelled
        cancelled = True
    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGINT, cancel)
    child = None
    signal_error = None
    try:
        # Preserve the CLI/check's own session and process group; responsibility
        # ownership survives setsid and remains anchored in this supervisor.
        child = subprocess.Popen(command, start_new_session=True)
        while child.poll() is None and not cancelled:
            time.sleep(0.01)
        return child.returncode if child.returncode is not None else -signal.SIGTERM
    finally:
        if child is not None:
            # Keep the anchor alive on transient sample failures. Parent cleanup
            # has its own bounded deadline and reports failure rather than
            # removing this anchor while an undiscovered descendant may live.
            while True:
                try:
                    _drain(identity)
                    break
                except Exception as error:
                    # Keep ownership while retrying, but a denied signal must
                    # remain a failure even if a later drain succeeds.
                    if isinstance(error, OSError) and signal_error is None:
                        signal_error = error
                        print(f"job cleanup failed: {error}", file=sys.stderr, flush=True)
                    time.sleep(0.05)
            child.wait()
            if signal_error is not None:
                raise signal_error


if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] not in ("--disclaim", "--supervise"):
        raise SystemExit("usage: _job_darwin.py --disclaim command ...")
    if sys.argv[1] == "--disclaim":
        _exec_disclaimed(sys.argv[2:])
    code = supervise(sys.argv[2:])
    if code < 0:
        sig = -code
        if sig not in (signal.SIGKILL, signal.SIGSTOP):
            signal.signal(sig, signal.SIG_DFL)
        _signal_pid(os.getpid(), _pid_identity(os.getpid()), sig)
        code = 128 + sig
    raise SystemExit(code)
