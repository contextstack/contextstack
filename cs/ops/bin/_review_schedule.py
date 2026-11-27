"""Weekly lane review as a host schedule. Not an op.

`cs review schedule` appends one row to <state>/schedules.jsonl. schedule-tick already
fires that file (node-listen and tick). The row is not a runner cell: command
review-weekly, cron Monday 08:00 in timezone "local", missed catch_up. A row without
missed would take node-listen's missed_default skip and drop the week when the machine
was asleep. catch_up runs the latest missed Monday once.

Delivery is one report at <state>/reviews/<local date>.md and one attention item in
<state>/attention.jsonl (kind needs_you) whose report field is that path. A lead
conversation's needs_you needs a bound conversation this host job does not have, so the
shelf is the attention item. Both writes are idempotent per local date.

CS_REVIEW_WEEKLY_HOOK, when set, is a file used as the report text (the regression).
Unset, the text is `cs review weekly` against the Director.
"""
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

from _estate_io import append_jsonl
from _schedule_delivery import config_directory, deliver, job_destinations, result_markdown

SCHEDULE_ID = "weekly-review"
COMMAND = "review-weekly"
CRON = "0 8 * * 1"


def local_zone_name():
    """IANA name for the machine's local zone. TZ wins, then /etc/localtime, else UTC."""
    tz = os.environ.get("TZ") or ""
    if tz and tz not in ("localtime", "local"):
        try:
            ZoneInfo(tz)
            return tz
        except (KeyError, ValueError, OSError):
            pass
    try:
        target = os.path.realpath("/etc/localtime")
    except OSError:
        target = ""
    marker = "zoneinfo/"
    if marker in target:
        name = target.split(marker, 1)[1]
        try:
            ZoneInfo(name)
            return name
        except (KeyError, ValueError, OSError):
            pass
    key = getattr(datetime.now().astimezone().tzinfo, "key", None)
    if isinstance(key, str) and key:
        return key
    return "UTC"


def zone_of(name):
    if not name or name == "local":
        name = local_zone_name()
    return ZoneInfo(name)


def slot_date(schedule, slot):
    """The schedule's local calendar date for a UTC slot (the week key)."""
    instant = datetime.fromisoformat(str(slot).replace("Z", "+00:00"))
    return instant.astimezone(zone_of(schedule.get("timezone"))).date().isoformat()


def _latest(path):
    latest = None
    if not path.is_file():
        return None
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(row, dict) and row.get("id") == SCHEDULE_ID:
            latest = row
    return latest


def _current(row):
    return bool(row) and row.get("enabled") is True and row.get("cron") == CRON \
        and row.get("missed") == "catch_up" and row.get("command") == COMMAND \
        and row.get("timezone") == "local" and row.get("kind") == "host"


def review_row():
    return {
        "id": SCHEDULE_ID,
        "enabled": True,
        "cron": CRON,
        "timezone": "local",
        "missed": "catch_up",
        "kind": "host",
        "command": COMMAND,
        "lane": "host",
        "event": "schedule_updated",
        "actor_id": "ops:cs-review",
        "actor_kind": "automated",
        "ts": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }


def install_schedule(state):
    """Append the weekly-review row when the live one is not already that definition."""
    from _node_schedules import schedule_lock

    state = Path(state)
    state.mkdir(parents=True, mode=0o700, exist_ok=True)
    path = state / "schedules.jsonl"
    with schedule_lock(state):
        if _current(_latest(path)):
            return {"installed": False, "already": True, "id": SCHEDULE_ID, "state": str(state)}
        append_jsonl(path, review_row(), sort_keys=True, compact=False, sync=True, encoding="utf-8")
    return {"installed": True, "already": False, "id": SCHEDULE_ID, "state": str(state)}


def _write_new(path, text):
    """Create path at 0600. If it already exists, leave it (one report per date)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        path.parent.chmod(0o700)
    except OSError:
        pass
    if path.is_file():
        return False
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as out:
        out.write(text if text.endswith("\n") else text + "\n")
    return True


def review_text(now):
    """Markdown for this week. The hook file is the regression's stand-in for the Director."""
    hook = os.environ.get("CS_REVIEW_WEEKLY_HOOK") or ""
    if hook:
        path = Path(hook)
        if not path.is_file():
            raise RuntimeError("CS_REVIEW_WEEKLY_HOOK is not a file")
        return path.read_text(encoding="utf-8")
    scripts = Path(__file__).resolve().parents[2] / "scripts"
    if str(scripts) not in sys.path:
        sys.path.insert(0, str(scripts))
    from cs_review import collect_weekly_review  # noqa: E402
    from _review_collect import resolve_director  # noqa: E402
    from _review_render import render_markdown  # noqa: E402

    director = resolve_director()
    stamp = now.timestamp() if isinstance(now, datetime) else None
    report = collect_weekly_review(director, since_days=7.0, now=stamp)
    text = render_markdown(report)
    try:
        token = director.token()
    except Exception:
        token = ""
    if token and len(token) >= 8:
        text = text.replace(token, "[REDACTED]")
    return text


def _attention_ids(path):
    found = set()
    if not path.is_file():
        return found
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(row, dict) and row.get("id"):
            found.add(row["id"])
    return found


def ensure_attention(state, attention_id, date, report_rel, slot, now):
    path = Path(state) / "attention.jsonl"
    if attention_id in _attention_ids(path):
        return False
    stamp = now.astimezone(timezone.utc).isoformat().replace("+00:00", "Z") if isinstance(now, datetime) else \
        datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    append_jsonl(path, {
        "event": "attention",
        "id": attention_id,
        "kind": "needs_you",
        "question": "Weekly lane review for %s is ready: %s" % (date, report_rel),
        "report": report_rel,
        "schedule_id": SCHEDULE_ID,
        "slot": slot,
        "ts": stamp,
    }, sort_keys=True, compact=False, sync=True, encoding="utf-8")
    return True


def deliver_weekly(state, schedule, slot, now):
    """Write the week's report and one attention item. Safe to call again for the same slot."""
    date = slot_date(schedule, slot)
    report_rel = "reviews/%s.md" % date
    report = Path(state) / report_rel
    if not report.is_file():
        _write_new(report, result_markdown(review_text(now), {"cost_usd": 0}))
    attention_id = "%s@%s" % (SCHEDULE_ID, date)
    ensure_attention(state, attention_id, date, report_rel, slot, now)
    config_dir = config_directory()
    targets = job_destinations(SCHEDULE_ID, config_dir)
    if targets:
        markdown = report.read_text(encoding="utf-8")
        if not markdown.endswith("\n\nCost: $0 (actual)\n"):
            markdown = result_markdown(markdown, {"cost_usd": 0})
        deliver(targets, markdown, attention_id, config_dir, schedule_id=SCHEDULE_ID)
    return {"job": "review:%s" % date, "report": report_rel, "attention_id": attention_id}
