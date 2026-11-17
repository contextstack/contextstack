"""Shared wording for node-restart orphaned jobs (follow-up)."""

NODE_RESTART_REASON = "node restarted while the job ran"
ORPHANED_RUNNING_STOP = "error: scheduler orphaned_running after restart"
ORPHANED_RUNNING_DETAIL = "scheduler orphaned_running after restart"


def orphan_error():
    return {
        "class": "node_restart",
        "code": "orphaned_running",
        "source": "launcher",
        "retryable": True,
        "detail": ORPHANED_RUNNING_DETAIL,
    }
