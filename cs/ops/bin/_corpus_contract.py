#!/usr/bin/env python3
"""Shared corpus-task metadata, write-set checks, and tagged install cache."""
import fnmatch

from _workspace_setup import (
    INSTALLS,
    clone_tree,
    freeze_tree,
    install_tag,
    metadata,
    patterns,
    seed_installs,
    thaw_tree,
)


def allowed(path, globs):
    return any(glob == "." or fnmatch.fnmatchcase(path, glob) for glob in globs)


def rejected(paths, globs):
    return sorted(path for path in paths if not allowed(path, globs))
