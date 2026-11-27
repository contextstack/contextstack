"""Host-side delivery of scheduled results to owner-selected destinations."""
import hashlib
import json
import math
import os
import secrets
import sys
from contextlib import ExitStack
from pathlib import Path
from urllib.parse import quote

from _estate_io import read_jsonl
from _job_reason import redact


def config_directory():
    return Path(os.environ.get("CS_CONFIG_ROOT") or Path.home() / ".contextstack/config")


def destinations(value):
    if value == "attention":
        return []
    if not isinstance(value, list):
        raise ValueError("delivery must be attention or an array of destinations")
    for target in value:
        if target == "attention":
            continue
        if (not isinstance(target, dict) or len(target) != 1 or
                not target.keys() <= {"conversation", "file"} or
                any(not isinstance(v, str) or not v.strip() or "\x00" in v for v in target.values())):
            raise ValueError("delivery destination needs a conversation ID or file path")
    return [target for target in value if target != "attention"]


def result_markdown(markdown, run):
    cost = run.get("cost_usd")
    estimated = bool(run.get("unmetered") and run.get("est_basis") == "api_list")
    if cost is None and run.get("est_basis") not in (None, "unmetered", "unreported"):
        cost, estimated = run.get("est_usd"), True
    shown = "unavailable"
    if type(cost) in (int, float) and math.isfinite(cost) and cost >= 0:
        shown = "$%s (%s)" % (cost, "estimated" if estimated else "actual")
    return "%s\n\nCost: %s\n" % (markdown.rstrip(), shown)


def owner_config(config_dir):
    path = config_dir / "delivery.json"
    try:
        config = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {}
    if not isinstance(config, dict):
        raise ValueError("delivery.json must be an object")
    return config


def file_destination(root, raw_path):
    if not isinstance(root, str) or not Path(root).is_absolute():
        raise ValueError("file delivery requires an absolute owner-configured file_root")
    root = Path(root).resolve(strict=True)
    if not root.is_dir():
        raise ValueError("file_root must be a directory")
    path = (root / raw_path).resolve()
    if not path.is_relative_to(root) or path == root:
        raise ValueError("delivery path is outside file_root")
    return root, path.relative_to(root)


def write_result(root, relative, markdown):
    # Pin the parent directories so a replaced symlink cannot redirect the write.
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    with ExitStack() as stack:
        directory = os.open(root, flags)
        stack.callback(os.close, directory)
        for part in relative.parts[:-1]:
            try:
                os.mkdir(part, mode=0o700, dir_fd=directory)
            except FileExistsError:
                pass
            directory = os.open(part, flags, dir_fd=directory)
            stack.callback(os.close, directory)
        temp = ".delivery-" + secrets.token_hex(12)
        fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=directory)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as result:
                result.write(markdown)
                result.flush()
                os.fsync(result.fileno())
            os.replace(temp, relative.name, src_dir_fd=directory, dst_dir_fd=directory)
        finally:
            try:
                os.unlink(temp, dir_fd=directory)
            except FileNotFoundError:
                pass


def deliver(targets, markdown, identity, config_dir, *, schedule_id=None, source=None):
    if not targets:
        return
    config = owner_config(config_dir)
    paths = {target["file"]: file_destination(config.get("file_root"), target["file"])
             for target in targets if "file" in target}
    director = None
    body = {"message": markdown}
    if source:
        body.update(source=source, trust="untrusted")
    for target in targets:
        if "file" in target:
            root, relative = paths[target["file"]]
            write_result(root, relative, markdown)
        else:
            if director is None:
                scripts = str(Path(__file__).resolve().parents[2] / "scripts")
                if scripts not in sys.path:
                    sys.path.insert(0, scripts)
                from cs_mcp import Director, Refused
                install = json.loads((config_dir / "install.json").read_text(encoding="utf-8"))
                url = install.get("director")
                if not url:
                    raise ValueError("conversation delivery requires a configured Director")
                director = Director(url, config_dir.parent / "state/director/local-credential")
                if schedule_id and install.get("node"):
                    body["schedule_key"] = "%s/%s" % (install["node"], schedule_id)
            conversation = target["conversation"]
            key = hashlib.sha256(json.dumps([identity, conversation]).encode()).hexdigest()
            try:
                status, _ = director.request(
                    "POST", "conversations/%s/notes" % quote(conversation, safe=""),
                    body=body, key="delivery-" + key[:48],
                )
            except Refused as error:
                raise RuntimeError(str(error)) from error
            if status not in (200, 201):
                raise RuntimeError("Director conversation delivery returned HTTP %s" % status)


def job_destinations(schedule, config_dir):
    schedules = owner_config(config_dir).get("schedules", {})
    if not isinstance(schedules, dict):
        raise ValueError("delivery.json schedules must be an object")
    return destinations(schedules.get(schedule, "attention"))


def deliver_job(job, run, runs_dir):
    """Deliver a terminal scheduled job using the owner's per-schedule destinations."""
    schedule = job.get("schedule_id")
    if not schedule:
        return
    config_dir = config_directory()
    targets = job_destinations(schedule, config_dir)
    if targets:
        identity = str(job["id"])
        markdown = "# Scheduled job · %s\n\nJob %s: %s." % (
            schedule, identity, run.get("stop") or job["state"],
        )
        if run.get("verdict"):
            markdown += "\n\nVerdict: %s." % run["verdict"]
        run_id = job.get("run_id")
        if run_id and Path(run_id).name == run_id and run_id not in (".", ".."):
            events = read_jsonl(runs_dir / run_id / "messages.jsonl", strict=False)
            reply = next((event["text"] for event in reversed(events)
                          if event.get("event") == "turn" and isinstance(event.get("text"), str)
                          and event["text"].strip()), "")
            if reply:
                text = redact(reply).encode("utf-8")
                markdown += "\n\n" + text[:180_000].decode("utf-8", errors="ignore")
                if len(text) > 180_000:
                    markdown += "\n\n[Result truncated]"
        deliver(targets, result_markdown(markdown, run), identity, config_dir, schedule_id=schedule)
