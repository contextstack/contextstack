"""Host-side job tags and birth identities; no ps, ancestry or cwd discovery."""
import ctypes
import errno
from functools import lru_cache
import os
import platform
import struct
import time
from pathlib import Path

MARKER = 'CS_JOB_MARKER'
_scans = {}


def forget_marker(marker):
    _scans.pop(marker, None)


@lru_cache(maxsize=1)
def _darwin_lib():
    return ctypes.CDLL('/usr/lib/libc.dylib', use_errno=True)


def _darwin_identity(pid):
    # PROC_PIDT_BSDINFOWITHUNIQID samples proc_bsdinfo and the unique
    # identifier together. The unique ID survives exec; pidversion need not.
    buf = ctypes.create_string_buffer(192)
    if _darwin_lib().proc_pidinfo(int(pid), 18, 0, buf, len(buf)) != len(buf):
        return None
    raw = buf.raw
    if struct.unpack_from('=I', raw, 4)[0] == 5:  # SZOMB
        return None
    birth = struct.unpack_from('=QQ', raw, 120)
    unique_id, _, version = struct.unpack_from('=QQI', raw, 152)
    return (*birth, unique_id), version


def _pid_identity(pid):
    """Kernel birth identity, excluding zombies and unreadable processes."""
    if platform.system() == 'Darwin':
        sample = _darwin_identity(pid)
        return sample[0] if sample else None
    try:
        fields = Path('/proc/%d/stat' % pid).read_text().rsplit(')', 1)[1].split()
        return None if fields[0] == 'Z' else int(fields[19])  # field 22, starttime
    except (OSError, ValueError, IndexError):
        return None


def _signal_pid(pid, identity, sig):
    # Revalidate before EACH signal, including the KILL following STOP. A failed
    # sample must never turn a cached integer into authority over a reused PID.
    if identity is None or pid <= 0:
        return False
    if platform.system() == 'Darwin':
        sample = _darwin_identity(pid)
        if sample is None or sample[0] != identity:
            return False
        if sig == 0:
            return True
        # XNU uses PID and pidversion from the token to look up the target.
        # The kernel checks the version while acquiring the process reference.
        token = (ctypes.c_uint32 * 8)(0, 0, 0, 0, 0, pid, 0, sample[1])
        error = _darwin_lib().proc_signal_with_audittoken(token, int(sig))
        if not error:
            return True
        if error == errno.ESRCH and _darwin_identity(pid) != sample:
            return False
        raise OSError(error, os.strerror(error), pid)
    if _pid_identity(pid) != identity:
        return False
    try:
        os.kill(pid, sig)
        return True
    except ProcessLookupError:
        # ESRCH is harmless only when the target actually exited or changed
        # identity. A still-live owned process refusing a signal is a failure.
        if _pid_identity(pid) == identity:
            raise
        return False


