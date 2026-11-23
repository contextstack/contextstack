"""Sub-agent types for cs mcp: named worker profiles as versioned files (cs/agents/<name>.md). Stdlib only.

A profile is markdown with a small frontmatter between `---` lines, one `key: value` per line:

    name         the file's stem ([a-z0-9._-])
    description  one line
    model        an executor template (ops/bin/executor-templates.json): cursor-sol, cursor-codex, claude-code-opus, ...
    writes       true: may change files; false: read-only (reviewer, researcher)
    lane         true: needs repo+lane (a lane worktree); false: a scratch workspace, repo+lane refused
    check        the default check (a shell body; "" = none)
    budget       {turns: N, tool_calls: N, usd: N, seconds: N}; seconds at most 3600 (the gateway's work-order limit)
    task_type    the default task_type meta tag
    fallback     optional JSON list of executor templates to try when the default model is usage-limited (e.g. ["codex", "cursor-composer"])

The body is the worker's system prompt. Values are true/false, a whole number, a "JSON string", a {k: number} map or
a bare string. Profiles come from the live release's cs/agents (~/.contextstack/current at call time), or
$CS_AGENTS_DIR.
"""
import hashlib
import json
import math
import os
import re
from pathlib import Path

NAME = re.compile(r"[a-z0-9][a-z0-9._-]{0,62}")
MAX_SECONDS = 3600  # the gateway refuses a work order over an hour, and the job then errors with no reason shown
BUDGET_KEYS = ("turns", "tool_calls", "usd", "seconds")
FIELDS = {"name": str, "description": str, "model": str, "writes": bool, "lane": bool, "check": str, "budget": dict,
          "task_type": str, "fallback": list}
REQUIRED = ("name", "description", "model", "writes", "lane")
_DEV_KIT = Path(__file__).resolve().parents[1]


def install_root():
    return Path(os.environ.get("CS_INSTALL_ROOT") or Path.home() / ".contextstack")


def kit_dir():
    """The release's cs tree: follow `current` on every call so a long-running cs mcp tracks release switches."""
    root = install_root() / "current"
    try:
        resolved = root.resolve()
    except OSError:
        resolved = None
    if resolved is not None:
        for name in ("cs",):
            kit = resolved / name
            if kit.is_dir():
                return kit
    return _DEV_KIT


class Refused(Exception):
    pass


def agents_dir():
    return Path(os.environ.get("CS_AGENTS_DIR") or kit_dir() / "agents")


def harness_model(model):
    return isinstance(model, str) and re.fullmatch(r"harness:[A-Za-z0-9][A-Za-z0-9._:/-]{0,99}", model)


def known_models():
    try:
        return set(json.loads((kit_dir() / "ops/bin/executor-templates.json").read_text()))
    except (OSError, ValueError):
        return None  # no templates file beside this code root: the node is the judge


def scalar(text, where):
    text = text.strip()
    if text in ("true", "false"):
        return text == "true"
    if re.fullmatch(r"-?\d+", text):
        return int(text)
    if re.fullmatch(r"-?\d+\.\d*", text):
        return float(text)
    if text.startswith('"'):
        try:
            return json.loads(text)
        except ValueError:
            raise Refused("%s: a bad quoted string %s" % (where, text))
    if text.startswith("{"):
        if not text.endswith("}"):
            raise Refused("%s: a map must end with } on the same line" % where)
        out = {}
        for part in filter(None, (p.strip() for p in text[1:-1].split(","))):
            k, sep, v = part.partition(":")
            v = scalar(v, where) if sep else None
            if not sep or not k.strip() or isinstance(v, (bool, dict, str)) or v is None:
                raise Refused("%s: %r is not key: number" % (where, part))
            out[k.strip()] = v
        return out
    if text.startswith("["):
        try:
            value = json.loads(text)
        except ValueError:
            raise Refused("%s: a bad list %s" % (where, text))
        if not isinstance(value, list):
            raise Refused("%s: fallback must be a JSON list of template names" % where)
        return value
    return text


