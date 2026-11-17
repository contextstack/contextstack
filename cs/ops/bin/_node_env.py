"""Node-local settings stay in Python memory, never in an exec environment.

Legacy node.env accepts shell-style quoted assignments, but no shell is run:
expansions and command substitutions are literal data. Children load the same
file through LAB_CONFIG_ROOT instead of inheriting its secret values.
"""
import os
import re
import shlex
from pathlib import Path

ASSIGNMENT = re.compile(r"\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$")


def node_env(path=None):
    """Read assignments without evaluating shell code or mutating os.environ."""
    if path is None:
        config = Path(os.environ.get("LAB_CONFIG_ROOT") or Path(__file__).resolve().parents[2])
        path = config / "node.env"
    try:
        text = Path(path).read_text(encoding="utf-8")
    except FileNotFoundError:
        return {}
    values = {}
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        match = ASSIGNMENT.fullmatch(line)
        if not match:
            raise ValueError("node.env: invalid assignment on line %d" % number)
        try:
            words = shlex.split(match[2], comments=True, posix=True)
        except ValueError:
            raise ValueError("node.env: invalid quoting on line %d" % number) from None
        if len(words) > 1:
            raise ValueError("node.env: unquoted whitespace on line %d" % number)
        values[match[1]] = words[0] if words else ""
    return values
