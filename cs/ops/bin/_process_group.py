"""Stop a subprocess started in its own session, including its children."""
import os
import signal
import subprocess
import time


def stop_group(process, grace=0.25):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    # Keep the group leader unreaped during the grace interval.
    time.sleep(grace)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        # macOS can reject a group signal when only zombies remain.
        remaining = subprocess.run(["/usr/bin/pgrep", "-g", str(process.pid)], timeout=2,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if remaining.returncode != 1:
            raise
