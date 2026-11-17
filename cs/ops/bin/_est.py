#!/usr/bin/env python3
"""What a coding agent reports about a run's usage, and est_usd. Not an op.
Reports usage per agent, including cache creation.

est_basis: api_list = the agent's own API list price of the tokens (Claude total_cost_usd), an estimate and
never a charge; rate = tokens x the template's cost_per_1k (cache reads at CACHE_READ_FACTOR); chars = prompt
length, when the agent reported nothing. Tokens: input_tokens excludes cache reads where the agent reports them
apart; output_tokens includes reasoning. A field the agent did not report is None, never 0."""
import json
import re
import time
from pathlib import Path

TOKENS = ("input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens", "reasoning_tokens")
CACHE_READ_FACTOR = 0.1
IN_KEYS = ("input_tokens", "inputTokens", "prompt_tokens")
OUT_KEYS = ("output_tokens", "outputTokens", "completion_tokens")
READ_KEYS = ("cache_read_input_tokens", "cache_read_tokens", "cacheReadInputTokens", "cacheReadTokens")
CACHE_KEYS = ("cache_creation_input_tokens", "cache_creation_tokens", "cacheCreationInputTokens", "cacheWriteTokens")
METER_KEYS = ("total_cost_usd", "totalCostUsd", "total_cost")
CODEX_WINDOWS = {300: "five_hour", 10080: "seven_day"}
THREAD_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9-]{7,79}$")
QUOTA = re.compile(r"quota|usage limit|rate limit|resource.exhausted", re.I)


def iter_objs(text):
    dec = json.JSONDecoder()
    i, s = 0, text or ""
    while i < len(s):
        start = s.find("{", i)
        if start < 0:
            return
        try:
            obj, end = dec.raw_decode(s, start)
        except json.JSONDecodeError:
            i = start + 1
            continue
        if isinstance(obj, dict):
            yield obj
        i = end


def _count(v):
    return int(v) if isinstance(v, (int, float)) and not isinstance(v, bool) and v >= 0 else None


def _usd(v):
    return float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) and v >= 0 else None


def _pct(v):
    return round(float(v), 4) if isinstance(v, (int, float)) and not isinstance(v, bool) else None


def _first(src, keys):
    for k in keys:
        n = _count(src.get(k))
        if n is not None:
            return n
    return None


def _add(*values):
    known = [v for v in values if v is not None]
    return sum(known) if known else None


def _sub(a, b):
    return None if a is None else max(0, a - (b or 0))


def _iso(epoch):
    if not isinstance(epoch, (int, float)) or isinstance(epoch, bool) or epoch <= 0:
        return None
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(epoch))


def _dict(v):
    return v if isinstance(v, dict) else {}


def _tokens(inn=None, out=None, read=None, write=None, reasoning=None):
    return dict(zip(TOKENS, (inn, out, read, write, reasoning)))


def claude(objs):
    """--output-format json (one result) or stream-json (init, rate_limit_event, ..., result). cursor-agent's
    stream-json has the same shape; its result.usage is {inputTokens (cache excluded), outputTokens, cacheReadTokens,
    cacheWriteTokens} and its init names the model's display name."""
    results = [o for o in objs if o.get("type") == "result"]
    limits = [_dict(o.get("rate_limit_info")) for o in objs if o.get("type") == "rate_limit_event"]
    if not results and not limits:
        return None
    res = results[-1] if results else {}
    usage, models = _dict(res.get("usage")), _dict(res.get("modelUsage"))
    per = [_dict(m) for m in models.values()]
    tokens = None
    if per or usage:
        # modelUsage covers every model the run used (it sums to total_cost_usd); usage fills a field it lacks.
        pairs = (("inputTokens", IN_KEYS), ("outputTokens", OUT_KEYS), ("cacheReadInputTokens", READ_KEYS),
                 ("cacheCreationInputTokens", CACHE_KEYS))
        summed = [_add(*(_count(m.get(k)) for m in per)) for k, _ in pairs]
        tokens = _tokens(*(s if s is not None else _first(usage, keys) for s, (_, keys) in zip(summed, pairs)))
    if tokens is not None:
        tokens["reasoning_tokens"] = _count(_dict(usage.get("output_tokens_details")).get("thinking_tokens"))
    model = None
    if models:
        model = max(models, key=lambda k: (_usd(_dict(models[k]).get("costUSD")) or 0,
                                           _count(_dict(models[k]).get("outputTokens")) or 0))
    else:
        init = [o for o in objs if o.get("type") == "system" and isinstance(o.get("model"), str)]
        model = init[-1]["model"] if init else None
    metered = next((_usd(res.get(k)) for k in METER_KEYS if _usd(res.get(k)) is not None), None)
    if metered is None and per and all(_usd(m.get("costUSD")) is not None for m in per):
        metered = sum(_usd(m.get("costUSD")) for m in per)
    return {"tokens": tokens, "model": model, "metered": metered, "allowance": claude_allowance(limits)}


