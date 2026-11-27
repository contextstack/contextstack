"""Poll, inspect, and execute one-use hosted land requests on the node.

The optional node policy/land-retry.json lists exact check rule messages in
known_flaky and maps timeout-eligible rule messages to standalone argv in
isolated_checks. Both default to empty.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

import _lanes as lanes
from _director_client import DirectorUnreachable
from _job_reason import redact
from _land_record import _body, _now, post_land_record
from _node_control import write_json
from _lane_policy import lane_changed_names, refuse_agent_configs

FAILURE = "check: FAIL — "
FAILURE_OUTPUT_END = "check: FAIL output kept: "
OID = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")


def inspect(repo, lane):
    cfg = lanes.repo_config(repo)
    lanes.check_lane(cfg, lane)
    main = cfg["path"]
    branch = "refs/heads/lane/" + lane
    git_out = lanes.workspace_out
    commit = git_out(main, "rev-parse", "--verify", branch + "^{commit}")
    base = git_out(main, "rev-parse", "--verify", "refs/heads/" + cfg["branch"] + "^{commit}")
    if git_out(main, "rev-parse", "HEAD") != base:
        raise lanes.LaneError("main checkout is on a different branch")
    names = lane_changed_names(git_out, main, commit)
    tracing = any(p.casefold().split("/", 1)[0] == "tracing" for p in names)
    protected = False
    hooks = lanes.git(main, "config", "--get", "core.hooksPath").stdout.strip()
    try:
        refuse_agent_configs(main, names, git_out, lanes.LaneError, lanes.repos_file(),
                             branch=commit, hooks_paths=(hooks,))
    except lanes.LaneError:
        protected = True
    return {"commit": commit, "base": base, "protected": protected, "tracing": tracing}


def retry_rule(result, flaky, isolated_pass):
    """Require a restored main and a single identified failing check rule."""
    if result.get("status") != "red" or result.get("stage") not in ("gate", "commit") or not result.get("main_unchanged"):
        return False
    rules = [line.removeprefix(FAILURE) for line in result.get("failed", []) if line.startswith(FAILURE)]
    if len(rules) != 1:
        return False
    rule = rules[0]
    if rule in flaky:
        return True
    output = result.get("output_tail", "").splitlines()
    # The check prints each failed rule's captured output between these markers.
    for index, line in enumerate(output):
        if line != FAILURE + rule:
            continue
        for end in range(index + 1, len(output)):
            if output[end].startswith(FAILURE_OUTPUT_END):
                rule_output = "\n".join(output[index + 1:end])
                return bool(re.search(r"\b(?:TimeoutExpired|timed out)\b", rule_output, re.I)
                            and isolated_pass(rule))
            if output[end].startswith(FAILURE):
                break
    return False


def isolated_pass(rule, configs, row):
    """An owner-supplied standalone check for this rule, run from the pinned lane."""
    argv = configs.get(rule)
    if not isinstance(argv, list) or not argv or not all(isinstance(arg, str) for arg in argv):
        return False
    try:
        cfg = lanes.repo_config(row["repo"])
        lane = row["lane"]
        cwd = lanes.lane_path(cfg, lane)
        candidate = inspect(row["repo"], lane)
        if any(candidate[key] != row[key] for key in ("commit", "base")):
            return False
        if lanes.lane_out(cfg["path"], lane, cwd, "status", "--porcelain"):
            return False
        return subprocess.run(argv, cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL, timeout=120,
                              env=lanes.clean_env()).returncode == 0
    except (OSError, ValueError, subprocess.TimeoutExpired, lanes.LaneError):
        return False


def land(code, row):
    if not all(OID.fullmatch(row.get(key, "")) for key in ("commit", "base")):
        return {"status": "refused", "reason": "the request has no valid commit pin"}
    argv = [sys.executable, "-I", str(code / "scripts/cs"), "land", row["repo"], row["lane"],
            "--json", "--request-pin", row["commit"], row["base"]]
    if row.get("decided_by"):
        argv.append("--request-owner")
        if row["protected"]:
            argv.append("--allow-agent-config")
    try:
        done = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, text=True)
        value = json.loads(done.stdout)
        if not isinstance(value, dict) or (done.returncode == 0) != (value.get("status") == "landed"):
            raise ValueError("unexpected cs land result")
        return value
    except (OSError, ValueError):
        return {"status": "refused", "reason": "cs land did not return a valid result"}


def execute(client, code, row, receipt_path, policy):
    receipt = {"row": row, "attempts": []}
    for attempt in range(2):
        receipt["in_flight"] = True
        write_json(receipt_path, receipt)
        result = land(code, row)
        if not attempt and result.get("status") == "refused" and result.get("busy") is True:
            receipt.update(in_flight=False, busy=True)
            write_json(receipt_path, receipt)
            break
        result.update(repo=row["repo"], lane=row["lane"])
        landed = _body(result, landed_by="hosted-lead", landed_at=_now())
        if landed["reason"]:
            landed["reason"] = redact(landed["reason"])[:4000]
        receipt["attempts"].append(landed)
        receipt["in_flight"] = False
        write_json(receipt_path, receipt)
        if attempt or not retry_rule(result, policy.get("known_flaky", []),
                                     lambda rule: isolated_pass(rule, policy.get("isolated_checks", {}), row)):
            break
    report(client, receipt_path, receipt)


def report(client, path, receipt):
    row, attempts = receipt["row"], receipt["attempts"]
    route = "land-requests/" + row["id"] + "/outcome"
    if receipt.get("busy"):
        client.call("POST", route, {"status": "busy", "claim_revision": row["revision"]})
        path.unlink()
        return
    for number, attempt in enumerate(attempts, 1):
        status, detail = post_land_record(client, row["target_run_id"], attempt,
                                          "%s:%d" % (row["id"], number))
        if status == "error":
            raise DirectorUnreachable(detail)
    status = ("interrupted" if receipt["in_flight"] else
              "succeeded" if attempts[-1]["state"] == "landed" else "failed")
    outcome = {"status": status, "attempts": attempts}
    client.call("POST", route, outcome)
    path.unlink()


def poll(client, state, code, config):
    directory = state / "node/land-requests"
    for path in sorted(directory.glob("act_*.json")):
        report(client, path, json.loads(path.read_text()))
    try:
        policy = json.loads((config / "policy/land-retry.json").read_text())
    except (OSError, ValueError):
        policy = {}
    if not isinstance(policy, dict):
        policy = {}
    flaky = policy.get("known_flaky")
    commands = policy.get("isolated_checks")
    policy = {"known_flaky": flaky if isinstance(flaky, list) else [],
              "isolated_checks": commands if isinstance(commands, dict) else {}}
    for row in client.call("GET", "land-requests").get("land_requests", []):
        prefix = "land-requests/" + row["id"]
        if row["status"] == "pending":
            try:
                evidence = inspect(row["repo"], row["lane"])
            except (lanes.LaneError, ValueError, OSError):
                client.call("POST", prefix + "/unavailable", {})
                continue
            row = client.call("POST", prefix + "/inspect", evidence)
        if row["status"] == "approved":
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            try:
                row = client.call("POST", prefix + "/claim", {})
            except DirectorUnreachable as error:
                if error.status == 409:
                    continue
                raise
            execute(client, code, row, directory / (row["id"] + ".json"), policy)
