"""The estate-cutover-v2 record of a state root renamed to a new estate (dev -> m4, 2026-11-11). Not an op:
ops/bin/estate-cutover writes it, scripts/estate-identity.py and `estate-cutover {mode: verify}` verify it. Stdlib only.

<state>/estate-cutover-v2.json = {schema: "estate-cutover-v2", estate (this estate, e.g. m4), aliases (earlier names
it continues, e.g. ["dev"]), state_root (the real path it was written in), previous ({schema, estate, sha256} of the
state root's estate-cutover-v1 record, or null), copy (state-cutover's cutover.json {inventory_sha256, ts, files,
bytes} when the root was made by it, else null), historical_prefixes ({shelf: {bytes, sha256}} for every *.jsonl
directly in the state root at cutover time), ts}. Rows inside a prefix keep the estate they were written with; rows
after it must say this estate (or nothing). The record is immutable: <config>/estate-cutover-v2.sha256 holds the
sha256 of its bytes, written once with it, so a rewritten record no longer matches its anchor.
"""
import hashlib
import json
from pathlib import Path

from _estate_io import CUTOVER_V2

ANCHOR = "estate-cutover-v2.sha256"


def sha256_bytes(raw):
    return hashlib.sha256(raw).hexdigest()


def shelves(state):
    return sorted(p for p in Path(state).glob("*.jsonl") if p.is_file() and not p.is_symlink())


def prefixes(state):
    """{shelf: {bytes, sha256}}; ValueError when a shelf does not end on a row boundary."""
    found = {}
    for path in shelves(state):
        raw = path.read_bytes()
        if raw and not raw.endswith(b"\n"):
            raise ValueError("%s does not end on a row boundary" % path.name)
        found[path.name] = {"bytes": len(raw), "sha256": sha256_bytes(raw)}
    return found


def verify(state, config, ident):
    """Problems with the state root's v2 record against this estate ([] when it holds)."""
    state, config = Path(state), Path(config)
    path = state / CUTOVER_V2
    if not path.is_file():
        return ["no %s in the state root" % CUTOVER_V2]
    raw = path.read_bytes()
    record = json.loads(raw)
    problems = []
    anchor = config / ANCHOR
    if not anchor.is_file() or anchor.read_text().strip() != sha256_bytes(raw):
        problems.append("%s does not match its anchor %s: the record is immutable" % (CUTOVER_V2, anchor))
    if record.get("schema") != "estate-cutover-v2" or record.get("estate") != ident:
        problems.append("the record is for %r, this estate is %r" % (record.get("estate"), ident))
    previous = record.get("previous")
    v1 = state / "estate-cutover.json"
    if previous and (not v1.is_file() or sha256_bytes(v1.read_bytes()) != previous.get("sha256")):
        problems.append("the estate-cutover-v1 record changed or vanished")
    marked = record.get("historical_prefixes") or {}
    have = {p.name: p for p in shelves(state)}
    problems += ["historical shelf removed: %s" % name for name in sorted(set(marked) - set(have))]
    for name, path in have.items():
        raw = path.read_bytes()
        size = (marked.get(name) or {}).get("bytes", 0)
        want = (marked.get(name) or {}).get("sha256", sha256_bytes(b""))
        if size > len(raw) or sha256_bytes(raw[:size]) != want:
            problems.append("%s: historical bytes changed" % name)
            continue
        for number, line in enumerate(raw[size:].splitlines(), start=raw[:size].count(b"\n") + 1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except ValueError:
                problems.append("%s:%d: not JSON" % (name, number))
                continue
            if not isinstance(row, dict) or row.get("estate") not in (None, "", ident):
                problems.append("%s:%d: a row after the cutover must say estate %s" % (name, number, ident))
    return problems
