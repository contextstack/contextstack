"""User-requested release updates. Channel indexes are documented in install.sh."""
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request

import cs_releases_manifest as manifest


def host_target():
    system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
    arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}.get(platform.machine())
    target = f"{system}-{arch}"
    if target not in ("macos-aarch64", "linux-aarch64", "linux-x86_64"):
        raise ValueError(f"unsupported platform: {platform.system()} {platform.machine()}")
    return target


def base_url(channel="stable"):
    path = "latest/download" if channel == "stable" else "download/channel-latest"
    default = f"https://github.com/contextstack/contextstack/releases/{path}"
    base = (os.environ.get("CS_INSTALL_BASE") or default).rstrip("/")
    if base != default:
        if os.environ.get("CS_INSTALL_DEV") != "1":
            raise ValueError("non-default CS_INSTALL_BASE requires CS_INSTALL_DEV=1")
        print(f"WARNING: CS_INSTALL_DEV=1: installing from non-default release source {base}", file=sys.stderr)
    url = urllib.parse.urlsplit(base)
    if url.scheme not in ("https", "http") or not url.hostname:
        raise ValueError("set CS_INSTALL_BASE to the HTTPS release host")
    if url.scheme == "http" and url.hostname not in ("localhost", "127.0.0.1", "::1"):
        raise ValueError("CS_INSTALL_BASE requires HTTPS except on localhost")
    return base


class SecureRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        base_url_check(newurl)
        if urllib.parse.urlsplit(req.full_url).scheme == "https" and urllib.parse.urlsplit(newurl).scheme != "https":
            raise ValueError("release redirects require HTTPS")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def base_url_check(url):
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" and not (parsed.scheme == "http" and parsed.hostname in ("localhost", "127.0.0.1", "::1")):
        raise ValueError("release downloads require HTTPS except on localhost")


def fetch(url):
    base_url_check(url)
    with urllib.request.build_opener(SecureRedirect()).open(url, timeout=60) as response:
        return response.read()


def install_archive(archive, directory):
    with tarfile.open(archive, "r:gz") as tar:
        members = tar.getmembers()
        for member in members:
            path = Path(member.name)
            if path.is_absolute() or ".." in path.parts or not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
                raise ValueError("unsafe bundle archive member")
            if member.issym() or member.islnk():
                link = Path(member.linkname)
                if link.is_absolute() or ".." in link.parts:
                    raise ValueError("unsafe bundle archive link")
        tar.extractall(directory, members=members, filter="data")


version_key = manifest.version_key
SEMVER = manifest.SEMVER


def build_time(meta):
    """UTC bundle creation time; legacy bundles can have no usable timestamp."""
    try:
        stamp = datetime.fromisoformat(meta.get("created", "").replace("Z", "+00:00"))
        return stamp.timestamp() if stamp.tzinfo is not None else None
    except (ValueError, TypeError, AttributeError):
        return None


def release_policy(version, installed, args, local=False, candidate_meta=None, current_meta=None):
    candidate, current = version_key(version), version_key(installed)
    if candidate and not candidate[3] and not (args.pre or args.channel in ("pre", "latest")):
        raise ValueError("pre-release refused on stable; use --pre or --channel pre")
    if installed == "none" or version == installed:
        return
    dated = r"[0-9]{8}-[A-Za-z0-9.-]+"  # pre-semver local release ids (e.g. 20261112-d5567b60c)
    if candidate is not None and current is not None:
        older = candidate < current
    elif candidate is not None and re.fullmatch(dated, installed):
        older = False  # every semver release is newer than any date-based local build (7th pass #60)
    elif current is not None and re.fullmatch(dated, version):
        older = True
    elif local and re.fullmatch(r"[0-9]{8}-[A-Za-z0-9.-]+", version) and re.fullmatch(r"[0-9]{8}-[A-Za-z0-9.-]+", installed):
        candidate_time, current_time = build_time(candidate_meta or {}), build_time(current_meta or {})
        # A hash identifies a build; it says nothing about when it was built.
        older = (candidate_time < current_time if candidate_time is not None and current_time is not None
                 else version[:8] < installed[:8])
    elif args.allow_downgrade:
        return
    else:
        raise ValueError("cannot compare release versions; use --allow-downgrade to explicitly install an incomparable release")
    if older and not args.allow_downgrade:
        raise ValueError("downgrade refused; use --allow-downgrade")


def load_manifest(install_base):
    url = manifest.manifest_url()
    manifest.manifest_source_ok(url, install_base)
    entries = manifest.parse_entries(fetch(url))
    return entries


