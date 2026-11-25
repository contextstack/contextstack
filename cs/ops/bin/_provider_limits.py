"""Known provider usage-limit signals per coding CLI: stop reason, allowance, node state. Not an op."""
import json
import os
import calendar
import math
import re
import time
from datetime import datetime, timezone
from pathlib import Path

from _estate_io import append_jsonl, write_atomic

from _est import iter_objs

CURSOR_LIMIT = re.compile(r"(?:ActionRequiredError:)?\s*You(?:'|')?ve hit your usage limit", re.I)
CURSOR_RESET = re.compile(r"monthly cycle ends on (\d{1,2}/\d{1,2}/\d{4}|\d{1,2}/\d{1,2}/\d{2})", re.I)
GATEWAY_QUOTA = re.compile(r"gateway\.provider_quota_exhausted", re.I)
GEMINI_INELIGIBLE = re.compile(r"IneligibleTierError|UNSUPPORTED_CLIENT", re.I)
QUOTA_TEXT = re.compile(r"(?:insufficient_quota|usage limit|rate limit exceeded|quota exceeded)", re.I)
UNKNOWN_RESET_S = 6 * 3600
LOGIN_RETRY_S = 10 * 60

STATE_NAME = "provider-limits.json"


def _parse_us_date(text):
    text = str(text or "").strip()
    for fmt in ("%m/%d/%Y", "%m/%d/%y"):
        try:
            dt = datetime.strptime(text, fmt).replace(tzinfo=timezone.utc)
            return dt.strftime("%Y-%m-%dT%H:%M:%SZ")
        except ValueError:
            continue
    return None


def _iso_epoch(value):
    if isinstance(value, (int, float)) and not isinstance(value, bool) and value > 0:
        return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(value))
    if isinstance(value, str) and value.strip():
        return value.strip()
    return None


def _clip(text, limit=500):
    text = " ".join(str(text or "").split())
    return text if len(text) <= limit else text[: limit - 3] + "..."


def _combine(stdout, stderr):
    return "\n".join(part for part in ((stdout or ""), (stderr or "")) if part)


def limit_from_event(obj):
    """One parsed stream object that signals a hard provider limit; else None."""
    if not isinstance(obj, dict):
        return None
    if obj.get("type") == "rate_limit_event":
        info = obj.get("rate_limit_info") if isinstance(obj.get("rate_limit_info"), dict) else {}
        if info.get("status") == "rejected":
            resets = _iso_epoch(info.get("resetsAt"))
            message = "Claude rate limit rejected"
            if info.get("rateLimitType"):
                message += " (%s window)" % info["rateLimitType"]
            return {"message": message, "resets_at": resets, "via": "rate_limit_event", "terminal": True}
    err = obj.get("error") if isinstance(obj.get("error"), dict) else {}
    if str(err.get("type") or "").startswith("provider."):
        return None  # OpenCode events require its dialect-specific type/status checks.
    if err and (err.get("type") == "TerminalQuotaError" or err.get("code") == 429
                or QUOTA_TEXT.search(str(err.get("message") or ""))):
        return {"message": _clip(err.get("message") or "provider quota error"), "resets_at": None,
                "via": "stream_error", "terminal": True}
    return None


def limit_signal(dialect, raw):
    """While streaming: a rate_limit rejection or quota error ends the CLI early."""
    if dialect != "claude":
        return None
    if isinstance(raw, bytes):
        raw = raw.decode("utf-8", errors="replace")
    line = (raw or "").strip()
    if not line.startswith("{"):
        return None
    try:
        obj = json.loads(line)
    except ValueError:
        return None
    hit = limit_from_event(obj)
    if not hit:
        return None
    return {"stop": "provider_limit", "reason": hit["message"], "resets_at": hit.get("resets_at"),
            "provider_limit": hit}


def detect_cursor(text):
    if not CURSOR_LIMIT.search(text):
        return None
    m = CURSOR_RESET.search(text)
    resets_at = _parse_us_date(m.group(1)) if m else None
    message = "You've hit your usage limit"
    if m:
        message += "; resets %s" % m.group(1)
    return {"message": message, "resets_at": resets_at, "via": "stderr", "terminal": True}


