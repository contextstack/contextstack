"""Temporary regression trees excluded from Spotlight indexing."""
import tempfile


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
