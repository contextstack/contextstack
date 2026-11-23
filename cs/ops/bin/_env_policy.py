"""Node environment policy (Plan.CS.v2.1 §4, decision 20). Not an op.

`policy/env.json` {"version": 1, "always": [...], "on_request": [...]} decides
what a child of the runner inherits. A spec may REQUEST names (`env: [NAME]`);
the launcher (tick, run-spec) resolves the request against this file and hands
the runner the effective list in CS_CHILD_ENV; the runner never reads the
policy. Effective = always ∪ (requested ∩ on_request); a requested name in
neither tier is refused by name. File absent: DEFAULT_ENV, the list the runner
compiled in before 2026-11-08 and still carries as its own fallback (run.go
`defaultChildEnv`; tasks/regressions/env-policy keeps the two equal). The
launcher hands the runner child_env_value(resolve(code_root, spec_text)).
"""
import json
from pathlib import Path

CODE_ROOT = Path(__file__).resolve().parent.parent.parent
POLICY = "policy/env.json"
DEFAULT_ENV = ("PATH HOME USER LOGNAME TERM LANG LC_ALL LC_CTYPE TMPDIR TMP TEMP SHELL HOMEBREW_PREFIX "
           "HOMEBREW_CELLAR CS_JOB_ID CS_TRIAL CS_SCHEDULE_DATE CS_SCHEDULE_SLOT "
           "CS_SCHEDULE_TIMEZONE CS_VIDEO_MUTATION CS_STEP_TTY CS_CODE_ROOT CS_CONFIG_ROOT CS_STATE_ROOT "
           "CS_WORKSPACES_ROOT CS_TOOL_PREFIX CS_SANDBOX_BIN CS_BROWSER_STATE "
           "CS_TOOL_TIMEOUT_SECONDS").split()


class EnvPolicyError(ValueError):
    """The policy file is malformed, or a spec requested a name it does not permit."""


def load_policy(code_root=CODE_ROOT):
    """{"always": [...], "on_request": [...]}; absent file = DEFAULT_ENV, malformed = EnvPolicyError."""
    try:
        raw = (Path(code_root) / POLICY).read_text(encoding="utf-8")
    except FileNotFoundError:
        return {"always": list(DEFAULT_ENV), "on_request": []}
    try:
        pol = json.loads(raw)
        tiers = {tier: pol.get(tier) or [] for tier in ("always", "on_request")}
        if pol.get("version") != 1 or not all(isinstance(t, list) and all(isinstance(n, str) for n in t)
                                              for t in tiers.values()):
            raise ValueError("want version 1 with string lists always, on_request")
    except (ValueError, AttributeError) as error:
        raise EnvPolicyError("%s: %s" % (POLICY, error))
    return tiers


def _names(raw):
    """One `[A, "B"]` flow list, quoted or bare (tick's spec_tasks shape); `[]` is empty; a scalar
    is refused, as the Go loader refuses it (spec.go `Env []string`), never read as a one-item request."""
    if raw in ("null", "~"):
        return []
    if not (raw.startswith("[") and raw.endswith("]")):
        raise EnvPolicyError("env: must be a list, got %s" % raw)
    try:
        names = json.loads(raw)
    except json.JSONDecodeError:
        names = [part.strip().strip("\"'") for part in raw.strip("[]").split(",") if part.strip()]
    if not isinstance(names, list):
        raise EnvPolicyError("env: must be a list, got %s" % raw)
    return [str(n) for n in names]


def requested(spec_text):
    """The spec's `env:` request as the Go loader's YAML list reads it: flow form (`env: [A, B]`) or
    block form (`env:` then `- A` lines, to the next top-level key), with trailing ` #` comments dropped."""
    lines = spec_text.splitlines()
    for i, line in enumerate(lines):
        if not line.startswith("env:"):
            continue
        raw = line.split(":", 1)[1].split(" #", 1)[0].strip()
        if raw:
            return _names(raw)
        names = []
        for item in lines[i + 1:]:
            item = item.split(" #", 1)[0].strip()
            if item.startswith("#") or not item:
                continue
            if not item.startswith("-"):
                break
            names.append(item[1:].strip().strip("\"'"))
        return [n for n in names if n]
    return []


def resolve(code_root, spec_text):
    """The effective child-environment names for a spec; EnvPolicyError names the first refused one."""
    pol, names = load_policy(code_root), []
    names.extend(pol["always"])
    for name in requested(spec_text):
        if name in pol["on_request"]:
            if name not in names:
                names.append(name)
        elif name not in names:
            raise EnvPolicyError('env "%s": not allowed by %s' % (name, POLICY))
    return names


def child_env_value(names):
    """CS_CHILD_ENV as the runner reads it: space-separated names."""
    return " ".join(names)