def detect_gateway(text):
    if not GATEWAY_QUOTA.search(text):
        return None
    return {"message": "provider quota exhausted (gateway)", "resets_at": None, "via": "gateway", "terminal": True}


def detect_gemini(text):
    if not GEMINI_INELIGIBLE.search(text):
        return None
    return {"message": "Gemini tier ineligible for this client", "resets_at": None,
            "via": "stderr", "terminal": True}


def allowance_hit(allowance):
    if not isinstance(allowance, dict) or allowance.get("rejected") is not True:
        return None
    resets_at = None
    for window in allowance.get("windows") or []:
        if not isinstance(window, dict):
            continue
        cand = window.get("resets_at")
        if cand and (resets_at is None or cand > resets_at):
            resets_at = cand
    via = str(allowance.get("via") or "allowance")
    message = "provider allowance reached (%s)" % via
    return {"message": message, "resets_at": resets_at, "via": via, "terminal": False}


def detect_opencode(stdout):
    """Only top-level native error events, never text parts quoting provider messages."""
    for line in (stdout or "").splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict) or event.get("type") != "error":
            continue
        error = event.get("error")
        if not isinstance(error, dict):
            continue
        kind = str(error.get("type") or "")
        if kind != "provider.go-limit" and not (
            error.get("status") == 429 and re.search(r"usage[._ -]?limit", kind, re.I)
        ):
            continue
        delay = error.get("retryAfterMs")
        resets = None
        if isinstance(delay, (int, float)) and not isinstance(delay, bool) and math.isfinite(delay) and delay >= 0:
            try:
                resets = _iso_epoch(time.time() + delay / 1000)
            except (OverflowError, OSError, ValueError):
                pass  # A malformed provider reset must not prevent run finalization.
        return {"message": _clip(error.get("message") or "OpenCode usage limit"), "resets_at": resets,
                "via": "opencode_error", "terminal": True}
    return None


def detect_grok(stdout, stderr):
    """Grok's own error message only; agent messages and plain stdout are untrusted."""
    def hit(message, via):
        if not isinstance(message, str) or not re.search(
            r"API error \(status 402(?: Payment Required)?\): Grok Build usage balance exhausted\b",
            message, re.I,
        ):
            return None
        return {"message": _clip(message), "resets_at": None, "via": via, "terminal": True}

    for line in _combine(stdout, stderr).splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if isinstance(event, dict) and event.get("type") == "error":
            found = hit(event.get("message"), "grok_error")
            if found:
                return found
    # The CLI also prints Internal error: {"message": ..., "http_status": 402}
    # on stderr (possibly pretty-printed). Require its error prefix before decoding.
    for match in re.finditer(r"(?m)^(?:Internal error|Error):[ \t]*", stderr or ""):
        tail = stderr[match.end():]
        try:
            error, _ = json.JSONDecoder().raw_decode(tail)
        except ValueError:
            error = None
        if isinstance(error, dict):
            message = error.get("message")
        else:
            message = tail.splitlines()[0] if tail else ""
        found = hit(message, "stderr")
        if found:
            return found
    return None


def detect_limit(template, binary, stdout, stderr, allowance=None):
    """Best limit signal for this CLI run. Preference: explicit stderr, then stream objects, then allowance.

    Prose patterns read the CLI's own stderr only, and only for that CLI: stdout carries the agent's
    work, which can quote a limit message (a lane editing this module once marked codex limited)."""
    if template == "grok" or Path(binary or "").name == "grok":
        hit = detect_grok(stdout, stderr)
        if hit:
            hit["template"] = template
        return hit
    text = _combine(stdout, stderr)
    err = stderr or ""
    name = "%s %s" % (template or "", binary or "")
    checks = [(detect_gateway, text)]
    if "cursor" in name:
        checks.insert(0, (detect_cursor, err))
    if "gemini" in name:
        checks.append((detect_gemini, err))
    for fn, source in checks:
        hit = fn(source)
        if hit:
            hit.setdefault("template", template)
            return hit
    if "opencode" in name:
        hit = detect_opencode(stdout)
        if hit:
            hit["template"] = template
            return hit
    for obj in (() if "opencode" in name else iter_objs(text)):
        hit = limit_from_event(obj)
        if hit:
            hit.setdefault("template", template)
            return hit
    if isinstance(allowance, dict) and allowance.get("rejected") is True:
        hit = allowance_hit(allowance)
        if hit:
            hit.setdefault("template", template)
            return hit
    if QUOTA_TEXT.search(err):
        hit = {"message": _clip(QUOTA_TEXT.search(err).group(0)), "resets_at": None, "via": "stderr", "terminal": True}
        hit["template"] = template
        return hit
    return None


