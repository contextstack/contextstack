"""Atomic file replace for install scripts and ops/bin (stdlib only)."""
import os
import stat
from pathlib import Path


def _fsync_dir(path):
    fd = os.open(str(path), os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_atomic(path, data, *, mode=None, encoding=None):
    """Replace `path` atomically: temp write, fsync file, rename, fsync parent directory.

    `data` is bytes, or text when `encoding` is set (default utf-8). When `mode` is set the result is chmod'd;
    otherwise an existing file keeps its mode."""
    path = Path(path)
    if isinstance(data, str):
        payload = data.encode(encoding or "utf-8")
    else:
        payload = data
    path.parent.mkdir(parents=True, exist_ok=True)
    if mode is None and path.is_file():
        try:
            mode = stat.S_IMODE(path.stat().st_mode)
        except OSError:
            pass
    tmp = path.with_name(".%s.%d.tmp" % (path.name, os.getpid()))
    try:
        if mode is not None:
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
            fd = os.open(tmp, flags, mode)
            with os.fdopen(fd, "wb") as out:
                out.write(payload)
                out.flush()
                os.fsync(out.fileno())
        else:
            with open(tmp, "wb") as out:
                out.write(payload)
                out.flush()
                os.fsync(out.fileno())
        os.replace(tmp, path)
        if mode is not None:
            try:
                os.chmod(path, mode)
            except OSError:
                pass
        _fsync_dir(path.parent)
    except BaseException:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        raise


def hardened_git_env(extra=None):
    """Isolate Git from inherited overrides and user/system configuration."""
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
               GIT_TERMINAL_PROMPT="0", GIT_PAGER="cat", LC_ALL="C")
    if extra:
        env.update(extra)
    return env

