#!/usr/bin/env python3
"""check append-only history and estate identity at the cutover."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ops/bin"))
from _estate_io import estate_id
sys.path.insert(0, str(Path(__file__).resolve().parent))
from _atomic_write import hardened_git_env


def shelves(lab):
    return [p for p in sorted((lab / "ledger").glob("*.jsonl"))
            if subprocess.run(["git", "-C", str(lab), "check-ignore", "-q", "--", str(p)],
                              capture_output=True, env=hardened_git_env()).returncode != 0]


def snapshot(lab):
    """Record byte prefixes once, immediately before the cutover commit."""
    prefixes = {}
    for path in shelves(lab):
        raw = path.read_bytes()
        prefixes[path.name] = {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
    return prefixes


def fresh_history(lab):
    """True only for an explicit local cutover with no inherited ledger history."""
    marker = lab / "ledger/estate-cutover.json"
    if not marker.is_file():
        return False
    cutover = json.loads(marker.read_text())
    return (cutover.get("schema") == "estate-cutover-v1"
            and cutover.get("estate") == estate_id(lab)
            and cutover.get("historical_prefixes") == {})


def check_state(lab):
    """A relocated state root with an estate-cutover-v2 record: verified against its anchor in the config
    root, never against git (the state root is not versioned)."""
    from _estate_cutover import verify
    from _roots import roots
    found = roots(lab)
    ident = estate_id(found["config"])
    problems = verify(found["state"], found["config"], ident)
    if problems:
        raise ValueError("; ".join(problems[:5]))
    return {"ok": True, "estate": ident, "state": str(found["state"]), "record": "estate-cutover-v2"}


def check(lab):
    if os.environ.get("LAB_STATE_ROOT"):
        from _estate_io import CUTOVER_V2, state_dir
        if (state_dir(lab) / CUTOVER_V2).is_file():
            return check_state(lab)
    if not (lab / "ledger").is_dir():  # the ledger left the checkout: its identity is checked where it lives
        return {"ok": True, "skip": "no lab/ledger: the estate's ledger lives under its state root"}
    ident = estate_id(lab)
    marker = lab / "ledger/estate-cutover.json"
    cutover = json.loads(marker.read_text())
    if cutover.get("estate") != ident or cutover.get("schema") != "estate-cutover-v1":
        raise ValueError("cutover and ESTATE disagree")
    # Once committed, the historical allowance cannot be enlarged or rewritten.
    prior = subprocess.run(["git", "-C", str(lab), "show", "HEAD:./ledger/estate-cutover.json"],
                           capture_output=True, env=hardened_git_env())
    if prior.returncode == 0 and json.loads(prior.stdout) != cutover:
        raise ValueError("committed estate cutover is immutable")
    prefixes = cutover["historical_prefixes"]
    if ident == "prod-1" and prefixes:
        raise ValueError("prod-1 must start with empty ledger history")
    paths = {p.name: p for p in shelves(lab)}
    if set(prefixes) - paths.keys():
        raise ValueError("historical ledger shelf removed")
    for name, path in paths.items():
        raw = path.read_bytes()
        committed = subprocess.run(["git", "-C", str(lab), "show", "HEAD:./ledger/" + name], capture_output=True, env=hardened_git_env())
        if committed.returncode == 0 and not raw.startswith(committed.stdout):
            raise ValueError(f"{name}: committed rows changed or removed")
        prefix = prefixes.get(name, {"bytes": 0, "sha256": hashlib.sha256(b"").hexdigest()})
        size = prefix["bytes"]
        if not isinstance(size, int) or size < 0 or size > len(raw) or hashlib.sha256(raw[:size]).hexdigest() != prefix["sha256"]:
            raise ValueError(f"{name}: historical bytes changed")
        if size and raw[size-1:size] != b"\n":
            raise ValueError(f"{name}: cutover is not a row boundary")
        for number, line in enumerate(raw[size:].splitlines(), start=raw[:size].count(b"\n")+1):
            if not line.strip():
                continue
            row = json.loads(line)
            if not isinstance(row, dict):
                raise ValueError(f"{name}:{number}: new row requires an object")
            marked = row.get("estate")
            # Omit = this ledger (cutover rule). Never backfill. Foreign is refused.
            if marked not in (None, "", ident):
                raise ValueError(f"{name}:{number}: new row requires estate {ident}")
    return {"ok": True, "estate": ident, "historical_shelves": len(prefixes), "shelves": len(paths)}


if __name__ == "__main__":
    try:
        print(json.dumps(check(Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1])))
    except (OSError, ValueError, KeyError, TypeError) as error:
        sys.exit("estate identity: " + str(error))
