"""Host-only lead control socket authentication and best-effort child watch registration."""
import fcntl
import json
import logging
import re
import shutil
import os
import time
from pathlib import Path
import socket
import stat

log = logging.getLogger(__name__)
LEGACY_SOCKET_MAX_AGE_S = 24 * 3600


def fail(message):
    raise ValueError(message)


def private_socket_dir(path: Path, *, create: bool = False) -> None:
    """Never repair an unsafe pre-existing directory or follow an attacker link."""
    parent = path.parent.absolute()
    for part in reversed((parent, *parent.parents)):
        if part.is_symlink():
            fail("symlink in socket directory")
    if create:
        parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = parent.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        fail("socket directory must be owned by user and mode 0700")


def read_socket_token(path: Path) -> str:
    private_socket_dir(path)
    fd = os.open(path.parent / "token", os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
            fail("unsafe socket token file")
        token = stream.read(256).strip()
    if not token:
        fail("missing socket token")
    return token


def report_lead_watch(job_id, lane=None):
    """Called by host-side cs mcp; the serve token never enters the CLI sandbox."""
    path = os.environ.get("LEAD_HOST_SOCKET", "").strip()
    if not path or not job_id:
        return
    try:
        payload = {"op": "watch_job", "job_id": str(job_id),
                   "token": read_socket_token(Path(path))}
        if lane:
            payload["lane"] = str(lane)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(2.0)
            client.connect(path)
            client.sendall((json.dumps(payload) + "\n").encode("utf-8"))
            client.recv(4096)
    except (OSError, ValueError):
        pass


TEMP_SOCKET_PATTERN = re.compile(r"cs-lead-%s-[0-9a-f]{16}" % os.getuid())


def temporary_socket_dir(path: Path) -> bool:
    return TEMP_SOCKET_PATTERN.fullmatch(path.name) is not None


def _legacy_socket_dir_contents(directory: Path) -> bool:
    """True when the directory is empty or holds only an unlinked unix socket."""
    try:
        names = list(directory.iterdir())
    except OSError:
        return False
    if not names:
        return True
    if len(names) != 1:
        return False
    path = directory / names[0]
    try:
        info = path.lstat()
    except OSError:
        return False
    return stat.S_ISSOCK(info.st_mode) and info.st_uid == os.getuid()


def sweep_legacy_socket_dirs(root: Path) -> list[str]:
    """Remove stale cs-lead-* directories left before owner.pid recording existed."""
    removed = []
    now = time.time()
    try:
        children = list(root.iterdir())
    except OSError:
        return removed
    for directory in children:
        if not temporary_socket_dir(directory):
            continue
        owner = directory / "owner.pid"
        if owner.exists():
            continue
        try:
            private_socket_dir(directory / "socket")
            info = directory.stat()
            if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
                continue
            if now - info.st_mtime < LEGACY_SOCKET_MAX_AGE_S:
                continue
            if not _legacy_socket_dir_contents(directory):
                continue
            shutil.rmtree(directory)
            removed.append(str(directory))
            log.info("removed legacy lead socket directory %s", directory)
        except (OSError, ValueError):
            continue
    return removed


def sweep_socket_dirs(root: Path) -> None:
    """Only reap exact, owned socket directories with a recorded dead serve PID.

    The lifetime lock also protects a serve between creating its directory and
    recording its PID. Legacy directories without ownership records stay put.
    """
    sweep_legacy_socket_dirs(root)
    for directory in root.iterdir():
        if not temporary_socket_dir(directory):
            continue
        try:
            private_socket_dir(directory / "socket")
            fd = os.open(directory / "serve.lock", os.O_RDWR | os.O_NOFOLLOW)
            try:
                info = os.fstat(fd)
                if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
                    continue
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                owner_fd = os.open(directory / "owner.pid", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
                with os.fdopen(owner_fd) as stream:
                    info = os.fstat(stream.fileno())
                    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
                        continue
                    raw = stream.read(32).strip()
                if not re.fullmatch(r"[1-9][0-9]*", raw):
                    continue
                try:
                    os.kill(int(raw), 0)
                except ProcessLookupError:
                    shutil.rmtree(directory)
            finally:
                os.close(fd)
        except (OSError, ValueError, OverflowError):
            continue


def remove_socket_dir(path: Path) -> None:
    """Called only by the serve holding this directory's lifetime lock."""
    if temporary_socket_dir(path.parent):
        private_socket_dir(path)
        shutil.rmtree(path.parent)
