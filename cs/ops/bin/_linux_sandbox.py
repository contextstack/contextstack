"""Linux sandbox readiness probes (bwrap, user namespaces, seccomp, Landlock), shared by cs doctor,
cs init and the land check. Standard library only: node bundles ship this module."""
import ctypes
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys


def sandbox_findings(*, bwrap_path=None):
    if bwrap_path is None:
        bwrap = shutil.which("bwrap")
    else:
        # Match cs-sandbox's first existing candidate, even if launching it fails.
        candidates = (Path(directory) / "bwrap" for directory in bwrap_path.split(os.pathsep))
        bwrap = next((str(path) for path in candidates if path.is_file()), None)
    yield finding("bubblewrap", bool(bwrap), bwrap or "bwrap was not found in " + (bwrap_path or "PATH"),
                  "install your distribution's bubblewrap package")
    if bwrap:
        command = [bwrap, "--unshare-user", "--ro-bind", "/", "/", "--", "/bin/true"]
        ok, detail = probe(command)
        yield finding("unprivileged user namespaces", ok, detail,
                      "enable unprivileged user namespaces and allow bubblewrap in your distribution's security policy")
        if ok:
            # A one-instruction classic BPF filter returning SECCOMP_RET_ALLOW.
            read_fd, write_fd = os.pipe()
            try:
                with os.fdopen(write_fd, "wb") as stream:
                    stream.write(struct.pack("=HBBI", 0x06, 0, 0, 0x7fff0000))
                ok, detail = probe(command[:-2] + ["--seccomp", str(read_fd)] + command[-2:], (read_fd,))
            finally:
                os.close(read_fd)
            yield finding("seccomp", ok, detail, "use a kernel with seccomp filter support and allow bubblewrap to install filters")
    abi = landlock_abi()
    yield dict(finding("Landlock", True, "ABI %d" % abi if abi > 0 else "unavailable", ""), info=True)


def probe(command, pass_fds=()):
    try:
        result = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                errors="replace", timeout=10, pass_fds=pass_fds)
    except (OSError, subprocess.TimeoutExpired) as error:
        return False, str(error)
    return result.returncode == 0, result.stderr.strip() or ("available" if not result.returncode
                                                            else "bwrap exited %d" % result.returncode)


def landlock_abi():
    if sys.platform != "linux" or os.uname().machine not in ("x86_64", "aarch64"):
        return -1
    # landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION).
    return ctypes.CDLL(None).syscall(444, 0, 0, 1)


def finding(check, ok, detail, fix):
    return dict(check=check, ok=ok, detail=detail, fix=fix)
