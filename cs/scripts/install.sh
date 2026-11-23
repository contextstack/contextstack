#!/bin/sh
# Public curl | sh entry point. Python is supplied by the verified bundle.
# Cs-init setup hosts: keep refusal text and INIT_SETUP_TARGETS in sync with scripts/_install_setup.py.
main() {
set -eu
fail() { echo "install: $*" >&2; exit 1; }
CHANNEL=stable
LINK_DIR=
YES=
FORCE_PLATFORM=
DOC_URL=https://github.com/contextstack/contextstack#documentation
while [ "$#" -gt 0 ]; do
  case "$1" in
    --channel) [ "$#" -ge 2 ] || fail '--channel needs stable or latest'; CHANNEL=$2; shift 2 ;;
    --link-dir) [ "$#" -ge 2 ] || fail '--link-dir needs a directory'; LINK_DIR=$2; shift 2 ;;
    --yes) YES=--yes; shift ;;
    --force-platform) FORCE_PLATFORM=1; echo 'WARNING: --force-platform skips setup support checks (unsupported; developer test hosts only)' >&2; shift ;;
    *) fail 'usage: sh install.sh [--channel stable|latest] [--link-dir DIR] [--yes] [--force-platform]' ;;
  esac
done
case "$CHANNEL" in stable|latest) ;; *) fail 'channel must be stable or latest';; esac
command -v curl >/dev/null 2>&1 || fail 'curl is required (install curl, then rerun this script)'
if command -v shasum >/dev/null 2>&1; then
  digest() { shasum -a 256 "$1"; }
elif command -v sha256sum >/dev/null 2>&1; then
  digest() { sha256sum "$1"; }
else
  fail 'shasum or sha256sum is required'
fi
install_root=$HOME/.contextstack
if [ -e "$install_root" ]; then
  [ -w "$install_root" ] || fail "install root is not writable: $install_root (choose another home or fix permissions)"
else
  [ -w "$HOME" ] || fail "install root is not writable: $install_root (choose another home or fix permissions)"
fi
min_kb=${CS_INSTALL_MIN_KB:-2097152}
case "$min_kb" in *[!0-9]*) fail 'CS_INSTALL_MIN_KB must be a positive integer';; esac
[ "$min_kb" -gt 0 ] 2>/dev/null || fail 'CS_INSTALL_MIN_KB must be a positive integer'
df_out=$(df -kP "$install_root" 2>/dev/null | awk 'NR==2 {print $4}')
[ -n "$df_out" ] || df_out=$(df -kP "$HOME" 2>/dev/null | awk 'NR==2 {print $4}')
[ -n "$df_out" ] || fail 'could not determine free disk space (df failed)'
if [ "$df_out" -lt "$min_kb" ]; then
  fail "not enough free disk space under $install_root (${df_out}KB available; need at least ${min_kb}KB for the release bundle and install)"
