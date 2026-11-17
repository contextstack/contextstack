"""Publish artifacts from an already staged bundle, and merge one-platform channel manifests. No network."""
import hashlib
import json
import re
import sys
from pathlib import Path
import tarfile


PLATFORMS = ('linux-aarch64', 'linux-x86_64', 'macos-aarch64')
VERSION = re.compile(r'[A-Za-z0-9][A-Za-z0-9._-]*')
COMMIT = re.compile(r'[a-fA-F0-9]+')
TARBALL = re.compile(r'[A-Za-z0-9][A-Za-z0-9._-]*\.tar\.gz')
SHA = re.compile(r'[a-f0-9]{64}')


def publish(bundle, destination, channel):
    bundle, destination = Path(bundle), Path(destination)
    meta = json.loads((bundle / 'bundle.json').read_text())
    target, version, commit = meta['target'], meta['release'], meta['lab_commit']
    if not isinstance(version, str) or not VERSION.fullmatch(version):
        raise ValueError('invalid release version')
    if not isinstance(commit, str) or not COMMIT.fullmatch(commit):
        raise ValueError('invalid release commit')
    if channel not in ('stable', 'latest'):
        raise ValueError('invalid channel')
    if target not in PLATFORMS:
        raise ValueError('unsupported public release platform: ' + target)
    destination.mkdir(parents=True, exist_ok=True)
    if destination.resolve().is_relative_to(bundle.resolve()):
        raise ValueError('publish directory must be outside bundle')
    manifest = destination / (channel + '.json')
    index = json.loads(manifest.read_text()) if manifest.exists() else {}
    if (index.get('version'), index.get('commit')) != (version, commit):
        index = {'version': version, 'commit': commit, 'platforms': {}}
    name = f'contextstack-{target}.tar.gz'
    with tarfile.open(destination / name, 'w:gz') as tar:
        for child in sorted(bundle.iterdir()):
            tar.add(child, arcname=child.name)
    checksum = _sha256(destination / name)
    (destination / (name + '.sha256')).write_text(f'{checksum}  {name}\n')
    index['platforms'][target] = {'tarball': name, 'sha256': checksum}
    write_channel(destination, channel, index)
    return index


def merge(directories, destination, channel):
    """Union per-platform `<channel>.json` files into one json and txt.

    `publish` records only the platform it just packed. A matrix build therefore
    leaves three one-platform manifests; this combines them. Every directory must
    hold that platform's tarball and `.sha256`, and the three public platforms
    must agree on version and commit.
    """
    if channel not in ('stable', 'latest'):
        raise ValueError('invalid channel')
    directories = [Path(directory) for directory in directories]
    if not directories:
        raise ValueError('no manifests to merge')
    version = commit = None
    platforms = {}
    for directory in directories:
        index = json.loads((directory / (channel + '.json')).read_text())
        _check_identity(index)
        if version is None:
            version, commit = index['version'], index['commit']
        elif (index['version'], index['commit']) != (version, commit):
            raise ValueError('channel manifests disagree on version or commit')
        found = index.get('platforms') or {}
        if not found:
            raise ValueError('channel manifest has no platforms')
        for platform, entry in found.items():
            checked = _check_entry(directory, platform, entry)
            previous = platforms.get(platform)
            if previous is not None and previous != checked:
                raise ValueError('platform entries disagree: ' + platform)
            platforms[platform] = checked
    missing = [platform for platform in PLATFORMS if platform not in platforms]
    if missing:
        raise ValueError('missing platforms: ' + ', '.join(missing))
    index = {'version': version, 'commit': commit, 'platforms': {platform: platforms[platform] for platform in PLATFORMS}}
    write_channel(destination, channel, index)
    return index


def write_channel(destination, channel, index):
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    manifest = destination / (channel + '.json')
    temporary = manifest.with_suffix('.json.tmp')
    temporary.write_text(json.dumps(index, indent=2) + '\n')
    temporary.replace(manifest)
    text_manifest = manifest.with_suffix('.txt')
    temporary = text_manifest.with_suffix('.txt.tmp')
    temporary.write_text(''.join(
        f"{platform} {index['version']} {index['commit']} {entry['tarball']} {entry['sha256']}\n"
        for platform, entry in sorted(index['platforms'].items())))
    temporary.replace(text_manifest)


def _check_identity(index):
    version, commit = index.get('version'), index.get('commit')
    if not isinstance(version, str) or not VERSION.fullmatch(version):
        raise ValueError('invalid release version')
    if not isinstance(commit, str) or not COMMIT.fullmatch(commit):
        raise ValueError('invalid release commit')


def _check_entry(directory, platform, entry):
    if platform not in PLATFORMS:
        raise ValueError('unsupported public release platform: ' + str(platform))
    name, checksum = entry['tarball'], entry['sha256']
    if not isinstance(name, str) or not TARBALL.fullmatch(name) or name != f'contextstack-{platform}.tar.gz':
        raise ValueError('invalid tarball name')
    if not isinstance(checksum, str) or not SHA.fullmatch(checksum):
        raise ValueError('invalid sha256')
    sidecar = (directory / (name + '.sha256')).read_text().split()
    if sidecar[:2] != [checksum, name]:
        raise ValueError('sha256 sidecar differs from channel manifest')
    if _sha256(directory / name) != checksum:
        raise ValueError('tarball sha256 mismatch')
    return {'tarball': name, 'sha256': checksum}


def _sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def main(argv):
    if len(argv) < 5 or argv[1] != 'merge':
        print('usage: node_publish.py merge CHANNEL DEST DIR [DIR ...]', file=sys.stderr)
        return 2
    try:
        merge(argv[4:], argv[3], argv[2])
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print('node_publish: %s' % error, file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