def _window(name, info):
    util = _pct(info.get("utilization"))
    return {"window": name, "used_pct": None if util is None else _pct(util * 100),
            "resets_at": _iso(info.get("resetsAt")),
            "status": info.get("status") if isinstance(info.get("status"), str) else None}


def claude_allowance(limits):
    """The last rate_limit_event per window. Public fields first; unifiedWindows (internal) only fills gaps."""
    windows = {}
    for info in limits:
        name = info.get("rateLimitType") if isinstance(info.get("rateLimitType"), str) else "unknown"
        windows[name] = _window(name, info)
        for other, w in _dict(info.get("unifiedWindows")).items():
            if other not in windows:
                windows[other] = _window(other, _dict(w))
    if not windows:
        return None
    return {"via": "rate_limit_event", "rejected": any(w["status"] == "rejected" for w in windows.values()),
            "windows": sorted(windows.values(), key=lambda w: w["window"])}


def codex(objs, sessions=None, since=None):
    """exec --json: thread.started{thread_id} ... turn.completed{usage}; the allowance is in the thread's rollout."""
    turns = [_dict(o.get("usage")) for o in objs if o.get("type") == "turn.completed"]
    thread = next((o.get("thread_id") for o in objs if o.get("type") == "thread.started"), None)
    if not turns and thread is None:
        return None
    tokens = None
    if turns:
        raw = {k: _add(*(_count(t.get(k)) for t in turns)) for k in
               ("input_tokens", "cached_input_tokens", "output_tokens", "cache_write_input_tokens",
                "reasoning_output_tokens")}
        tokens = _tokens(_sub(raw["input_tokens"], raw["cached_input_tokens"]), raw["output_tokens"],
                         raw["cached_input_tokens"], raw["cache_write_input_tokens"], raw["reasoning_output_tokens"])
    roll = codex_rollout(sessions, thread, since) if sessions else {}
    if tokens is None and roll.get("tokens"):
        tokens = roll["tokens"]
    failed = [o for o in objs if o.get("type") in ("turn.failed", "error")]
    allowance = roll.get("allowance")
    if allowance is None and any(QUOTA.search(json.dumps(o)) for o in failed):
        allowance = {"via": "error", "rejected": True, "windows": []}
    return {"tokens": tokens, "model": roll.get("model"), "metered": None, "allowance": allowance,
            "thread_id": thread if isinstance(thread, str) else None}


def codex_rollout(sessions, thread, since=None):
    """Only usage-shaped lines of ~/.codex/sessions/YYYY/MM/DD/rollout-*-<thread>.jsonl: token_count and
    turn_context.model. The rest of the file (the transcript) is never parsed."""
    if not isinstance(thread, str) or not THREAD_ID.match(thread):
        return {}
    root = Path(sessions)
    days = {time.strftime("%Y/%m/%d", time.localtime(t)) for t in (since or time.time(), time.time())}
    files = sorted(p for d in days for p in (root / d).glob("rollout-*-%s.jsonl" % thread) if p.is_file())
    if not files:
        return {}
    model = counts = limits = None
    try:
        with files[-1].open(encoding="utf-8", errors="replace") as stream:
            for line in stream:
                if '"token_count"' not in line and '"turn_context"' not in line:
                    continue
                try:
                    obj = json.loads(line)
                except ValueError:
                    continue
                payload = _dict(_dict(obj).get("payload"))
                if obj.get("type") == "turn_context" and isinstance(payload.get("model"), str):
                    model = payload["model"]
                elif payload.get("type") == "token_count":
                    counts = _dict(_dict(payload.get("info")).get("total_token_usage")) or counts
                    limits = _dict(payload.get("rate_limits")) or limits
    except OSError:
        return {}
    out = {"model": model}
    if counts:
        out["tokens"] = _tokens(_sub(_count(counts.get("input_tokens")), _count(counts.get("cached_input_tokens"))),
                                _count(counts.get("output_tokens")), _count(counts.get("cached_input_tokens")),
                                _count(counts.get("cache_write_input_tokens")),
                                _count(counts.get("reasoning_output_tokens")))
    if limits:
        windows = []
        for key in ("primary", "secondary"):
            w = _dict(limits.get(key))
            if w:
                minutes = _count(w.get("window_minutes"))
                windows.append({"window": CODEX_WINDOWS.get(minutes, "%s_minutes" % minutes if minutes else key),
                                "used_pct": _pct(w.get("used_percent")), "resets_at": _iso(w.get("resets_at")),
                                "status": None})
        reached = limits.get("rate_limit_reached_type")
        out["allowance"] = {"via": "rollout", "rejected": reached is not None, "windows": windows}
        credits = _dict(limits.get("credits"))
        if credits:
            out["allowance"]["credits"] = {k: credits.get(k) for k in ("has_credits", "unlimited", "balance")
                                           if k in credits}
    return out


