#!/usr/bin/env python3
"""suspended ops caps; always measure and warn.

The commit hook records indexed Ops-cap-state on every rules commit.
Cap-debt markers are optional. Historical enforcement helpers remain for
caps-enforce, which requires a minuted new opening balance before enabling.
"""
import argparse
import json
import os
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path

CAPS = {"executables": 43, "lines": 12040}  # amendment #3, the owner 2026-09-09, measured repair tree after hunt
SUSPENDED = True  # the owner 2026-09-09; caps-enforce requires a new minuted opening balance
RULES_COMMITS = 10
FOOTER = "Ops-cap-state: "


def git(code_root, *args):
    return subprocess.check_output(["git", *args], cwd=code_root)


def measure(code_root, ref):
    """Same depth-one blobs, modes and newline count as the original measure."""
    blobs = []
    if ref == "worktree":
        for path in (code_root / "ops/bin").glob("*"):
            if path.is_symlink():
                blobs.append((path.name, "120000", os.readlink(path).encode()))
            elif path.is_file():
                mode = "100755" if path.stat().st_mode & 0o111 else "100644"
                blobs.append((path.name, mode, path.read_bytes()))
    else:
        if ref == "index":
            listing = git(code_root, "ls-files", "--stage", "-z", "--", "ops/bin")
        else:
            listing = git(code_root, "ls-tree", "-z", ref, "ops/bin/")
        for entry in listing.split(b"\0"):
            if not entry:
                continue
            header, name = entry.split(b"\t", 1)
            fields = header.split()
            if name.count(b"/") != 2:
                continue
            if ref == "index":
                mode, oid, stage = fields
                if stage != b"0":
                    raise ValueError("unmerged ops index")
            else:
                mode, kind, oid = fields
                if kind != b"blob":
                    continue
            blobs.append((os.fsdecode(name).rsplit("/", 1)[1], mode.decode(),
                          git(code_root, "cat-file", "blob", oid.decode())))
    raw = b"".join(content for name, _, content in blobs if not name.endswith(".json"))
    executables = {name for name, mode, _ in blobs if mode == "100755"}
    sidecars = {name[:-5] for name, _, _ in blobs if name.endswith(".json")}
    return {"ops_bin_executables": len(executables),
            "ops_bin_pairs": len(executables & sidecars),
            "ops_bin_non_json_files": sum(not name.endswith(".json") for name, _, _ in blobs),
            "ops_bin_non_test_lines": raw.count(b"\n"),
            "ops_bin_code_lines": sum(bool(line.strip()) and not line.lstrip().startswith(b"#")
                                      for line in raw.splitlines())}


def counts(measured):
    return {"executables": measured["ops_bin_executables"],
            "lines": measured["ops_bin_non_test_lines"]}


def instant(value):
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    return parsed.replace(tzinfo=timezone.utc) if parsed.tzinfo is None else parsed


def prune_time(code_root, *, indexed=False):
    # run-prune's last-prune is storage housekeeping, not an estate review.
    # The record is state; once the ledger has left the checkout there is no
    # index copy, and no record at all means no review since the epoch.
    import sys
    sys.path.insert(0, str(Path(code_root) / "ops/bin"))
    from _estate_io import state_dir
    path = state_dir(code_root) / "ops-prune-review.json"
    reviewed = instant(json.loads(path.read_text())["reviewed_at"]) if path.is_file() else instant("1970-01-01T00:00:00Z")
    if indexed and (Path(code_root) / "ledger").is_dir():
        try:
            staged = json.loads(git(code_root, "show", ":./ledger/ops-prune-review.json"))
            reviewed = max(reviewed, instant(staged["reviewed_at"]))
        except subprocess.CalledProcessError:
            pass
    return reviewed


