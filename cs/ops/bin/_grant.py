#!/usr/bin/env python3
"""Shared grant loader. Not an op. enqueue and schedule paths import this."""
import fnmatch
import re
from datetime import datetime, timezone
from pathlib import Path
from _estate_io import estate_id, estate_matches, state_dir  # dev grants continue on workstation through the alias

HERE = Path(__file__).resolve().parent
CODE_ROOT = HERE.parent.parent
GRANTS = CODE_ROOT / "grants"
STATUSES = {"active", "revoked"}
RFC3339_UTC = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|\+00:00)$"
)
INT = re.compile(r"^-?(0|[1-9]\d*)$")


def rel_path(raw, prefix=False):
    if not isinstance(raw, str):
        return None
    rel = raw.strip()
    if rel.startswith("./"):
        rel = rel[2:]
    if prefix:
        rel = rel.rstrip("/")
    if not rel or rel.startswith("/") or rel == ".." or rel.startswith("../"):
        return None
    parts = rel.split("/")
    if any(part in ("", ".", "..") for part in parts):
        return None
    return rel


def covers_path(path, prefixes):
    rel = rel_path(path)
    if rel is None:
        return False
    for raw in prefixes or []:
        p = rel_path(raw, prefix=True)
        if not p:
            continue
        if any(mark in p for mark in "*?[") and fnmatch.fnmatchcase(rel, p):
            return True
        if rel == p or rel.startswith(p + "/"):
            return True
    return False


def resolved(raw):
    path = Path(str(raw or "").strip())
    if not str(path):
        return None
    if not path.is_absolute():
        path = CODE_ROOT / path
    return path.resolve()


def same_repo(left, right):
    a, b = resolved(left), resolved(right)
    return a is not None and b is not None and a == b


def parse_expiry(raw):
    if not isinstance(raw, str):
        return None
    text = raw.strip()
    if not RFC3339_UTC.match(text):
        return None
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        stamp = datetime.fromisoformat(text)
    except ValueError:
        return None
    if stamp.utcoffset() != timezone.utc.utcoffset(stamp):
        return None
    return stamp.astimezone(timezone.utc)


def skip_ws(s, i):
    while i < len(s) and s[i] in " \t":
        i += 1
    return i


def parse_double(s, i):
    i += 1
    out = []
    while i < len(s):
        c = s[i]
        if c == '"':
            return "".join(out), i + 1
        if c == "\\" and i + 1 < len(s):
            nxt = s[i + 1]
            out.append({"n": "\n", "t": "\t", "r": "\r", "\\": "\\", '"': '"'}.get(nxt, nxt))
            i += 2
            continue
        out.append(c)
        i += 1
    return None, i


def parse_single(s, i):
    i += 1
    out = []
    while i < len(s):
        c = s[i]
        if c == "'" and i + 1 < len(s) and s[i + 1] == "'":
            out.append("'")
            i += 2
            continue
        if c == "'":
            return "".join(out), i + 1
        out.append(c)
        i += 1
    return None, i


def unquoted_scalar(raw):
    text = raw.strip()
    if text == "true":
        return True
    if text == "false":
        return False
    if text in ("null", "~"):
        return None
    if INT.fullmatch(text):
        return int(text)
    return text


def parse_quoted_or_plain(s, i, stop):
    i = skip_ws(s, i)
    if i >= len(s):
        return None, i
    if s[i] == '"':
        return parse_double(s, i)
    if s[i] == "'":
        return parse_single(s, i)
    start = i
    while i < len(s) and s[i] not in stop:
        if s[i] == "#" and (i == start or s[i - 1] in " \t"):
            break
        i += 1
    return unquoted_scalar(s[start:i]), i


def parse_flow_list(s, i):
    if i >= len(s) or s[i] != "[":
        return None, i
    i += 1
    items = []
    i = skip_ws(s, i)
    if i < len(s) and s[i] == "]":
        return items, i + 1
    while True:
        i = skip_ws(s, i)
        if i >= len(s) or s[i] in "[{":
            return None, i
        val, i = parse_quoted_or_plain(s, i, ",]")
        if val is None and (i >= len(s) or s[i] not in ",]"):
            return None, i
        items.append(val)
        i = skip_ws(s, i)
        if i >= len(s):
            return None, i
        if s[i] == ",":
            i += 1
            continue
        if s[i] == "]":
            return items, i + 1
        return None, i