def enrich_allowance(allowance, hit):
    """Run-row allowance with rejected set and the limit's reset time when known."""
    out = dict(allowance) if isinstance(allowance, dict) else {}
    out["rejected"] = True
    out.setdefault("via", hit.get("via") or "error")
    resets = hit.get("resets_at")
    if resets:
        windows = list(out.get("windows") or [])
        windows.append({"window": "limit", "resets_at": resets, "used_pct": None, "status": "rejected"})
        out["windows"] = windows
        out["resets_at"] = resets
    return out


def state_path(state_root):
    return Path(state_root) / STATE_NAME


def _now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def _login_changed(row):
    path = row.get("login_path")
    if not path:
        return False
    try:
        st = Path(path).stat()
    except OSError:
        return False  # still missing: keep the pause until a login appears
    return [st.st_ino, st.st_mtime_ns, st.st_size] != row.get("login_signature")


def load(state_root):
    path = state_path(state_root)
    if not path.is_file():
        return {"templates": {}}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"templates": {}}
    if not isinstance(data, dict):
        return {"templates": {}}
    templates = data.setdefault("templates", {})
    if not isinstance(templates, dict):
        templates = {}
        data["templates"] = templates
    now = time.time()
    active = {}
    for name, row in list(templates.items()):
        if not isinstance(row, dict):
            templates.pop(name, None)
            continue
        if row.get("via") == "login" and _login_changed(row):
            # The owner signed in again (the login file changed since the pause): lift it.
            templates.pop(name, None)
            continue
        resets = row.get("resets_at")
        if not resets and row.get("recorded_at") and (
                row.get("via") != "login" or row.get("login_signature") is None):
            # Retry unwatchable logins shortly; unknown quota windows get a longer grace period.
            try:
                rec = calendar.timegm(time.strptime(str(row["recorded_at"])[:19], "%Y-%m-%dT%H:%M:%S"))
                resets = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(rec + (LOGIN_RETRY_S if row.get("via") == "login" else UNKNOWN_RESET_S)))
            except ValueError:
                resets = None
        if resets:
            try:
                ts = calendar.timegm(time.strptime(resets[:19], "%Y-%m-%dT%H:%M:%S"))
            except ValueError:
                ts = None
            if ts is not None and ts <= now:
                templates.pop(name, None)
                continue
        active[name] = row
    if active != templates:
        save(state_root, data)
    return data


def save(state_root, data):
    path = state_path(state_root)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"
    write_atomic(path, payload.encode("utf-8"))