def previous(code_root, ref="HEAD"):
    # No sidecar or post-commit receipt can be lost between execution and record.
    sha = git(code_root, "log", "-1", "--first-parent", "--format=%H", ref, "--", "ops/bin").decode().strip()
    if not sha:
        return {key: 0 for key in CAPS}, {}
    measured = counts(measure(code_root, sha))
    if all(measured[key] <= cap for key, cap in CAPS.items()):
        return measured, {}
    message = git(code_root, "show", "-s", "--format=%B", sha).decode()
    footers = [line[len(FOOTER):] for line in message.splitlines() if line.startswith(FOOTER)]
    if not footers and message.startswith("chore: estate rules"):
        prior_counts, prior_debt = previous(code_root, sha + "^")
        if prior_debt and all(measured[k] <= prior_counts[k] for k in CAPS):
            # A subtractive automatic sweep lost metadata, not authority.
            # Preserve the proven origin; unmarked growth remains refused.
            return measured, prior_debt
    if len(footers) != 1:
        raise ValueError("over-cap committed ops have no unique debt record")
    record = json.loads(footers[0])
    if record["counts"] != measured:
        raise ValueError("committed debt record does not match ops tree")
    debt = record["debt"]
    # Legacy footers retain their origin; migration never starts a new allowance.
    for unit, entry in debt.items():
        if "origin_parent" not in entry:
            origin = None
            for commit in git(code_root, "log", "--first-parent", "--format=%H", sha, "--", "ops/bin").decode().splitlines():
                body = git(code_root, "show", "-s", "--format=%B", commit).decode()
                matched = False
                for line in body.splitlines():
                    if line.startswith(FOOTER):
                        prior = json.loads(line[len(FOOTER):]).get("debt", {}).get(unit, {})
                        if prior.get("opened") == entry["opened"]:
                            origin, matched = commit, True
                if not matched:
                    break
            if origin is None:
                raise ValueError("debt origin cannot be resolved")
            entry["origin_parent"] = git(code_root, "rev-parse", origin + "^").decode().strip()
    return measured, debt


def debt_status(code_root, debt, prune, *, indexed=False, message=None):
    import runpy
    is_rules = runpy.run_path(str(code_root / "scripts/estate_classify.py"))["is_rules"]
    pending = indexed and any(is_rules(p) for p in git(code_root, "diff", "--cached", "--name-only", "--relative").decode().splitlines()) and (message is None or not (message.splitlines() or [""])[0].startswith("chore:"))
    result = {}
    for unit, record in debt.items():
        anchor = record["origin_parent"]
        git(code_root, "merge-base", "--is-ancestor", anchor, "HEAD")
        log_output = git(code_root, "log", "--first-parent", "--diff-merges=first-parent", "--format=%x00%s", "--name-only", "--relative", anchor + "..HEAD").decode()
        blocks = log_output.split("\0")
        elapsed = 0
        for block in blocks[1:-1]:  # skip the empty prefix and the final (opening commit) block
            lines = block.splitlines()
            if not lines:
                continue
            subject = lines[0]
            if subject.startswith("chore:"):
                continue
            paths = lines[1:] if len(lines) > 1 else []
            if any(is_rules(p) for p in paths if p):
                elapsed += 1
        elapsed = max(0, elapsed) + int(pending)
        result[unit] = {"rules_commits": elapsed, "remaining": max(0, RULES_COMMITS - elapsed),
                        "expired": elapsed >= RULES_COMMITS or bool(prune and prune >= instant(record["opened"]))}
    return result


def requested(message):
    result = {}
    for line in message.splitlines():
        if not line.startswith("cap-debt:"):
            continue
        match = re.fullmatch(r"cap-debt: \+([1-9][0-9]*) (lines|executables) (\S.*)", line)
        if not match or match[2] in result:
            raise ValueError("cap-debt requires +N lines|executables and a repayment obligation, once per unit")
        result[match[2]] = int(match[1])
    if result and not re.search(r"(?m)^(Demanded-by:|Incident:)\s*\S.*$", message):
        raise ValueError("cap-debt requires a named Demanded-by or named Incident")
    return result