def resolve_release(args, install_base, entries):
    channel = "latest" if args.channel in ("pre", "latest") else "stable"
    manifest_channel = "pre" if channel == "latest" else "stable"
    tag = manifest.latest_tag(entries, manifest_channel)
    if manifest_channel == "stable":
        manifest.strict_stable_tag(tag)
    base = manifest.release_assets_base(tag, install_base)
    return channel, tag, base


def run(args):
    root = Path.home() / ".contextstack"
    config = root / "config/install.json"
    configured = json.loads(config.read_text()).get("channel", "stable") if config.exists() else "stable"
    args.channel = args.channel or ("pre" if args.pre else configured)
    if args.channel not in ("stable", "pre", "latest"):
        raise ValueError("invalid configured release channel")
    installed = (root / "current").resolve().name if (root / "current").exists() else "none"
    if args.bundle:
        bundle = args.bundle.resolve()
        meta = json.loads((bundle / "bundle.json").read_text())
        if meta["target"] != host_target():
            raise ValueError("bundle target differs from host")
        current_meta_path = root / "current/bundle.json"
        current_meta = json.loads(current_meta_path.read_text()) if current_meta_path.is_file() else {}
        release_policy(meta["release"], installed, args, local=True, candidate_meta=meta, current_meta=current_meta)
        if not args.check:
            subprocess.run(["sh", str(bundle / "release/cs/scripts/node-install"), "install", str(bundle)] + (["--no-smoke"] if getattr(args, "no_smoke", False) else []), check=True)
        return 0
    install_base = os.environ.get("CS_INSTALL_BASE")
    if install_base:
        install_base = install_base.rstrip("/")
        if os.environ.get("CS_INSTALL_DEV") != "1":
            raise ValueError("non-default CS_INSTALL_BASE requires CS_INSTALL_DEV=1")
        print(f"WARNING: CS_INSTALL_DEV=1: installing from non-default release source {install_base}", file=sys.stderr)
    entries = load_manifest(install_base)
    channel, tag, base = resolve_release(args, install_base, entries)
    target = host_target()
    print(f"Will read {base}/{channel}.json for {target}.", flush=True)
    index = json.loads(fetch(f"{base}/{channel}.json"))
    version = index["version"]
    commit = index["commit"]
    if not isinstance(version, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", version):
        raise ValueError("invalid release version")
    if version_key(version) is None:
        raise ValueError("release channel requires a semver version")
    if version != tag:
        raise ValueError("release version differs from RELEASES.json on main")
    listed = manifest.lookup(entries, tag, commit)
    if channel == "stable":
        manifest.strict_stable_tag(version)
    if args.channel == "stable" and listed["channel"] != "stable":
        raise ValueError("pre-release refused on stable; use --pre or --channel pre")
    entry = index["platforms"][target]
    name, checksum = entry["tarball"], entry["sha256"]
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*\.tar\.gz", name) or not re.fullmatch(r"[a-f0-9]{64}", checksum):
        raise ValueError("invalid tarball name or sha256")
    print(f"Installed: {installed}; available ({args.channel}): {version}; commit: {commit}", flush=True)
    if args.check:
        return 0
    release_policy(version, installed, args)
    print(f"Will download {base}/{name} and .sha256, verify both checksums, unpack, and run node-install install.\n"
          "Running jobs drain before activation; previous releases stay available for node-install rollback.\n"
          "No sudo. No telemetry.", flush=True)
    with tempfile.TemporaryDirectory(prefix="cs-update-") as tmp:
        directory = Path(tmp)
        archive = directory / "bundle.tar.gz"
        sums = fetch(f"{base}/{name}.sha256").decode().split()
        if sums != [checksum, name]:
            raise ValueError("sha256 sidecar differs from channel manifest")
        archive.write_bytes(fetch(f"{base}/{name}"))
        if hashlib.sha256(archive.read_bytes()).hexdigest() != checksum:
            raise ValueError("bundle sha256 mismatch; current release unchanged")
        bundle = directory / "bundle"
        bundle.mkdir()
        install_archive(archive, bundle)
        meta = json.loads((bundle / "bundle.json").read_text())
        if meta["release"] != version or meta["target"] != target or meta["lab_commit"] != commit:
            raise ValueError("bundle metadata differs from channel manifest")
        manifest.lookup(entries, version, meta["lab_commit"])
        subprocess.run(["sh", str(bundle / "release/cs/scripts/node-install"), "install", str(bundle)] + (["--no-smoke"] if getattr(args, "no_smoke", False) else []), check=True)
    return 0


def main(args):
    try:
        return run(args)
    except (ValueError, KeyError, OSError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"cs update: {error}", file=__import__("sys").stderr)
        return 1
