"""Temporary regression trees excluded from Spotlight indexing."""
import os
import tempfile


def short_tmp():
    """A writable temp parent short enough for Unix socket fixtures.

    Order: a short TMPDIR (a lane check's private scratch), then /tmp when it can really be
    written (the host), then any writable TMPDIR (an agent session inside the sandbox, where
    /tmp is denied; socket paths may then be long, but ordinary fixtures work).
    """
    tmp = os.environ.get("TMPDIR")
    usable = bool(tmp) and os.path.isdir(tmp) and os.access(tmp, os.W_OK | os.X_OK)
    if usable and len(os.fsencode(os.path.realpath(tmp))) <= 40:
        return tmp
    try:
        probe = tempfile.mkdtemp(prefix=".short-tmp-probe-", dir="/tmp")
    except OSError:
        return tmp if usable else "/tmp"
    os.rmdir(probe)
    return "/tmp"


def scratch_dir(prefix="tmp", **kwargs):
    """Like TemporaryDirectory, using system temp unless a fixture needs a parent."""
    return tempfile.TemporaryDirectory(prefix=prefix, suffix=".noindex", **kwargs)


def clone_tree(src, dst):
    """Copy a fresh fixture tree, preserving symlinks; clone on APFS if available."""
    import os
    import shutil
    import subprocess
    import sys

    if sys.platform == "darwin" and not os.path.lexists(dst):
        os.makedirs(os.path.dirname(os.path.abspath(dst)), exist_ok=True)
        result = subprocess.run(["/bin/cp", "-c", "-R", str(src), str(dst)],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if result.returncode == 0:
            return str(dst)
        if os.path.lexists(dst):
            shutil.rmtree(dst)
    return shutil.copytree(src, dst, symlinks=True)
