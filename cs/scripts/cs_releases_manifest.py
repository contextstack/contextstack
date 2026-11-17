"""RELEASES.json on main is the install trust anchor; clients never read it from a release."""
import json
import os
import re
import urllib.parse

MANIFEST_PATH = "RELEASES.json"
SEMVER = re.compile(
    r"v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
COMMIT = re.compile(r"^[a-fA-F0-9]+$")
STABLE_TAG = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
CHANNELS = {"stable", "pre"}


def version_key(version):
    match = SEMVER.fullmatch(version)
    if not match:
        return None
    major, minor, patch, pre = match.groups()
    identifiers = pre.split(".") if pre else []
    if any(x.isdigit() and len(x) > 1 and x.startswith("0") for x in identifiers):
        raise ValueError("invalid semver pre-release")
    return (int(major), int(minor), int(patch), pre is None,
            tuple((0, int(x)) if x.isdigit() else (1, x) for x in identifiers))


MANIFEST_DEFAULT = f"https://raw.githubusercontent.com/contextstack/contextstack/main/{MANIFEST_PATH}"
GITHUB_RELEASES = "https://github.com/contextstack/contextstack/releases"


def manifest_url():
    return (os.environ.get("CS_RELEASES_MANIFEST_URL") or MANIFEST_DEFAULT).strip()


def manifest_source_ok(url, install_base=None):
    parsed = urllib.parse.urlsplit(url)
    if not parsed.scheme or not parsed.hostname:
        raise ValueError("invalid release manifest URL")
    local = parsed.hostname in ("localhost", "127.0.0.1", "::1") and parsed.scheme == "http"
    if local:
        if parsed.path.rstrip("/") != "/" + MANIFEST_PATH:
            raise ValueError("release manifest must be RELEASES.json on main")
        return
    if parsed.scheme != "https" or parsed.hostname != "raw.githubusercontent.com":
        raise ValueError("release manifest must be fetched from raw.githubusercontent.com main")
    parts = [segment for segment in parsed.path.strip("/").split("/") if segment]
    if len(parts) < 4 or parts[2] != "main" or parts[-1] != MANIFEST_PATH:
        raise ValueError("release manifest must be the main branch RELEASES.json")
    if install_base:
        base = urllib.parse.urlsplit(install_base)
        if base.hostname == parsed.hostname and "/releases/" in parsed.path:
            raise ValueError("release manifest must not be fetched from the release itself")


def parse_entries(raw):
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as error:
        raise ValueError("invalid release manifest") from error
    releases = data.get("releases")
    if not isinstance(releases, list):
        raise ValueError("invalid release manifest")
    entries = []
    seen = set()
    for item in releases:
        if not isinstance(item, dict):
            raise ValueError("invalid release manifest")
        tag, commit, channel = item.get("tag"), item.get("commit"), item.get("channel")
        if not isinstance(tag, str) or not isinstance(commit, str) or not isinstance(channel, str):
            raise ValueError("invalid release manifest")
        if channel not in CHANNELS or tag in seen or not COMMIT.fullmatch(commit):
            raise ValueError("invalid release manifest")
        if version_key(tag) is None:
            raise ValueError("invalid release manifest")
        if channel == "stable" and not STABLE_TAG.fullmatch(tag):
            raise ValueError("invalid release manifest")
        seen.add(tag)
        entries.append({"tag": tag, "commit": commit, "channel": channel})
    return entries


def latest_tag(entries, channel):
    pool = [entry for entry in entries if entry["channel"] == channel]
    if not pool:
        raise ValueError(f"no {channel} release in manifest")
    if channel == "stable":
        pool = [entry for entry in pool if version_key(entry["tag"])[3]]
    return max(pool, key=lambda entry: version_key(entry["tag"]))["tag"]


def lookup(entries, tag, commit):
    for entry in entries:
        if entry["tag"] == tag and entry["commit"] == commit:
            return entry
    raise ValueError("release commit is not listed in RELEASES.json on main")


def release_assets_base(tag, install_base=None):
    if install_base:
        return install_base.rstrip("/")
    return f"{GITHUB_RELEASES}/download/{tag}"


def strict_stable_tag(tag):
    if not STABLE_TAG.fullmatch(tag):
        raise ValueError("stable release requires strict semver tag")