def _darwin_pids():
    lib = _darwin_lib()
    size = lib.proc_listpids(4, os.getuid(), None, 0)
    if size <= 0:
        raise RuntimeError('cannot enumerate job processes')
    while True:
        buf = (ctypes.c_int * (size // 4 + 256))()
        used = lib.proc_listpids(4, os.getuid(), buf, ctypes.sizeof(buf))
        if used <= 0:
            raise RuntimeError('cannot enumerate job processes')
        if used < ctypes.sizeof(buf):
            return [pid for pid in buf[:used // 4] if pid > 0]
        size = ctypes.sizeof(buf) * 2


@lru_cache(maxsize=1)
def _darwin_argmax():
    argmax = ctypes.c_int()
    length = ctypes.c_size_t(ctypes.sizeof(argmax))
    query = (ctypes.c_int * 2)(1, 8)
    if _darwin_lib().sysctl(query, 2, ctypes.byref(argmax), ctypes.byref(length), None, 0):
        raise OSError(ctypes.get_errno(), 'KERN_ARGMAX')
    return argmax.value


def _darwin_environment(pid):
    lib = _darwin_lib()
    mib = (ctypes.c_int * 3)(1, 49, pid)  # CTL_KERN, KERN_PROCARGS2
    # KERN_ARGMAX is host-defined; do not truncate large environments.
    buf = ctypes.create_string_buffer(_darwin_argmax())
    length = ctypes.c_size_t(len(buf))
    if lib.sysctl(mib, 3, buf, ctypes.byref(length), None, 0):
        error = ctypes.get_errno()
        if error in (errno.ESRCH, errno.EINVAL, errno.EPERM, errno.EACCES, errno.EIO):
            return None  # unavailable is not proof of an absent tag
        raise OSError(error, 'KERN_PROCARGS2')
    raw = buf.raw[:length.value]
    if len(raw) < 4:
        return ()
    argc = struct.unpack_from('=i', raw)[0]
    pos = raw.find(b'\0', 4) + 1  # executable path
    if pos <= 0 or argc < 0:
        return ()
    while pos < len(raw) and raw[pos] == 0:
        pos += 1
    for _ in range(argc):
        end = raw.find(b'\0', pos)
        if end < 0:
            return ()
        pos = end + 1
    return raw[pos:].split(b'\0')


def _marked_pids(marker, deadline):
    expected = (MARKER + '=' + marker).encode()
    # Birth identities let later samples skip unrelated long-lived host tasks.
    # The per-job cache starts after Popen's exec handshake, never before launch.
    checked = _scans.setdefault(marker, {})
    pids = _darwin_pids() if platform.system() == 'Darwin' else [int(p.name) for p in Path('/proc').iterdir() if p.name.isdigit()]
    owned = {}
    for pid in pids:
        if time.monotonic() >= deadline:
            raise RuntimeError('job process cleanup deadline exceeded')
        identity = _pid_identity(pid)
        if identity is None:
            continue
        previous = checked.get(pid)
        if previous is not None and previous[0] == identity and previous[1] is not None:
            # Positive and negative results both stand for this birth identity: the
            # tag is inherited at fork, so re-reading up to 1 MiB of environment per
            # process on every scan buys nothing. Unreadable (None) is retried.
            if previous[1]:
                owned[pid] = identity
            continue
        if platform.system() == 'Darwin':
            env = _darwin_environment(pid)
        else:
            try:
                env = Path('/proc/%d/environ' % pid).read_bytes().split(b'\0')
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                continue
        if env is None:
            checked[pid] = (identity, None)
            continue
        if _pid_identity(pid) == identity:
            matches = expected in env
            checked[pid] = (identity, matches)
            if matches:
                owned[pid] = identity
    present = set(pids)
    for pid in set(checked) - present:
        checked.pop(pid, None)
    return owned


def suspect_pids(root_identity, owned, deadline):
    """Evidence only: never grant signal authority from unreadable metadata."""
    from _job_darwin import responsible_pid
    suspects = []
    for pid in _darwin_pids():  # proc_listpids(PROC_UID_ONLY, real uid)
        if time.monotonic() >= deadline:
            # Evidence only: a slow scan on a busy machine must not fail a finished job
            # (2026-11-12: a passing lane ended void). Record that the list is partial.
            suspects.append({'reason': 'evidence_truncated', 'pid': None, 'identity': []})
            break
        if pid in owned or pid == os.getpid():
            continue
        birth = _pid_identity(pid)
        if birth is None or birth[-1] <= root_identity[-1]:
            continue
        # Birth identities must bracket EVERY metadata query, including path.
        env = _darwin_environment(pid)
        responsible = responsible_pid(pid)
        if env is not None and responsible is not None:
            continue
        path = ctypes.create_string_buffer(4096)  # PROC_PIDPATHINFO_MAXSIZE
        length = _darwin_lib().proc_pidpath(pid, path, len(path))
        if _pid_identity(pid) != birth:
            continue
        suspects.append({'pid': pid, 'identity': list(birth),
                         'proc_pidpath': os.fsdecode(path.value) if length > 0 else None,
                         'reason': 'membership_unreadable',
                         'environment_readable': env is not None,
                         'responsibility_readable': responsible is not None})
    return suspects
