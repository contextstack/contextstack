"""Linux init and doctor checks. Never inspect credential contents."""
import os
from pathlib import Path
import stat
import sys


sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "ops/bin"))
try:
    from _linux_sandbox import finding, landlock_abi, probe, sandbox_findings  # noqa: E402,F401
finally:
    sys.path.pop(0)


def credential_findings(root, spec, *, create=False):
    if spec == "off":
        return
    directory = Path(spec) if spec else root / "credentials"
    if not directory.is_absolute():
        yield finding("gateway credentials", False, "Linux supports only the file credential store",
                      "set CSGATEWAY_CREDENTIALS to an absolute directory or leave it empty")
        return
    try:
        if create and not os.path.lexists(directory):
            directory.mkdir(mode=0o700, exist_ok=True)
        if not create and not os.path.lexists(directory):
            return  # The gateway creates the default store on first write.
        check = private_path(directory, 0o700, stat.S_ISDIR)
        yield check
        if check["ok"]:
            for path in sorted(directory.iterdir()):
                yield private_path(path, 0o600, stat.S_ISREG)
    except OSError as error:
        yield finding("gateway credentials", False, str(error), "check access to %s" % directory)


def private_path(path, want, valid_type):
    meta = path.lstat()
    mode = stat.S_IMODE(meta.st_mode)
    ok = valid_type(meta.st_mode) and meta.st_uid == os.getuid()
    required = 0o500 if want == 0o700 else 0o400
    private_mode = mode & 0o7077 == 0 and mode & required == required
    return finding("mode %s" % path, ok and private_mode, "%s is mode %04o" % (path, mode),
                   "chmod %o %s" % (want, path) if ok else
                   "use an owned %s at %s (no symlinks)" % ("directory" if want == 0o700 else "regular file", path))
