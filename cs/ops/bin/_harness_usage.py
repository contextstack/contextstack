"""Sign-in harnesses (pi, goose, grok): private run state, staged auth, unmetered cost.

Provider quota/limit signals on harness stdout/stderr are classified in _provider_limits (same as native CLIs).

A harness already holds the user's provider login. The template names headless args, a private
home (`_cli_native`), read-only links to the login files, and `deny_write` paths the sandbox
must not modify. Auth file contents are never read here.

`unmetered` means the CLI can spend (an API key behind the login) and does not promise a
dollar figure before it runs. A work order with budget.usd > 0 is refused first. A reported
dollar amount is kept; a missing one is not stored as 0.
"""
import json
from pathlib import Path


REFUSAL = "unmetered harness reports no spend; refuse a USD budget rather than count $0"
KINDS = ("pi_json", "grok_json", "goose_json")
OAUTH_SUBSCRIPTION_PREFIXES = ("openai-codex/", "anthropic/")


def model_from_cli_args(args):
    for index, part in enumerate(args or []):
        if part == "--model" and index + 1 < len(args):
            return str(args[index + 1]).strip()
    return None


def harness_subscription_model(model):
    if not model:
        return False
    lowered = str(model).strip().lower()
    return any(lowered.startswith(prefix) for prefix in OAUTH_SUBSCRIPTION_PREFIXES)


def resolve_harness_cost(tmpl, cli_args=None):
    """When a sign-in harness runs an OAuth subscription model, treat it as subscription (wallet $0)."""
    args = cli_args if cli_args is not None else tmpl.get("args")
    base = str(tmpl.get("cost_class") or "").strip()
    unmetered = tmpl.get("unmetered") is True
    if tmpl.get("terminal") not in KINDS:
        return base, unmetered
    model = model_from_cli_args(args)
    if harness_subscription_model(model):
        return "subscription", False
    return base, unmetered


def usd_budget_refusal(tmpl, limits):
    """The refusal string when an unmetered harness is given a positive USD budget, else None."""
    if tmpl.get("unmetered") is not True or not isinstance(limits, dict):
        return None
    try:
        usd = float(limits.get("usd") or 0)
    except (TypeError, ValueError):
        return None
    return REFUSAL if usd > 0 else None


def settle_cost(row, cost_usd, unmetered, cost_class):
    """Wallet cost after the CLI's own report. Subscription stays 0. An unmetered api harness
    takes a reported dollar amount and leaves a missing one as None."""
    if not unmetered:
        return cost_usd
    row["unmetered"] = True
    if row.get("est_basis") == "api_list" and row.get("est_usd") is not None and cost_class == "api":
        row["cost_usd"] = row["est_usd"]
        return row["est_usd"]
    if row.get("est_basis") != "api_list":
        row["est_usd"] = None
        row["est_basis"] = "unmetered"
    row["cost_usd"] = cost_usd
    return cost_usd


def _delegate(name, stdout):
    if not (stdout or "").strip():
        return "native_terminal_missing"
    try:
        from _cli_terminal import terminal
        terminal(name, stdout)
    except (ValueError, TypeError, KeyError, json.JSONDecodeError):
        return "native_terminal_error"
    return None


def _goose(stdout):
    """goose run --output-format json: one object, metadata.status completed."""
    text = (stdout or "").strip()
    if not text:
        return "native_terminal_missing"
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return "native_terminal_error"
    meta = data.get("metadata") if isinstance(data, dict) else None
    if not isinstance(meta, dict) or "status" not in meta:
        return "native_terminal_missing"
    if meta.get("status") != "completed":
        return "native_terminal_error"
    token_fields = ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_write_input_tokens")
    if not any(type(meta.get(key)) in (int, float) and meta[key] > 0 for key in token_fields):
        return "no_model_call"
    return None


def terminal_stop(kind, stdout):
    """None when the harness printed its own successful completion; else the stop reason."""
    if kind == "goose_json":
        return _goose(stdout)
    if kind == "pi_json":
        return _delegate("pi", stdout)
    if kind == "grok_json":
        return _delegate("grok", stdout)
    raise ValueError("unknown terminal kind %s" % kind)
