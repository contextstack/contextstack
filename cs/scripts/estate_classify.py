"""Shared path classification for estate-commit and the check-red gate.

Every non-ignored path under lab/ is facts or rules. classify never
returns a third bucket. Hook `is_rules` is classify == rules, so
estate-commit cannot --no-verify a hook-rules path. Automatic commits
still skip .go / internal/ / cmd/ (human-gated) without calling them
other.
"""

from functools import lru_cache
import json
import os
from pathlib import Path
import re

LAB = Path(os.environ.get("LAB_ESTATE_ROOT") or Path.cwd())


def receipted_paths(lab):
    lab = Path(lab)
    shelves = (lab / "ledger/director-delivery.jsonl", lab / "ledger/finalizations.jsonl",
               lab / "ledger/dispatch.jsonl")
    stamp = tuple((p.stat().st_mtime_ns, p.stat().st_size) if p.exists() else None for p in shelves)
    return frozenset(name for name in _receipted_paths(str(lab), stamp)
                     if not any((lab / part).is_symlink() for part in (Path(name), *Path(name).parents))
                     and (not name.startswith("specs/archive/") or (lab / name).is_file()))


@lru_cache(maxsize=8)
def _receipted_paths(root, stamp):
    lab = Path(root)
    import sys
    sys.path.insert(0, str(lab / "ops/bin"))
    from _estate_io import estate_dir  # an install's ESTATE is in its config root
    estate_file = estate_dir(lab) / "ESTATE"
    estate = estate_file.read_text().strip() if estate_file.exists() else ""
    def rows(name):
        path = lab / "ledger" / name
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    journal = rows("director-delivery.jsonl")
    completed = {r.get("request_id"): r for r in journal
                 if r.get("event") == "delivery_completed" and r.get("operation") == "delivery-author"
                 and r.get("result", {}).get("ok") is True and r.get("estate") == estate}
    paths, cells = set(), {}
    for row in journal:
        cell = row.get("cell") or {}
        sid = str(cell.get("id") or "")
        receipt = completed.get(row.get("request_id"), {})
        if (row.get("event") != "cell_authored" or row.get("estate") != estate or not estate
                or not re.fullmatch(r"exp-[0-9]+-flowchart-delivery", sid)
                or not row.get("request_id") or not row.get("intent_id")
                or not re.fullmatch(r"[a-f0-9]{64}", str(row.get("request_digest") or ""))
                or not row.get("actor_id") or row.get("actor_kind") not in ("human", "automated")
                or not cell.get("grant") or not re.fullmatch(r"[a-f0-9]{64}", str(cell.get("grant_digest") or ""))
                or any(receipt.get(k) != row.get(k) for k in ("actor_id", "actor_kind", "request_digest", "intent_id"))
                or receipt.get("result", {}).get("cell") != cell
                or cell.get("task") != "tasks/work/" + sid or cell.get("spec") != "specs/" + sid + ".yaml"):
            continue
        cells[sid] = (cell, row["intent_id"])
        paths.add(cell["spec"])
        paths.update(cell["task"] + "/" + name for name in ("goal.md", "check", "notes.md"))
    finalizations = rows("finalizations.jsonl")
    # A granted session's dispatch row is the estate-scoped receipt for the
    # spec it authored. Once that exact root spec has an archive, both names
    # are facts: the archive is retained and the live source may be deleted.
    for row in rows("dispatch.jsonl"):
        source = row.get("spec")
        grant = row.get("grant")
        if (row.get("estate") != estate or not estate
                or not isinstance(grant, str) or not grant.strip()
                or not isinstance(source, str)
                or not re.fullmatch(r"specs/[a-zA-Z0-9_-]+[.]yaml", source)):
            continue
        archive = "specs/archive/" + Path(source).name
        if (lab / archive).is_file():
            paths.update((archive, source))
    # Earlier land versions expose provenance through the Director receipt.
    # Accept only its exact cell archive and a settled, prepared finalization.
    for row in journal:
        cell, intent = cells.get(row.get("cell_id"), ({}, None))
        result = row.get("result") or {}
        judgment = result.get("judgment") or {}
        fid = judgment.get("finalization_id")
        if (row.get("event") != "delivery_completed" or row.get("operation") != "delivery-archive"
                or row.get("estate") != estate or not row.get("actor_id")
                or row.get("actor_kind") not in ("human", "automated")
                or not cell or row.get("intent_id") != intent or result.get("ok") is not True
                or not fid or not judgment.get("judgment_id")
                or result.get("spec") != "specs/archive/" + Path(cell["spec"]).name):
            continue
        prepared = [r for r in journal if r.get("event") == "finalization_prepared"
                    and r.get("estate") == estate and r.get("intent_id") == intent
                    and r.get("cell_id") == cell["id"] and (r.get("payload") or {}).get("finalization_id") == fid]
        outcomes = [r for r in finalizations if r.get("event") == "finalize_outcome"
                    and r.get("estate") == estate and r.get("finalization_id") == fid]
        if len(prepared) == len(outcomes) == 1 and outcomes[0].get("state") in ("applied", "failed", "no_destination_effect"):
            paths.add(result["spec"])
    # Land records archive provenance after the checked judgment owns the spec.
    for row in finalizations:
        path = str(row.get("spec") or "")
        if (row.get("event") == "spec_archived" and row.get("estate") == estate
                and (row.get("judgment_id") or row.get("closure_id")) and row.get("actor_id")
                and re.fullmatch(r"specs/archive/[a-zA-Z0-9_-]+[.]yaml", path)):
            source = "specs/" + Path(path).name
            if row.get("source_spec") == source:
                paths.update((path, source))
            elif (re.fullmatch(r"tasks/(?:[a-zA-Z0-9_-]+/)+[a-zA-Z0-9_-]+[.]yaml", str(row.get("source_spec") or ""))
                  and Path(row["source_spec"]).name == Path(path).name):
                paths.add(path)
    return frozenset(paths)


FACTS_PREFIXES = (
    "specs/archive/",
    "specs/sweeps/",
    "tasks/judgments/",
    "prompts/packs/",
    "docs/dev/",
)


def _norm(path):
    return path.replace("\\", "/")


def classify(path):
    path = _norm(path)
    if path in receipted_paths(LAB):
        return "facts"
    if path.startswith(FACTS_PREFIXES):
        return "facts"
    if path.startswith("ledger/") and path.endswith(".jsonl") and path.count("/") == 1:
        return "facts"
    if path == "ledger/CHANGELOG.md":
        return "facts"
    return "rules"


def is_rules(path):
    return classify(path) == "rules"


def skip_auto(path):
    path = _norm(path)
    return path.endswith(".go") or path.startswith(("internal/", "cmd/"))