def assess(current, before, debt, now, prune, message=None, *, rules_commits=0):
    wanted = requested(message) if message is not None else {}
    result = {}
    used = {}
    for unit, cap in CAPS.items():
        excess = max(0, current[unit] - cap)
        if not excess:
            continue
        old_excess = max(0, before[unit] - cap)
        record = debt.get(unit) if old_excess else None
        if old_excess and not record:
            raise ValueError(f"{unit} debt record missing")
        if record:
            opened = instant(record["opened"])
            if rules_commits >= 10 or (prune and prune >= opened):
                raise ValueError(f"{unit} debt expired at tenth authored rules commit or prune; repay or obtain a minuted amendment")
        increase = max(0, excess - old_excess)
        if increase:
            if wanted.get(unit) != increase:
                raise ValueError(f"{unit} {current[unit]} exceeds cap {cap}; requires cap-debt: +{increase} {unit} <repayment obligation>")
            used[unit] = increase
        if not record:
            record = {"opened": now.isoformat()}
        else:
            record = {k: v for k, v in record.items() if k != "due"}
        result[unit] = record
    if wanted != used:
        raise ValueError("cap-debt units must exactly name new measured excess, not unused allowance")
    return result




def check(code_root, ref, message_path=None):
    current = counts(measure(code_root, ref))
    message = message_path.read_text() if message_path else None
    if SUSPENDED:
        print("check: WARN — ops cap suspended: " + json.dumps(current, sort_keys=True))
        if message_path:
            lines = [line for line in message.splitlines() if not line.startswith(FOOTER)]
            lines += ["", FOOTER + json.dumps({"counts": current, "suspended": True}, sort_keys=True)]
            message_path.write_text("\n".join(lines) + "\n")
        return
    before, debt = previous(code_root)
    prune = prune_time(code_root, indexed=message_path is not None) if any(
        current[key] > cap for key, cap in CAPS.items()) else None
    status = debt_status(code_root, debt, prune, indexed=message_path is not None, message=message)
    elapsed = max((status[unit]["rules_commits"] for unit in debt if current[unit] > CAPS[unit]), default=0)
    active = assess(current, before, debt, datetime.now(timezone.utc), prune, message, rules_commits=elapsed)
    for record in active.values():
        record.setdefault("origin_parent", git(code_root, "rev-parse", "HEAD").decode().strip())
    for unit, record in active.items():
        print(f"check: WARN — ops cap: {current[unit] - CAPS[unit]} {unit} debt; {status.get(unit, {}).get('rules_commits', 0)}/10 subsequent authored rules commits; due at tenth or next prune")
    if message_path:
        # Never trust caller-supplied state or renew the existing deadline.
        lines = [line for line in message.splitlines() if not line.startswith(FOOTER)]
        if active:
            lines += ["", FOOTER + json.dumps({"counts": current, "debt": active}, sort_keys=True)]
        message_path.write_text("\n".join(lines) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("check", "commit-msg", "measure"))
    parser.add_argument("value", nargs="?")
    parser.add_argument("--paths", type=int)
    args = parser.parse_args()
    code_root = Path(__file__).resolve().parents[1]  # code_root may be a subfolder of its repo
    try:
        if args.mode == "measure":
            ref = args.value or "HEAD"
            sha = git(code_root, "rev-parse", ref).decode().strip()
            if args.paths is not None and args.paths < 0:
                raise ValueError("decision path count must be nonnegative")
            print(json.dumps({"code_commit": sha, **measure(code_root, sha),
                              "independent_decision_paths": args.paths}))
        else:
            check(code_root, "index" if args.mode == "commit-msg" else "worktree",
                  Path(args.value) if args.mode == "commit-msg" else None)
        return 0
    except (ValueError, KeyError, TypeError, OSError, subprocess.CalledProcessError) as error:
        if SUSPENDED and args.mode == "check":
            print(f"check: WARN — ops cap measurement unavailable: {error}")
            return 0
        print(f"check: FAIL — ops cap measurement: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