def gemini(objs):
    """--output-format json: {session_id, response, stats{models{<model>{tokens{...}}}}, error?}."""
    docs = [o for o in objs if isinstance(_dict(o.get("stats")).get("models"), dict)
            or ("session_id" in o and "response" in o)]
    if not docs:
        return None
    doc = docs[-1]
    models = {k: _dict(_dict(v).get("tokens")) for k, v in _dict(_dict(doc.get("stats")).get("models")).items()}
    tokens = None
    if models:
        t = list(models.values())
        cached = _add(*(_count(m.get("cached")) for m in t))
        inn = _add(*((_count(m.get("input")) if _count(m.get("input")) is not None
                      else _sub(_count(m.get("prompt")), _count(m.get("cached")))) for m in t))
        thoughts = _add(*(_count(m.get("thoughts")) for m in t))
        out = _add(*(_count(m.get("candidates")) for m in t))
        tokens = _tokens(inn, None if out is None else out + (thoughts or 0), cached, None, thoughts)
    model = max(models, key=lambda k: (_count(models[k].get("total")) or 0)) if models else None
    err = _dict(doc.get("error"))
    allowance = None
    if err and (err.get("type") == "TerminalQuotaError" or err.get("code") == 429
                or QUOTA.search(str(err.get("message") or ""))):
        allowance = {"via": "error", "rejected": True, "windows": []}
    return {"tokens": tokens, "model": model, "metered": None, "allowance": allowance}


def gemini_stream(objs):
    """--output-format stream-json: init{model}, message..., result{status, error?, stats{total_tokens, input_tokens,
    output_tokens, cached, input, duration_ms, tool_calls}} (0.68.0 bundle; input = input_tokens less cached)."""
    results = [o for o in objs if o.get("type") == "result" and isinstance(o.get("stats"), dict)]
    if not results:
        return None
    res = results[-1]
    stats = res["stats"]
    cached = _count(stats.get("cached"))
    inn = _count(stats.get("input"))
    if inn is None:
        inn = _sub(_count(stats.get("input_tokens")), cached)
    tokens = _tokens(inn, _count(stats.get("output_tokens")), cached, None, None)
    init = [o for o in objs if o.get("type") == "init" and isinstance(o.get("model"), str)]
    errors = [_dict(res.get("error"))] + [_dict(o.get("error")) or o for o in objs if o.get("type") == "error"]
    allowance = None
    if any(e and (e.get("type") == "TerminalQuotaError" or e.get("code") == 429
                  or QUOTA.search(str(e.get("message") or ""))) for e in errors):
        allowance = {"via": "error", "rejected": True, "windows": []}
    return {"tokens": tokens if any(v is not None for v in tokens.values()) else None,
            "model": init[-1]["model"] if init else None, "metered": None, "allowance": allowance}


def antigravity(objs):
    """agy --output-format json|stream-json: {event: init|step_update|result}; result.result (stream) or the
    envelope itself (json) carries {status, response, usage{input_tokens, output_tokens, thinking_tokens,
    cache_read_tokens}}. input_tokens is taken as excluding cache reads (unconfirmed: no live capture yet)."""
    results = [_dict(o.get("result")) for o in objs if o.get("event") == "result"]
    results += [o for o in objs if "status" in o and "conversation_id" in o and "usage" in o]
    if not results:
        return None
    res = results[-1]
    usage = _dict(res.get("usage"))
    thinking = _count(usage.get("thinking_tokens"))
    out = _count(usage.get("output_tokens"))
    tokens = _tokens(_count(usage.get("input_tokens")), None if out is None else out + (thinking or 0),
                     _count(usage.get("cache_read_tokens")), None, thinking)
    init = [_dict(o.get("init")) for o in objs if o.get("event") == "init"]
    model = next((i["model"] for i in reversed(init) if isinstance(i.get("model"), str)), None)
    allowance = None
    if QUOTA.search(str(res.get("error") or "")) or "credits balance is too low" in str(res.get("error") or ""):
        allowance = {"via": "error", "rejected": True, "windows": []}
    return {"tokens": tokens if any(v is not None for v in tokens.values()) else None,
            "model": model, "metered": None, "allowance": allowance}


