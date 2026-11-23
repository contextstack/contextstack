# Shared by tools/bin/* — not a tool. Appends one JSON line per call so
# hooks/bin/doom-loop can see same-tool+same-args without runner growth.
import json
import os

LOG = ".cs-calls.jsonl"


def destination():
    """Permit a direct batch parent to isolate composed child-call evidence."""
    candidate = os.environ.get("CS_BATCH_CHILD_CALLLOG", "")
    parent = os.environ.get("CS_BATCH_PARENT_PID", "")
    if candidate == ".cs-batch-child-calls.jsonl" and parent == str(os.getppid()):
        return candidate
    return LOG


def record(name, args):
    try:
        with open(destination(), "a", encoding="utf-8") as f:
            f.write(json.dumps({"name": name, "args": args}, sort_keys=True, default=str) + "\n")
    except OSError:
        pass
