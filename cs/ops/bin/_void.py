"""Config/credential void classification and schedule hold. Not an op."""
import fcntl
import hashlib
import json
import re
from pathlib import Path
from _estate_io import append_jsonl, read_jsonl, state_dir, utc_time

HERE = Path(__file__).resolve().parent
LAB = HERE.parent.parent
DEFAULT_SCHEDULES = state_dir(LAB) / "schedules.jsonl"
LOCK = state_dir(LAB) / "schedule.lock"

CONFIG_VOID = re.compile(
    r"(?:gateway\.)?(no_provider_credential|provider_unsupported|unknown_model|capture-stimulus|env_policy|roots|sandbox_policy)"
)


def config_void_reason(row):
    stop = str((row or {}).get("stop") or "")
    match = CONFIG_VOID.search(stop)
    return match.group(1) if match else ""


def is_config_void(row):
    row = row or {}
    if str(row.get("verdict") or "") != "void":
        return False
    return bool(config_void_reason(row))


def schedule_rows(path):
    current = {}
    rows = read_jsonl(path, strict=True)
    repairs = {(row.get('repairs_revision_sha'), row['id']): index
               for index, row in enumerate(rows)
               if row.get('event') == 'schedule_updated' and row.get('repairs_revision_sha')}
    for index, row in enumerate(rows):
        if row['id'] in current and row.get('event') != 'schedule_updated':
            digest = hashlib.sha256(json.dumps(row, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
            if repairs.get((digest, row['id']), -1) <= index:
                raise ValueError('duplicate schedule id without a revision event')
        current[row['id']] = row
    return list(current.values())


def hold_schedule(schedule_id, reason, schedules=None, lock=None):
    """Append a disabled revision; never rewrite historical schedule rows."""
    if not schedule_id or not reason:
        return False
    path = Path(schedules or DEFAULT_SCHEDULES)
    lock_path = Path(lock or LOCK)
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a") as handle:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        for row in schedule_rows(path):
            if row.get("id") == schedule_id:
                append_jsonl(path, {**row, "event": "schedule_updated", "enabled": False, "hold": reason},
                             sort_keys=False, compact=True, sync=True, encoding="utf-8")
                return True
    return False


def repeated_failure(schedule, dispatch):
    """Count distinct jobs, first terminal wins; any different result breaks the streak."""
    threshold = schedule.get("failure_hold_after", 3)
    if type(threshold) is not int or threshold < 2:
        raise ValueError("failure_hold_after must be an integer >= 2")
    seen, tail = set(), []
    enabled_at = utc_time(schedule.get("enabled_at") or schedule.get("ts"))
    for row in dispatch:
        job = str(row.get("id") or row.get("job") or "")
        if not job or row.get("state") not in {"done", "error", "operator_close", "cancelled"}:
            continue
        if job in seen:
            continue
        seen.add(job)
        stamp = utc_time(row.get("ts"))
        if enabled_at and stamp and stamp < enabled_at:
            continue
        if row.get("schedule_id") == schedule["id"]:
            tail.append(row)
    tail = tail[-threshold:]
    if len(tail) != threshold:
        return ""
    signature = (tail[0].get("verdict"), tail[0].get("stop"))
    if signature[0] not in {"fail", "void"} or not signature[1]:
        return ""
    if all((r.get("verdict"), r.get("stop")) == signature for r in tail):
        return "repeated_terminal_failure:" + str(signature[1])
    return ""