def pi_stream(objs):
    """pi --mode json: JSONL with message_end / agent_end assistant usage {input, output, cacheRead, ...}."""
    usage = None
    for obj in objs:
        if obj.get("type") == "message_update" and isinstance(obj.get("usage"), dict):
            usage = obj["usage"]
        message = obj.get("message") if isinstance(obj.get("message"), dict) else None
        if obj.get("type") == "message_end" and isinstance(message, dict) and isinstance(message.get("usage"), dict):
            usage = message["usage"]
        if obj.get("type") in ("turn_end", "agent_end") and isinstance(obj.get("message"), dict):
            msg = obj["message"]
            if isinstance(msg.get("usage"), dict):
                usage = msg["usage"]
        if obj.get("type") == "agent_end" and isinstance(obj.get("messages"), list):
            for msg in reversed(obj["messages"]):
                if isinstance(msg, dict) and msg.get("role") == "assistant" and isinstance(msg.get("usage"), dict):
                    usage = msg["usage"]
                    break
    if not isinstance(usage, dict):
        return None
    tokens = _tokens(
        _count(usage.get("input")),
        _count(usage.get("output")),
        _count(usage.get("cacheRead")),
        _count(usage.get("cacheWrite")),
        _count(usage.get("reasoning")),
    )
    if not any(v is not None for v in tokens.values()):
        return None
    model = None
    for obj in reversed(objs):
        if not isinstance(obj, dict):
            continue
        message = obj.get("message") if isinstance(obj.get("message"), dict) else None
        if message and isinstance(message.get("model"), str):
            model = message["model"]
            break
        if isinstance(obj.get("model"), str):
            model = obj["model"]
            break
    return {"tokens": tokens, "model": model, "metered": None, "allowance": None}


def goose(objs):
    """goose run --output-format json: {messages, metadata{status, input_tokens, output_tokens,
    cache_read_input_tokens, cache_write_input_tokens, cost_usd}}. cost_usd is omitted when unknown."""
    found = [o for o in objs if isinstance(o.get("metadata"), dict) and "status" in o["metadata"]]
    if not found:
        return None
    meta = found[-1]["metadata"]
    tokens = _tokens(_count(meta.get("input_tokens")), _count(meta.get("output_tokens")),
                     _count(meta.get("cache_read_input_tokens")), _count(meta.get("cache_write_input_tokens")))
    return {"tokens": tokens if any(v is not None for v in tokens.values()) else None,
            "model": None, "metered": _usd(meta.get("cost_usd")), "allowance": None}


def generic(objs):
    """Any other agent: the last object (or its usage) carrying token keys; a metered total if one is named."""
    tokens = metered = None
    for obj in objs:
        for k in METER_KEYS:
            if _usd(obj.get(k)) is not None:
                metered = _usd(obj.get(k))
                break
        usage = _dict(obj.get("usage")) or obj
        if any(k in usage for k in IN_KEYS + OUT_KEYS + CACHE_KEYS + READ_KEYS):
            tokens = _tokens(_first(usage, IN_KEYS), _first(usage, OUT_KEYS), _first(usage, READ_KEYS),
                             _first(usage, CACHE_KEYS))
    if tokens is None and metered is None:
        return None
    return {"tokens": tokens, "model": None, "metered": metered, "allowance": None}


def parse_usage(text, prompt, cost_per_1k, codex_sessions=None, since=None):
    """Row fields for one run's agent output: est_usd and est_basis always; the five token counts (None where
    not reported) when the agent reported usage; reported_model, allowance and codex_thread when known."""
    objs = list(iter_objs(text))
    found = (antigravity(objs) or gemini_stream(objs) or claude(objs) or codex(objs, codex_sessions, since)
             or gemini(objs) or goose(objs) or pi_stream(objs) or generic(objs) or {})
    tokens = found.get("tokens")
    rate = float(cost_per_1k or 0)
    row = {}
    if found.get("metered") is not None:
        row.update(est_usd=round(found["metered"], 6), est_basis="api_list")
    elif tokens and any(v is not None for v in tokens.values()):
        weighted = ((tokens["input_tokens"] or 0) + (tokens["output_tokens"] or 0)
                    + (tokens["cache_creation_tokens"] or 0) + CACHE_READ_FACTOR * (tokens["cache_read_tokens"] or 0))
        row.update(est_usd=round(weighted * rate / 1000.0, 6), est_basis="rate")
    else:
        row.update(est_usd=round(len(prompt or "") * rate / 1000.0, 6), est_basis="chars")
    if tokens:
        row.update(tokens)
    for key, field in (("model", "reported_model"), ("allowance", "allowance"), ("thread_id", "codex_thread")):
        if found.get(key) is not None:
            row[field] = found[key]
    return row
