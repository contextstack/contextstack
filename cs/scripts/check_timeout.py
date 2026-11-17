"""Scale regression orchestration waits, never the behavior being asserted."""
import math
import os


def timeout(seconds):
    scale = float(os.environ.get("CHECK_TIMEOUT_SCALE", "1"))
    if not math.isfinite(scale) or scale <= 0:
        raise ValueError("CHECK_TIMEOUT_SCALE must be finite and positive")
    return seconds * scale


if __name__ == "__main__":
    timeout(1)  # scripts/check: refuse a malformed CHECK_TIMEOUT_SCALE before any rule uses it