def parse_value(text):
    s = str(text or "")
    i = skip_ws(s, 0)
    if i >= len(s) or s[i] == "#":
        return None, "empty"
    if s[i] == "[":
        items, i = parse_flow_list(s, i)
        if items is None:
            return None, "bad list"
        i = skip_ws(s, i)
        if i < len(s) and s[i] != "#":
            return None, "trailing"
        return items, None
    if s[i] in "{":
        return None, "mapping"
    val, i = parse_quoted_or_plain(s, i, "")
    if s[i:].lstrip() and not s[i:].lstrip().startswith("#"):
        # quoted value already consumed; leftover must be comment or empty
        rest = s[i:].lstrip()
        if rest and not rest.startswith("#"):
            return None, "trailing"
    return val, None


def parse_grant(text):
    out = {
        "id": None, "estate": None, "repo": None, "paths": None, "gates": None,
        "effects": None, "commit": None, "expiry": None, "status": None,
    }
    current = None
    block = {"paths": False, "gates": False, "effects": False}
    for raw in str(text or "").splitlines():
        if raw.strip() in ("---", "..."):
            continue
        if current in ("paths", "gates", "effects") and raw[:1] in " \t" and raw.strip():
            item = raw.strip()
            if item.startswith("#"):
                continue
            if not item.startswith("-"):
                return None, "grant %s not a list" % current
            if block[current] == "flow":
                return None, "grant %s mixed list syntax" % current
            val, err = parse_value(item[1:])
            if err or val is None:
                return None, "grant %s empty item" % current
            out[current].append(val)
            block[current] = "block"
            continue
        line = raw
        if not line.strip() or line.strip().startswith("#"):
            continue
        if ":" not in line or line[:1] in " \t":
            continue
        key, value = line.split(":", 1)
        key = key.strip()
        current = key
        if key not in out:
            continue
        if key in ("paths", "gates", "effects"):
            stripped = value.strip()
            if not stripped or stripped.startswith("#"):
                out[key] = []
                block[key] = "block"
                continue
            val, err = parse_value(value)
            if err or not isinstance(val, list):
                return None, "grant %s not a list" % key
            out[key] = val
            block[key] = "flow"
            continue
        val, err = parse_value(value)
        if err == "empty":
            out[key] = None
            continue
        if err:
            return None, "grant %s invalid" % key
        out[key] = val
    return out, None


def load(gid):
    gid = str(gid or "").strip()
    if not gid or ".." in gid or "/" in gid or gid in (".", ".."):
        return None, "bad grant id"
    path = None
    for ext in (".yaml", ".yml"):
        candidate = GRANTS / (gid + ext)
        if candidate.is_file():
            path = candidate
            break
    if path is None:
        return None, "grant not found"
    return load_file(path, gid)


def load_file(path, gid):
    """A grant from a given file: <code>/grants/<id>.yaml (load), or the copy a director job carried
    in <state>/jobs/<job>/grants/ (the director issues, the node enforces)."""
    gid = str(gid or "").strip()
    if not gid or ".." in gid or "/" in gid or gid in (".", ".."):
        return None, "bad grant id"
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError:
        return None, "grant not found"
    grant, err = parse_grant(text)
    if err:
        return None, err
    if not isinstance(grant.get("id"), str) or grant.get("id") != gid:
        return None, "grant id mismatch"
    if not isinstance(grant.get("repo"), str) or not grant["repo"].strip():
        return None, "grant repo missing"
    if not isinstance(grant.get("commit"), bool):
        return None, "grant commit missing"
    if grant.get("status") not in STATUSES:
        return None, "grant status invalid"
    raw_paths = grant.get("paths")
    if not isinstance(raw_paths, list) or not raw_paths:
        return None, "grant paths empty"
    if any(not isinstance(p, str) for p in raw_paths):
        return None, "grant path invalid"
    paths = [rel_path(p, prefix=True) for p in raw_paths]
    if any(p is None for p in paths):
        return None, "grant path invalid"
    grant["paths"] = paths
    raw_gates = grant.get("gates")
    if not isinstance(raw_gates, list) or not raw_gates:
        return None, "grant gates empty"
    if any(not isinstance(g, str) or not g.strip() for g in raw_gates):
        return None, "grant gates empty"
    grant["gates"] = [g.strip() for g in raw_gates]
    raw_effects = grant.get("effects")
    if raw_effects is None:
        grant["effects"] = []
    elif (not isinstance(raw_effects, list) or
          any(not isinstance(effect, str) or
              not re.fullmatch(r"[a-z][a-z0-9_]{0,63}", effect)
              for effect in raw_effects) or
          len(raw_effects) != len(set(raw_effects))):
        return None, "grant effects invalid"
    if parse_expiry(grant.get("expiry")) is None:
        return None, "grant expiry invalid"
    return grant, None