def parse(text, stem):
    """A profile's text -> its fields plus `system` (the body). Refused names the line that is wrong."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        raise Refused("line 1: a profile starts with a --- line, then its frontmatter, then ---")
    try:
        end = next(i for i in range(1, len(lines)) if lines[i].strip() == "---")
    except StopIteration:
        raise Refused("the frontmatter has no closing --- line")
    fields = {}
    for n in range(1, end):
        line = lines[n]
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        where = "line %d" % (n + 1)
        key, sep, value = line.partition(":")
        key = key.strip()
        if not sep or key not in FIELDS:
            raise Refused("%s: %r is not one of %s" % (where, key if sep else line.strip(), ", ".join(FIELDS)))
        if key in fields:
            raise Refused("%s: %s given twice" % (where, key))
        value = scalar(value, where)
        if FIELDS[key] is str and not isinstance(value, str):
            value = json.dumps(value) if key == "check" else str(value)  # check: 0 is an odd but valid body
        if not isinstance(value, FIELDS[key]):
            raise Refused("%s: %s must be %s" % (where, key,
                {bool: "true or false", dict: "a {k: number} map", list: "a JSON list"}[FIELDS[key]]))
        fields[key] = value
    missing = [k for k in REQUIRED if k not in fields]
    if missing:
        raise Refused("missing %s" % ", ".join(missing))
    if fields["name"] != stem or not NAME.fullmatch(stem):
        raise Refused("name %r must be the file's stem %r ([a-z0-9._-])" % (fields["name"], stem))
    budget = fields.get("budget") or {}
    bad = [k for k in budget if k not in BUDGET_KEYS]
    if bad:
        raise Refused("budget: %s not one of %s" % (", ".join(bad), ", ".join(BUDGET_KEYS)))
    check_budget(budget, "budget")
    models = known_models()
    if models is not None and fields["model"] not in models and not harness_model(fields["model"]):
        raise Refused("model %r is not an executor template (%s)" % (fields["model"], ", ".join(sorted(models))))
    chain = fields.get("fallback") or []
    if chain:
        bad = [m for m in chain if not isinstance(m, str) or (models is not None and m not in models and not harness_model(m))]
        if bad:
            raise Refused("fallback lists invalid models: %s" % ", ".join(str(x) for x in bad))
    fields["system"] = "\n".join(lines[end + 1:]).strip() + "\n"
    if not fields["system"].strip():
        raise Refused("the body (the worker's system prompt) is empty")
    return fields


def check_budget(budget, what="budget"):
    for k, v in (budget or {}).items():
        if isinstance(v, bool) or not isinstance(v, (int, float)):
            raise Refused("%s.%s must be a number >= 0, not %r" % (what, k, v))
        if not math.isfinite(float(v)) or v < 0:
            raise Refused("%s.%s must be a finite number >= 0, not %r" % (what, k, v))
    if float((budget or {}).get("seconds") or 0) > MAX_SECONDS:
        raise Refused("%s.seconds %s is over %d: the gateway refuses a work order longer than an hour"
                      % (what, budget["seconds"], MAX_SECONDS))


def names(where=None):
    where = where or agents_dir()
    try:
        return sorted(p.stem for p in where.iterdir() if p.suffix == ".md" and p.is_file())
    except OSError:
        return []


def load(name):
    """The profile `name`, with `path` and `sha256` (of the file's bytes). Refused: unknown or malformed."""
    where = agents_dir()
    known = names(where)
    if not isinstance(name, str) or name not in known:
        raise Refused("unknown agent %r: the agents in %s are %s" % (name, where, ", ".join(known) or "none"))
    path = where / (name + ".md")
    data = path.read_bytes()
    try:
        profile = parse(data.decode("utf-8"), name)
    except (Refused, UnicodeDecodeError) as e:
        raise Refused("agent profile %s is malformed: %s" % (path, e))
    return dict(profile, path=str(path), sha256=hashlib.sha256(data).hexdigest())


def listing():
    out = []
    for name in names():
        try:
            p = load(name)
        except Refused as e:
            out.append({"name": name, "error": str(e)})
            continue
        out.append({k: p.get(k) for k in ("name", "description", "model", "writes", "lane", "task_type")})
    return {"dir": str(agents_dir()), "agents": out}


def as_check(body):
    return body if not body or body.startswith("#!") else "#!/bin/sh\n" + body + ("" if body.endswith("\n") else "\n")


def read_only_guard(name, check):
    """A read-only agent's check in a lane worktree: fail if it left the worktree changed, then run `check`."""
    inner = check if check and check.strip() else "#!/bin/sh\nexit 0\n"
    guard = ("#!/bin/sh\n# agent %s is read-only (writes: false): the lane worktree must be unchanged\n"
             "if git rev-parse --is-inside-work-tree >/dev/null 2>&1 && [ -n \"$(git status --porcelain)\" ]; then\n"
             "  echo \"read-only agent %s changed the worktree:\"; git status --porcelain | head -20; exit 1\nfi\n" % (name, name))
    sentinel = "CS_AGENT_INNER_CHECK"
    while sentinel in inner:
        sentinel += "_"
    return guard + ('inner=$(mktemp)\ncat > "$inner" <<\'%s\'\n%s%s\n%s\nchmod +x "$inner"\n"$inner"\nstatus=$?\n'
                    'rm -f "$inner"\nexit $status\n' % (sentinel, as_check(inner), "" if inner.endswith("\n") else "\n", sentinel))


def _limits_rows(node_status=None, state_root=None):
    rows = []
    if isinstance(node_status, dict):
        for row in node_status.get("provider_limits") or []:
            if isinstance(row, dict) and row.get("template"):
                rows.append(row)
    if state_root:
        try:
            import sys
            from pathlib import Path as _Path
            ops = kit_dir() / "ops" / "bin"
            if str(ops) not in sys.path:
                sys.path.insert(0, str(ops))
            from _provider_limits import active_templates
            rows.extend(active_templates(state_root))
        except (ImportError, OSError, ValueError):
            pass
    seen, out = set(), []
    for row in rows:
        key = row.get("template")
        if key in seen:
            continue
        seen.add(key)
        out.append(row)
    return out


def _model_chain(model, profile):
    chain = []
    if profile and isinstance(profile.get("fallback"), list):
        chain = [str(x) for x in profile["fallback"] if isinstance(x, str)]
    if model in chain:
        return [model] + chain[chain.index(model) + 1 :]
    out = [model]
    for candidate in chain:
        if candidate not in out:
            out.append(candidate)
    return out


def choose_model(model, profile, limits_rows, *, allow_fallback=True):
    """Refuse, or pick the profile fallback chain, when model is usage-limited."""
    try:
        import sys
        ops = kit_dir() / "ops" / "bin"
        if str(ops) not in sys.path:
            sys.path.insert(0, str(ops))
        from _provider_limits import is_limited, pick_fallback, refuse_message
    except ImportError as error:
        return model, [], str(error)
    limited, row = is_limited(model, limits_rows)
    if not limited:
        return model, [], None
    if not allow_fallback:
        until = (row or {}).get("resets_at")
        return None, [{"model": model, "until": until}], refuse_message(model, row, None)
    skipped = []
    for candidate in _model_chain(model, profile):
        lim, lim_row = is_limited(candidate, limits_rows)
        if not lim:
            return candidate, skipped, None
        skipped.append({"model": candidate, "until": (lim_row or {}).get("resets_at")})
    suggestion = pick_fallback(model, profile, limits_rows) if profile else None
    return None, skipped, refuse_message(model, row, suggestion)


def retry_limit_refuse(model, profile, limits_rows):
    """Plain refusal when retry_task cannot keep model; names how to opt into profile fallback."""
    try:
        import sys
        ops = kit_dir() / "ops" / "bin"
        if str(ops) not in sys.path:
            sys.path.insert(0, str(ops))
        from _provider_limits import is_limited, pick_fallback, refuse_message
    except ImportError as error:
        return str(error)
    limited, row = is_limited(model, limits_rows)
    if not limited:
        return "executor template %s is unavailable" % model
    msg = refuse_message(model, row, None)
    if profile:
        nxt = pick_fallback(model, profile, limits_rows)
        if nxt:
            msg += "; pass allow_fallback=true on retry_task to use %s" % nxt
        else:
            msg += "; pass allow_fallback=true on retry_task to walk the profile fallback chain"
    else:
        msg += "; pass allow_fallback=true on retry_task when the job has an agent profile with fallback templates"
    return msg


def pick_retry_model(prior_model, profile, limits_rows, explicit=None, *, allow_fallback=False, edit_refused=False):
    """Keep the prior template by default and automatically switch after an edit refusal stop."""
    if explicit:
        chosen, skipped, err = choose_model(explicit, profile, limits_rows, allow_fallback=False)
        if err:
            raise Refused(retry_limit_refuse(explicit, profile, limits_rows) if "usage-limited" in (err or "") else err)
        return chosen, skipped or (prior_model if chosen != prior_model else None)
    try:
        import sys
        ops = kit_dir() / "ops" / "bin"
        if str(ops) not in sys.path:
            sys.path.insert(0, str(ops))
        from _provider_limits import is_limited, pick_fallback
    except ImportError as error:
        raise Refused(str(error))
    limited, _ = is_limited(prior_model, limits_rows)
    if not limited and not edit_refused:
        return prior_model, None
    if not allow_fallback and not edit_refused:
        raise Refused(retry_limit_refuse(prior_model, profile, limits_rows))
    if not profile:
        raise Refused("no agent profile on the job: pass model= explicitly")
    chosen = pick_fallback(prior_model, profile, limits_rows)
    if not chosen or chosen == prior_model:
        raise Refused("no available fallback model after %s: pass model= explicitly" % prior_model)
    return chosen, prior_model


def resolve(agent, args, default_model):
    """start_task's arguments with the profile's defaults filled in (explicit ones win); refuses before any upload.
    Returns (args, profile or None, meta extras)."""
    budget = args.get("budget")
    if budget is not None and not isinstance(budget, dict):
        raise Refused("budget must be an object {turns, tool_calls, usd, seconds}")
    if not agent:
        check_budget(budget)
        return dict(args, model=args.get("model") or default_model), None, {}
    p = load(agent)
    out = dict(args)
    explicit = args.get("model")
    out["model"] = explicit or p["model"]
    out["system"] = args.get("system") or p["system"]
    out["task_type"] = args.get("task_type") or p.get("task_type")
    out["budget"] = dict(p.get("budget") or {}, **(budget or {}))
    check = args.get("check")
    if check is None:
        check = as_check(p.get("check") or "")
    has_lane = bool(args.get("repo"))
    if p["lane"] and not has_lane:
        raise Refused("agent %s works in a lane worktree: give repo and lane" % agent)
    if not p["lane"] and has_lane:
        raise Refused("agent %s works in a scratch workspace (lane: false): leave out repo and lane" % agent)
    if not p["writes"] and has_lane:
        check = read_only_guard(agent, check)
    out["check"] = check
    check_budget(out["budget"])
    return out, p, {"agent": agent, "agent_sha256": p["sha256"]}
