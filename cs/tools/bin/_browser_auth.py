"""Narrow, local cookie handoffs for browser-backed corpus collectors."""
from pathlib import Path
from urllib.parse import urlsplit


RULES = {
    "tasks.google.com": ("chrome", ".google.com", "Default", "google-auth"),
    "x.com": ("chrome", ".x.com", "Default", "x-auth-chrome"),
    "www.linkedin.com": ("chrome", ".linkedin.com", "Default", "linkedin-auth-chrome"),
}


def state_partition_for(requested_url):
    """Keep cookie-bearing browser memory separate from public browsing."""
    requested = (urlsplit(requested_url).hostname or "").lower()
    rule = RULES.get(requested)
    return rule[3] if rule else "public"


def cookie_import_for(requested_url, current_url):
    requested = (urlsplit(requested_url).hostname or "").lower()
    current = (urlsplit(current_url).hostname or "").lower()
    rule = RULES.get(requested)
    if not rule:
        return None
    browser, cookie_domain, profile, _ = rule
    normalized = cookie_domain.lstrip(".")
    if current != normalized and not current.endswith("." + normalized):
        return None
    return browser, cookie_domain, profile


def isolated_state_dir(raw, workspace, code_root):
    """Resolve browser state outside model-writable and harvestable trees."""
    candidate = Path(raw).expanduser()
    if not candidate.is_absolute():
        raise ValueError("browser state must be an absolute operator path")
    candidate = candidate.resolve()
    workspace = Path(workspace).resolve()
    code_root = Path(code_root).resolve()
    if candidate == workspace or candidate.is_relative_to(workspace):
        raise ValueError("browser state cannot live inside the run workspace")
    if candidate == code_root or candidate.is_relative_to(code_root):
        raise ValueError("browser state cannot live inside the code_root or task setup")
    if "setup" in candidate.parts:
        raise ValueError("browser state cannot live inside a setup tree")
    return candidate