fi
UNAME_S=$(uname -s)
UNAME_M=$(uname -m)
setup_refuse() {
  case "$UNAME_S/$UNAME_M" in
    Linux/*)
      fail "ContextStack's setup currently supports Apple Silicon Macs. Linux support is coming; you can follow progress at $DOC_URL"
      ;;
    Darwin/x86_64)
      fail "ContextStack's setup requires an Apple Silicon Mac (this Mac has an Intel processor). Use a Mac with Apple silicon (M1 or later), or follow $DOC_URL for other options."
      ;;
    Darwin/arm64)
      return 0
      ;;
    *)
      fail "ContextStack's setup does not support $UNAME_S/$UNAME_M yet. See $DOC_URL for supported platforms."
      ;;
  esac
}
case "$FORCE_PLATFORM" in
  '') setup_refuse ;;
  1) ;;
esac
case "$UNAME_S/$UNAME_M" in
  Darwin/arm64) target=macos-aarch64 ;;
  Linux/x86_64) target=linux-x86_64 ;;
  Linux/aarch64|Linux/arm64) target=linux-aarch64 ;;
  *) fail "unsupported platform $UNAME_S/$UNAME_M" ;;
esac
case "$CHANNEL" in
  stable) default_base=https://github.com/contextstack/contextstack/releases/latest/download ;;
  latest) default_base=https://github.com/contextstack/contextstack/releases/download/channel-latest ;;
esac
base=${CS_INSTALL_BASE:-$default_base}
base=${base%/}
if [ "$base" != "$default_base" ]; then
  [ "${CS_INSTALL_DEV:-}" = 1 ] || fail 'non-default CS_INSTALL_BASE requires CS_INSTALL_DEV=1'
  echo "WARNING: CS_INSTALL_DEV=1: installing from non-default release source $base" >&2
fi
case "$base" in https://*|http://*) ;; *) fail 'set CS_INSTALL_BASE to the HTTPS release host';; esac
authority=${base#*://}; authority=${authority%%/*}
case "$authority" in
  localhost|localhost:*|127.0.0.1|127.0.0.1:*|'[::1]'|'[::1]':*) local_host=yes ;;
  *) local_host=no ;;
esac
case "$base/$local_host" in http://*/no) fail 'CS_INSTALL_BASE requires HTTPS except on localhost';; esac
fetch() {
  url=$1 destination=$2
  set -- --fail --show-error --location --retry 3 --retry-delay 2 --connect-timeout 15
  if [ -t 2 ]; then set -- "$@" --progress-bar; else set -- "$@" --silent; fi
  if [ "$local_host" != yes ]; then set -- "$@" --proto '=https'; fi
  curl "$@" "$url" -o "$destination"
}
printf 'Checking release…\n'
tmp=$(mktemp -d "${TMPDIR:-/tmp}/cs-install-XXXXXXXX")
trap 'rm -rf "$tmp"' 0 HUP INT TERM
fetch "$base/$CHANNEL.txt" "$tmp/channel"
entry=$(awk -v target="$target" '$1 == target { if (NF != 5) exit 1; count++; row=$0 } END { if (count != 1) exit 1; print row }' "$tmp/channel") || fail 'invalid or missing platform in channel manifest'
# Disable glob expansion while splitting the five whitespace-separated fields.
set -f
set -- $entry
[ "$#" -eq 5 ] || fail 'invalid channel manifest'
version=$2 commit=$3 name=$4 checksum=$5
case "$version" in ''|[!A-Za-z0-9]*|*[!A-Za-z0-9._-]*) fail 'invalid release version';; esac
case "$commit" in ''|*[!a-fA-F0-9]*) fail 'invalid commit';; esac
case "$name" in [A-Za-z0-9]*.tar.gz) ;; *) fail 'invalid tarball name';; esac
case "$name" in *[!A-Za-z0-9._-]*) fail 'invalid tarball name';; esac
case "$checksum" in *[!a-f0-9]*|'') fail 'invalid sha256';; esac
[ "${#checksum}" -eq 64 ] || fail 'invalid sha256'
head_proto=
[ "$local_host" = yes ] || head_proto="--proto =https"
size=$(curl $head_proto --silent --show-error --location --head --max-time 5 --retry-max-time 5 --retry 3 --retry-delay 2 --connect-timeout 15 "$base/$name" 2>/dev/null | awk 'tolower($1) == "content-length:" {gsub("\r", "", $2); size=$2} END {print size}') || size=
printf 'Downloading ContextStack %s (%s)…\n' "$version" "${size:-unknown size}${size:+ bytes}"
fetch "$base/$name" "$tmp/bundle.tar.gz"
fetch "$base/$name.sha256" "$tmp/checksum"
printf 'Verifying…\n'
sidecar=$(awk 'NR == 1 {print $1}' "$tmp/checksum")
actual=$(digest "$tmp/bundle.tar.gz"); actual=${actual%% *}
[ "$sidecar" = "$checksum" ] && [ "$actual" = "$checksum" ] || fail 'bundle sha256 mismatch; current release unchanged'
tar -tzf "$tmp/bundle.tar.gz" > "$tmp/members"
while IFS= read -r member; do
  case "$member" in /*|../*|*/../*|*/..|..) fail 'unsafe archive member; current release unchanged';; esac
done < "$tmp/members"
mkdir "$tmp/bundle"
tar -xzf "$tmp/bundle.tar.gz" -C "$tmp/bundle"
printf 'Installing…\n'
# The bundle carries Python as uv's install-only archive (python/<build>/cpython-*.tar.gz); node-install puts it in
# place, but these preflight checks need an interpreter first, so unpack one copy into the scratch dir.
set +f  # globbing was turned off while the channel line was split
set -- "$tmp"/bundle/python/*/cpython-*-install_only*.tar.gz
set -f
[ "$#" -eq 1 ] && [ -f "$1" ] || fail 'bundle has no single bundled Python archive; current release unchanged'
mkdir "$tmp/py"
tar -xzf "$1" -C "$tmp/py" || fail 'bundled Python archive did not unpack; current release unchanged'
[ -x "$tmp/py/python/bin/python3" ] || fail 'bundled Python archive has no python/bin/python3; current release unchanged'
"$tmp/py/python/bin/python3" - "$tmp/bundle" "$version" "$target" "$commit" "$LINK_DIR" "$YES" <<'PY'
import json
from pathlib import Path
import subprocess
import sys

try:
    bundle = Path(sys.argv[1])
    meta = json.loads((bundle / 'bundle.json').read_text())
    if (meta['release'], meta['target'], meta['code_commit']) != tuple(sys.argv[2:5]):
        raise ValueError('bundle metadata differs from channel manifest')
    try:
        terminal = open('/dev/tty')
    except OSError:
        terminal = None
    options = (["--link-dir", sys.argv[5]] if sys.argv[5] else []) + (["--yes"] if sys.argv[6] else [])
    try:
        subprocess.run(['sh', str(bundle / 'release/cs/scripts/node-install'), 'install', str(bundle), *options],
                       check=True, stdin=terminal)
    except BaseException:
        if terminal is not None:
            terminal.close()
        raise
    command = [str(Path.home() / '.contextstack/current/cs/scripts/cs'), 'init']
    if terminal is None or sys.argv[6]:
        command.append('--yes')
    try:
        init = subprocess.run(command, stdin=terminal)
        print('Open a new terminal (or run: `export PATH="$HOME/.local/bin:$PATH"`) to use `cs`')
        if init.returncode != 0:
            # ContextStack is installed; only setup is unfinished, and cs init said why just above.
            print('ContextStack is installed, but setup is not finished: see the message above, then run `cs init` again.')
            sys.exit(init.returncode)
    finally:
        if terminal is not None:
            terminal.close()
except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
    sys.exit(f'install: {error}')
PY
}
main "$@"
