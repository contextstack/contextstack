"""Deterministic host delivery for a price-watch schedule."""
import fcntl
import json
import os
import subprocess
from datetime import datetime, timezone
from pathlib import Path

from _estate_io import append_jsonl, read_jsonl, write_atomic
from _review_schedule import _attention_ids
from _sandbox_policy import wrapper_path
from _schedule_delivery import config_directory, deliver, destinations, result_markdown
from _watch_config import PREFIX, config_digest, load_watch, watch_name


class WatchError(RuntimeError):
    def __init__(self, kind, message):
        super().__init__(message)
        self.kind = kind


def fetch_price(code_root, run, name, config):
    """Fetch and extract in browser_session with just this watch's broker socket."""
    wrapper = wrapper_path(code_root)
    if wrapper is None:
        raise WatchError("watch_config", "cs-sandbox is required for price watches")
    workspace = run / "workspace"
    workspace.mkdir(parents=True, exist_ok=True, mode=0o700)
    set_name = PREFIX + name
    socket_name = "browser-" + set_name
    alias = workspace / ("browser_" + set_name.replace("-", "_"))
    alias.unlink(missing_ok=True)
    alias.symlink_to(code_root / "tools/bin/browser_session")
    write_atomic(alias.with_suffix(".json"), json.dumps({
        "runtime": "python3", "sandbox": {"net": "none", "sockets": [socket_name]},
    }))
    policy = run / "sandbox.json"
    host = json.loads((code_root / "policy/sandbox.json").read_text(encoding="utf-8"))
    host.update(allow_net=["none"], sockets={
        socket_name: str(Path.home() / ".contextstack/public-browser/sockets" / (set_name + ".sock")),
    })
    host.pop("write_env", None)
    host.pop("profiles", None)
    write_atomic(policy, json.dumps(host), mode=0o600)
    args = {"url": config["url"], "extract": config["extract"]}
    env = {**os.environ, "CS_CODE_ROOT": str(code_root)}
    env.pop("CS_BROWSER_BROKER_EXEC", None)
    try:
        child = subprocess.run([str(wrapper), "--policy", str(policy), "--cwd", str(workspace), "--", str(alias)],
                               input=json.dumps(args), capture_output=True, text=True, env=env, timeout=135)
    except subprocess.TimeoutExpired as error:
        raise WatchError("retryable", "browser_session timed out") from error
    if child.returncode:
        try:
            failure = json.loads(child.stdout)
        except ValueError:
            failure = None
        if isinstance(failure, dict) and failure.get("error_kind") == "timeout":
            raise WatchError("retryable", "browser broker timed out")
        raise WatchError("fetch_failed", (child.stderr or "browser_session failed").strip()[-500:])
    page = json.loads(child.stdout)
    status = page.get("status")
    if isinstance(status, (float, int)) and 500 <= status < 600:
        raise WatchError("retryable", "HTTP %s" % status)
    if not isinstance(status, (float, int)) or not 200 <= status < 300:
        raise WatchError("fetch_failed", "Unexpected HTTP status %s" % status)
    return page


def below_or_above(price, threshold):
    return price < threshold["below"] if "below" in threshold else price > threshold["above"]


def evidence_entry(code_root, run, name, config, history):
    page = fetch_price(code_root, run, name, config)
    stamp = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
    identity = config_digest(config)
    evidence = {"currency": config["currency"], "fetched_at": stamp, "page_sha256": page["sha256"]}
    if page.get("error_kind") == "layout_changed":
        alert = history[-1]["alert"] if history and history[-1]["kind"] == "layout_changed" else "new"
        return {"kind": "layout_changed", "config_sha256": identity, "evidence": evidence,
                "message": "The extractor found no parseable price.", "alert": alert}
    evidence["price"] = page["price"]
    previous = next((row for row in reversed(history) if row["kind"] == "price"), None)
    threshold = config["threshold"]
    crossed = (previous is not None and previous["config_sha256"] == identity and
               not below_or_above(previous["evidence"]["price"], threshold) and
               below_or_above(evidence["price"], threshold))
    return {"kind": "price", "config_sha256": identity, "evidence": evidence,
            "message": "Price %s %s%s." % (evidence["price"], evidence["currency"],
                                           " crossed the threshold" if crossed else ""),
            "alert": "new" if crossed else None}


def deliver_price_watch(code_root, state, schedule, slot):
    if not schedule["id"].startswith(PREFIX):
        raise WatchError("watch_config", "Price watch schedule ID must start with price-watch-")
    name = watch_name(schedule["id"].removeprefix(PREFIX))
    watch = state / "watches" / name
    watch.mkdir(parents=True, exist_ok=True, mode=0o700)
    run_id = schedule["id"] + "@" + slot.replace(":", "")
    run = state / "runs" / run_id
    relative_report = "watches/%s/latest.md" % name
    attention = state / "attention.jsonl"
    history_path = watch / "history.jsonl"
    with (watch / "watch.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        history = read_jsonl(history_path, strict=True)
        entry = next((row for row in history if row["slot"] == slot), None)
        if entry is None:
            try:
                config = load_watch(name)
            except (OSError, ValueError) as error:
                raise WatchError("watch_config", str(error)) from error
            if config["origin"] != schedule.get("watch_origin"):
                raise WatchError("watch_config", "Watch origin changed. Re-enable the schedule and restart the broker.")
            entry = evidence_entry(code_root, run, name, config, history)
            if entry["alert"] == "new":
                entry["alert"] = run_id
            entry["slot"] = slot
            entry["delivery"] = destinations(config["delivery"])
            # The browser watch runs without a model or a metered service.
            entry["cost_usd"] = 0
            append_jsonl(history_path, entry, sort_keys=True, compact=False, sync=True, encoding="utf-8")
            history.append(entry)
        evidence = entry["evidence"]
        result = evidence if entry["kind"] == "price" else {
            **evidence, "error_kind": entry["kind"], "retryable": False,
        }
        write_atomic(run / "workspace/_evidence/price-watch.json", json.dumps(result, sort_keys=True) + "\n",
                     mode=0o600)
        markdown = result_markdown("# Price watch · %s\n\n%s\n\nFetched %s." %
                                   (name, entry["message"], evidence["fetched_at"]),
                                   {"cost_usd": entry.get("cost_usd", 0)})
        if entry is history[-1]:
            write_atomic(state / relative_report, markdown, mode=0o600)
        alert = entry["alert"]
        if alert and alert not in _attention_ids(attention):
            append_jsonl(attention, {
                "event": "attention", "id": alert, "kind": "needs_you",
                "question": "Price watch %s. %s" % (name, entry["message"]),
                "report": relative_report, "schedule_id": schedule["id"], "slot": slot,
                "ts": evidence["fetched_at"],
            }, sort_keys=True, compact=False, sync=True, encoding="utf-8")
        if entry is history[-1]:
            deliver(entry.get("delivery", []), markdown, run_id, config_directory(), schedule_id=schedule["id"])
    return {"kind": entry["kind"], "evidence": str((run / "workspace/_evidence/price-watch.json").relative_to(state))}