def record_limit(state_root, template, hit, *, preserve_existing=False):
    import fcntl
    Path(state_root).mkdir(parents=True, exist_ok=True)
    with (Path(state_root) / "provider-limits.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        data = load(state_root)
        templates = data.setdefault("templates", {})
        if preserve_existing and str(template) in templates:
            return
        if templates.get(str(template), {}).get("via") == "login" and hit.get("via") != "login":
            return  # A quota window must never unpause a dead login.
        templates[str(template)] = {
            "message": hit.get("message"),
            "resets_at": hit.get("resets_at"),
            "via": hit.get("via"),
            "recorded_at": _now(),
        }
        if hit.get("via") == "login":
            templates[str(template)].update(login_path=hit.get("login_path"),
                                            login_signature=hit.get("login_signature"))
        save(state_root, data)


def active_templates(state_root, now=None):
    data = load(state_root)
    out = []
    now = now or time.time()
    for name, row in sorted((data.get("templates") or {}).items()):
        if not isinstance(row, dict):
            continue
        resets = row.get("resets_at")
        if resets:
            try:
                ts = calendar.timegm(time.strptime(resets[:19], "%Y-%m-%dT%H:%M:%S"))
            except ValueError:
                ts = None
            if ts is not None and ts <= now:
                continue
        out.append({"template": name, "message": row.get("message"), "resets_at": resets,
                    "recorded_at": row.get("recorded_at"), "via": row.get("via")})
    return out


def status_limits(state_root):
    limits = active_templates(state_root)
    return {"provider_limits": limits} if limits else {}


def is_limited(template, limits_rows, now=None):
    """True when template appears in limits_rows with a future resets_at (or no resets_at)."""
    now = now or time.time()
    for row in limits_rows or []:
        if row.get("template") != template:
            continue
        resets = row.get("resets_at")
        if not resets:
            return True, row
        try:
            ts = calendar.timegm(time.strptime(resets[:19], "%Y-%m-%dT%H:%M:%S"))
        except ValueError:
            return True, row
        if ts > now:
            return True, row
    return False, None


def pick_fallback(model, profile, limits_rows):
    """Next executor template from profile fallback list that is not limited."""
    chain = []
    if profile and isinstance(profile.get("fallback"), list):
        chain = [str(x) for x in profile["fallback"] if isinstance(x, str)]
    if model in chain:
        idx = chain.index(model) + 1
        chain = chain[idx:]
    for candidate in chain:
        limited, _ = is_limited(candidate, limits_rows)
        if not limited:
            return candidate
    return None


def refuse_message(template, row, suggestion=None):
    if (row or {}).get("via") == "login":
        return (row or {}).get("message") or "Sign in again before running this template"
    resets = (row or {}).get("resets_at") or "an unknown time"
    msg = "executor template %s is usage-limited until %s" % (template, resets)
    if (row or {}).get("message"):
        msg += ": " + _clip(row["message"], 240)
    if suggestion:
        msg += "; try model %s instead" % suggestion
    return msg


# Login pauses and notices (item 23). Here, not in _login_refresh, because partial test
# estates copy this module and the executor calls these after every run.
LOGIN_SOURCES = {"pi": "~/.pi/agent/auth.json", "codex": "~/.codex/auth.json", "grok": "~/.grok/auth.json"}


def login_signature(home, cli):
    """The owner's login file identity; a change (signing in again) lifts a login pause."""
    source = LOGIN_SOURCES.get(cli) or ("~/.claude/.credentials.json" if cli == "claude" else None)
    if source is None:
        return None, None
    path = Path(home) / source[2:]
    try:
        st = path.stat()
    except OSError:
        return str(path), None
    return str(path), [st.st_ino, st.st_mtime_ns, st.st_size]


def notice(state, template, cli, home=None):
    """One actionable attention item per template per UTC day; persist a dispatch pause."""
    home = Path.home() if home is None else home
    label = {"pi": "Pi", "codex": "Codex", "grok": "Grok", "claude": "Claude"}.get(cli, cli)
    command = {"pi": "pi", "codex": "codex login", "grok": "grok login", "claude": "claude"}.get(cli, cli)
    reason = "%s's sign-in expired. Run `%s` and sign in again; lanes on %s are paused until a successful run confirms the login." % (label, command, label)
    login_path, signature = login_signature(home, cli)
    if signature is None:
        reason += " Dispatch will retry after %d minutes." % (LOGIN_RETRY_S // 60)
    record_limit(state, template, {"message": reason, "via": "login",
                                   "login_path": login_path, "login_signature": signature})
    stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    attention_id = "login:%s:%s" % (template, stamp[:10])
    path = Path(state) / "attention.jsonl"
    # Serialize read/dedup/append across concurrent lane completions.
    import fcntl
    path.parent.mkdir(parents=True, exist_ok=True)
    with (Path(state) / "login-attention.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if path.exists():
            for line in path.read_text().splitlines():
                try:
                    if json.loads(line).get("id") == attention_id:
                        return
                except (ValueError, AttributeError):
                    pass
        append_jsonl(path, {"event": "attention", "id": attention_id, "kind": "needs_you",
                     "question": reason, "template": template, "ts": stamp}, sort_keys=True, compact=False, sync=True, encoding="utf-8")


def clear_pause(state, template):
    import fcntl
    Path(state).mkdir(parents=True, exist_ok=True)
    with (Path(state) / "provider-limits.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        data = load(state)
        if data.get("templates", {}).get(template, {}).get("via") == "login":
            del data["templates"][template]
            save(state, data)