def live_error(grant, commit=False):
    if not grant:
        return "grant not found"
    if grant.get("estate") is not None and not estate_matches(CODE_ROOT, grant["estate"]):
        return "grant estate mismatch"
    if grant.get("status") != "active":
        return "revoked"
    expiry = parse_expiry(grant.get("expiry"))
    if expiry is None or expiry <= datetime.now(timezone.utc):
        return "expired"
    if commit and grant.get("commit") is not True:
        return "grant.commit is not true"
    return ""


def remedy(code, grant=None, gid=""):
    """Every refusal carries a remedy clause (Fable 2026-09-23)."""
    grant = grant or {}
    name = str(grant.get("id") or gid or "").strip() or "grant"
    try:
        estate = estate_id(CODE_ROOT)
    except (ValueError, OSError, FileNotFoundError):
        estate = "?"
    if code == "grant estate mismatch":
        ge = str(grant.get("estate") or "").strip() or "?"
        return "grant %s is estate: %s; this estate is %s; run on %s." % (name, ge, estate, ge)
    if code == "expired":
        return "grant %s expired; renew by tag." % name
    if code in ("grant not found", "grant id mismatch"):
        return "grant id %s unregistered; create a grant." % (gid or name)
    if code == "revoked":
        return "grant %s revoked; renew by tag." % name
    if code == "grant estate required":
        return "grant %s missing estate; renew by tag." % name
    if code == "repo mismatch":
        return "grant %s repo mismatch; renew by tag." % name
    if code == "grant.commit is not true":
        return "grant %s commit false; renew by tag." % name
    if code.startswith("path not in grant:") or code.startswith("effect not in grant:"):
        return "%s; renew by tag." % code
    if code.startswith("grant "):
        return "%s; renew by tag." % code
    return "%s; renew by tag." % code


def record_refusal(code_root, *, code, message, grant=None, gid="", context=None):
    """Append a refusal receipt; return its id for factory_gap evidence."""
    from _estate_io import append_jsonl
    import time
    code_root = Path(code_root)
    shelf = state_dir(code_root) / "refusals.jsonl"
    shelf.parent.mkdir(parents=True, exist_ok=True)
    rid = str(time.time_ns())
    row = {
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "id": rid,
        "event": "enqueue_refused",
        "code": code,
        "message": message,
        "grant": str((grant or {}).get("id") or gid or ""),
    }
    if context:
        row["context"] = context
    append_jsonl(shelf, row, sort_keys=False, compact=False, sync=True, encoding="utf-8")
    return rid


def cover(gid, repo=None, paths=None, commit=False, effects=None):
    grant, err = load(gid)
    if err:
        return False, remedy(err, grant=grant or {}, gid=gid), grant or {}
    err = live_error(grant, commit=commit)
    if err:
        return False, remedy(err, grant=grant, gid=gid), grant
    if repo:
        if not same_repo(repo, grant["repo"]):
            return False, remedy("repo mismatch", grant=grant, gid=gid), grant
    for rel in paths or []:
        if not covers_path(rel, grant["paths"]):
            return False, remedy("path not in grant: " + rel, grant=grant, gid=gid), grant
    for effect in effects or []:
        if effect not in grant["effects"]:
            return False, remedy("effect not in grant: " + effect, grant=grant, gid=gid), grant
    return True, "", grant


def schedule_error(schedule):
    """Check schedule authority before preparing any destination or enqueueing."""
    gid = schedule.get("grant")
    grant, error = load(gid)
    if error:
        return remedy(error, grant=grant or {}, gid=gid)
    if not grant.get("estate"):
        return remedy("grant estate required", grant=grant, gid=gid)
    node = schedule.get("node")
    if node:  # a schedule fired at a fleet node is authority for that node, whose ESTATE is its director name
        if grant["estate"] != node:
            return "grant %s is estate: %s; this schedule targets node %s." % (gid, grant["estate"], node)
        if schedule.get("root"):
            return "schedule %s: a node schedule cannot name a destination root (no clone workspace yet)." % schedule.get("id")
        error = live_error(dict(grant, estate=None))
        return remedy(error, grant=grant, gid=gid) if error else ""
    if not estate_matches(CODE_ROOT, grant["estate"]):
        return remedy("grant estate mismatch", grant=grant, gid=gid)
    error = live_error(grant, commit=bool(schedule.get("root")))
    if error:
        return remedy(error, grant=grant, gid=gid)
    if schedule.get("root") and not same_repo(schedule["root"], grant["repo"]):
        return remedy("repo mismatch", grant=grant, gid=gid)
    return ""
