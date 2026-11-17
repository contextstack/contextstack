"""Interactive dispatch ordering, with bounded preference over waiting batch work."""
import calendar
import time


BATCH_AGE_S = 300
LANE_RANK = {"production": 0, "manager": 1, "probe": 2, "shadow": 3}


def is_turn(row):
    # Identity comes from Director job fields, never the submitted bundle.
    return bool(row.get("conversation_id") and row.get("turn_id"))


def seat_candidates(pending, running, turn_only=False):
    """One extra job-process seat, reserved for Director-owned conversation turns.

    Called under dispatch.lock, before recording a running row. Normal model
    capacity remains the gateway's concern, not a dispatcher process-count cap.
    """
    conversations = {r.get("conversation_id") for r in running if is_turn(r)}
    if turn_only and any(r.get("dispatch_seat") == "reserved" for r in running):
        return []
    return [r for r in pending
            if (not is_turn(r) or r.get("conversation_id") not in conversations)
            and (not turn_only or (is_turn(r) and r.get("director_job")
                                  and r.get("origin") == "director"))]


def select(pending, now=None):
    now = time.time() if now is None else now

    def rank(row):
        turn = is_turn(row)
        try:
            queued = calendar.timegm(time.strptime(row["ts"], "%Y-%m-%dT%H:%M:%SZ"))
        except (KeyError, ValueError, TypeError):
            queued = now
        aged = not turn and now - queued >= BATCH_AGE_S
        return (0 if aged else 1 if turn else 2,
                queued if aged else LANE_RANK.get(str(row.get("lane") or "production"), 0))

    return min(pending, key=rank) if pending else None
