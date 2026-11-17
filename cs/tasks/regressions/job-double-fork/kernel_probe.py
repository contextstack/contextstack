"""Host-only, fixture-owned observations of Darwin accounting identities."""
import ctypes
import json
import os
from pathlib import Path
import signal
import struct
import sys
import tempfile
import time

from _job_identity import _darwin_environment, _darwin_lib, _pid_identity, _signal_pid
from _job_darwin import _responsibility_api, responsible_pid


def _snapshot(pid):
    lib = _darwin_lib()
    path = ctypes.create_string_buffer(4096)
    path_length = lib.proc_pidpath(pid, path, len(path))
    coalition = ctypes.create_string_buffer(40)
    ctypes.set_errno(0)
    size = lib.proc_pidinfo(pid, 20, 0, coalition, len(coalition))  # PROC_PIDCOALITIONINFO
    coalition_error = ctypes.get_errno() if size != 40 else None
    audit = ctypes.create_string_buffer(56)  # struct auditpinfo_addr, SDK bsm/audit.h
    struct.pack_into('=i', audit, 0, pid)
    ctypes.set_errno(0)
    result = lib.auditon(28, audit, len(audit))  # A_GETPINFO_ADDR
    audit_error = ctypes.get_errno() if result else None
    return {'pid': pid, 'birth': _pid_identity(pid), 'responsible': responsible_pid(pid),
            'path': os.fsdecode(path.value) if path_length > 0 else None,
            'environment_readable': _darwin_environment(pid) is not None,
            'coalition': list(struct.unpack('=5Q', coalition.raw)) if size == 40 else None,
            'coalition_errno': coalition_error,
            'audit_session': struct.unpack_from('=I', audit.raw, 40)[0] if result == 0 else None,
            'audit_errno': audit_error}


_ROOT = r'''
import ctypes, json, os, sys, time
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from _job_identity import _pid_identity
from _job_darwin import responsible_pid
file, release, mode = sys.argv[2:]
def row(stage):
    buf = ctypes.create_string_buffer(48)
    lib = ctypes.CDLL(None, use_errno=True)
    result = lib.getaudit_addr(buf, len(buf))
    import struct
    record = {'stage': stage, 'pid': os.getpid(), 'birth': _pid_identity(os.getpid()),
              'responsible': responsible_pid(os.getpid()),
              'self_audit_session': struct.unpack_from('=I', buf.raw, 36)[0] if result == 0 else None}
    with open(file, 'a') as out: out.write(json.dumps(record) + '\n')
row('root')
if os.fork() == 0:
    row('fork'); os.setsid(); row('setsid')
    if os.fork(): os._exit(0)
    os.chdir('/'); row('double-fork')
    fd = os.open('/dev/null', os.O_RDWR)
    for target in (0, 1, 2): os.dup2(fd, target)
    env = dict(os.environ) if mode.endswith('-env') else {}
    if mode.startswith('platform'):
        os.execve('/bin/sleep', ['sleep', '15'], env)
    target = 'import time; from pathlib import Path; Path(' + repr(file + '.exec') + ').touch(); time.sleep(15)'
    os.execve(sys.executable, [sys.executable, '-c', target], env)
os.wait()
while not Path(release).exists(): time.sleep(.005)
'''


def _spawn(argv):
    lib = _darwin_lib()
    _, disclaim = _responsibility_api()
    attr = ctypes.c_void_p()
    def checked(result):
        if result:
            raise OSError(result, 'kernel identity probe posix_spawn')
    checked(lib.posix_spawnattr_init(ctypes.byref(attr)))
    try:
        checked(disclaim(ctypes.byref(attr), 1))
        encoded = [os.fsencode(part) for part in argv]
        environment = [os.fsencode(key) + b'=' + os.fsencode(value) for key, value in os.environ.items()]
        cargv = (ctypes.c_char_p * (len(encoded) + 1))(*encoded, None)
        cenv = (ctypes.c_char_p * (len(environment) + 1))(*environment, None)
        pid = ctypes.c_int()
        checked(lib.posix_spawn(ctypes.byref(pid), encoded[0], None, ctypes.byref(attr), cargv, cenv))
        return pid.value
    finally:
        lib.posix_spawnattr_destroy(ctypes.byref(attr))


def kernel_identity_probe():
    """Print reproducible observations; never signal anything but fixture pids."""
    query, disclaim = _responsibility_api()  # ctypes resolves both via dlsym
    report = {'dlsym': {'responsibility_get_pid_responsible_for_pid': bool(query),
                        'responsibility_spawnattrs_setdisclaim': bool(disclaim)},
              'launcher': _snapshot(os.getpid()), 'cases': []}
    audit = ctypes.create_string_buffer(48)
    if _darwin_lib().getaudit_addr(audit, len(audit)) == 0:
        report['launcher']['self_audit_session'] = struct.unpack_from('=I', audit.raw, 36)[0]
    ops = str(Path(__file__).resolve().parents[3] / 'ops/bin')
    with tempfile.TemporaryDirectory(prefix='kernel-identity-', dir=Path(__file__).resolve().parents[4]) as raw:
        workspace = Path(raw)
        for mode in ('python-env', 'python-clear', 'platform-env', 'platform-clear'):
            records, release = workspace / (mode + '.jsonl'), workspace / (mode + '.release')
            root = _spawn([sys.executable, '-c', _ROOT, ops, str(records), str(release), mode])
            root_birth = _pid_identity(root)
            grand = grand_birth = None
            executed = False
            try:
                deadline = time.monotonic() + 10
                rows = []
                while time.monotonic() < deadline:
                    if records.exists():
                        rows = [json.loads(line) for line in records.read_text().splitlines()]
                    if rows and rows[-1]['stage'] == 'double-fork':
                        grand, grand_birth = rows[-1]['pid'], tuple(rows[-1]['birth'])
                        state = _snapshot(grand)
                        executed = (state['path'] == '/bin/sleep' if mode.startswith('platform')
                                    else Path(str(records) + '.exec').exists())
                        if executed:
                            break
                    time.sleep(.01)
                assert grand is not None and _pid_identity(grand) == grand_birth, mode + ': fixture did not exec'
                assert executed, mode + ': fixture did not complete exec'
                state = _snapshot(grand)
                assert all(row['responsible'] == root for row in rows), mode + ': fork/setsid lost responsibility'
                assert state['responsible'] == root, mode + ': exec lost responsibility'
                assert (state['path'] == '/bin/sleep' if mode.startswith('platform') else bool(state['path']))
                state['coalition_matches_launcher'] = state['coalition'] == report['launcher']['coalition']
                state['audit_matches_launcher'] = all(row['self_audit_session'] == report['launcher'].get('self_audit_session') for row in rows)
                report['cases'].append({'case': mode, 'stages': rows, 'exec': state})
                release.touch()
                os.waitpid(root, 0)
                report['cases'][-1]['after_root_exit'] = _snapshot(grand)
            finally:
                # Both identities originate from this fixture before exec. A
                # failed sample or PID reuse never authorizes fallback cleanup.
                if grand is not None:
                    _signal_pid(grand, grand_birth, signal.SIGKILL)
                _signal_pid(root, root_birth, signal.SIGKILL)
                try:
                    os.waitpid(root, 0)
                except ChildProcessError:
                    pass
    print('check: kernel identity observations ' + json.dumps(report, sort_keys=True))
    return report
