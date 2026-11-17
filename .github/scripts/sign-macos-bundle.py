#!/usr/bin/env python3
"""Sign the staged macOS bundle before regenerating its publish artifacts."""
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

SECRETS = ('MACOS_CERT_P12_BASE64', 'MACOS_CERT_PASSWORD', 'APPLE_TEAM_ID',
           'ASC_KEY_ID', 'ASC_ISSUER_ID', 'ASC_KEY_P8_BASE64')
MAGIC = {bytes.fromhex(x) for x in ('feedface', 'cefaedfe', 'feedfacf', 'cffaedfe',
                                   'cafebabe', 'bebafeca', 'cafebabf', 'bfbafeca')}


def run(*args, **kwargs):
    # Tool output may contain certificate subjects; never log command arguments or secrets.
    return subprocess.run(args, check=True, **kwargs)


def sha(path):
    with path.open('rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def manifest_sha(path):
    """The hash node-bundle records and verifies: a symlink is hashed by its target string, not followed."""
    if path.is_symlink():
        return hashlib.sha256(('symlink:' + os.readlink(path)).encode()).hexdigest()
    return sha(path)


# node-bundle ships these as hard links to bin/ and the installer links them to bin/ again. codesign rewrites each
# file separately, which breaks the link and gives the two copies different signatures (the installed copy then
# fails verify, found on a clean VM 2026-11-17), so after signing they are linked to the signed bin/ file again.
TARGET_LINKS = {'cs-tools': 'release/target/release/cs-tools', 'cs-sandbox': 'release/target/release/cs-sandbox'}


def relink_targets(bundle):
    for name, rel in TARGET_LINKS.items():
        src, dst = bundle / 'release/bin' / name, bundle / rel
        if src.is_file() and (dst.exists() or dst.is_symlink()):
            dst.unlink()
            os.link(src, dst)


def sign_tree(root, identity, keychain):
    count = 0
    for path in sorted(root.rglob('*')):
        if path.is_symlink() or not path.is_file():
            continue
        with path.open('rb') as handle:
            magic = handle.read(4)
        if magic not in MAGIC:
            continue
        run('codesign', '--force', '--options', 'runtime', '--timestamp',
            '--keychain', str(keychain), '--sign', identity, str(path))
        run('codesign', '--verify', '--strict', '--verbose=2', str(path))
        count += 1
    if not count:
        raise RuntimeError('no Mach-O files found')


def main():
    bundle, destination, channel = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
    meta_path = bundle / 'bundle.json'
    meta = json.loads(meta_path.read_text())
    meta['signing'] = {'status': 'unsigned'}
    if not all(os.environ.get(name) for name in SECRETS):
        print('::warning::MACOS RELEASE IS UNSIGNED: signing/notarization secrets are missing; skipping both.')
    else:
        keychain = Path(os.environ['RUNNER_TEMP']) / 'contextstack-signing.keychain-db'
        with tempfile.TemporaryDirectory(dir=os.environ['RUNNER_TEMP']) as tmp:
            work = Path(tmp)
            cert, key = work / 'certificate.p12', work / 'AuthKey.p8'
            cert.write_bytes(base64.b64decode(os.environ['MACOS_CERT_P12_BASE64'], validate=True))
            key.write_bytes(base64.b64decode(os.environ['ASC_KEY_P8_BASE64'], validate=True))
            cert.chmod(0o600)
            key.chmod(0o600)
            password = base64.b64encode(os.urandom(32)).decode()
            # Always-step cleanup in the workflow also covers interruption/failure.
            try:
                run('security', 'create-keychain', '-p', password, str(keychain))
                run('security', 'set-keychain-settings', '-lut', '21600', str(keychain))
                run('security', 'unlock-keychain', '-p', password, str(keychain))
                # codesign looks identities up through the user search list even with --keychain: add ours first
                # (CI said "no identity found" right after find-identity listed it, 2026-11-13).
                searched = [line.strip().strip('"') for line in subprocess.check_output(
                    ['security', 'list-keychains', '-d', 'user'], text=True).splitlines() if line.strip()]
                run('security', 'list-keychains', '-d', 'user', '-s', str(keychain), *searched)
                run('security', 'import', str(cert), '-P', os.environ['MACOS_CERT_PASSWORD'],
                    '-k', str(keychain), '-T', '/usr/bin/codesign', stdout=subprocess.DEVNULL)
                run('security', 'set-key-partition-list', '-S', 'apple-tool:,apple:,codesign:',
                    '-s', '-k', password, str(keychain), stdout=subprocess.DEVNULL)
                identities = subprocess.check_output(['security', 'find-identity', '-v', '-p',
                                                     'codesigning', str(keychain)], text=True)
                import re
                matches = re.findall(r'\b([0-9A-F]{40}) "Developer ID Application: .* \(' +
                                     re.escape(os.environ['APPLE_TEAM_ID']) + r'\)"', identities)
                if len(matches) != 1:
                    raise RuntimeError('expected one Developer ID Application identity for APPLE_TEAM_ID')
                identity = matches[0]  # its SHA-1: unambiguous, and needs no name lookup
                # The bundled Python archive stays byte-for-byte as python-build-standalone published it: uv
                # verifies it against its own checksum on install, so re-packing it with signed binaries made every
                # signed release fail to install (hash mismatch, found on a clean VM 2026-11-13). curl | sh installs
                # carry no quarantine, so its unsigned binaries run; ours and uv are signed and notarized.
                sign_tree(bundle, identity, keychain)
                relink_targets(bundle)
                submission = work / 'notarize'
                submission.mkdir()
                import shutil
                # Notarize only what we sign: Apple rejects a submission containing the untouched Python archive
                # (its binaries are not ours to sign without breaking uv's checksum).
                shutil.copytree(bundle, submission / 'bundle', symlinks=True,
                                ignore=lambda folder, names: ['python'] if Path(folder) == bundle else [])
                zip_path = work / 'submission.zip'
                run('ditto', '-c', '-k', '--keepParent', str(submission), str(zip_path))
                result = work / 'notary.json'
                auth = ('--key', str(key), '--key-id', os.environ['ASC_KEY_ID'],
                        '--issuer', os.environ['ASC_ISSUER_ID'])
                with result.open('w') as output:
                    submitted = subprocess.run(['xcrun', 'notarytool', 'submit', str(zip_path),
                                                *auth, '--wait', '--output-format', 'json'], stdout=output)
                response = json.loads(result.read_text())
                if submitted.returncode or response.get('status') != 'Accepted':
                    if response.get('id'):
                        run('xcrun', 'notarytool', 'log', response['id'], *auth)
                    raise RuntimeError('Apple notarization failed; see notarization log above')
                meta['signing'] = {'status': 'signed', 'notarization': 'accepted'}
                meta['uv']['binary_sha256'] = sha(bundle / 'uv/uv')
                meta['files'] = {name: manifest_sha(bundle / name) for name in meta['files']}
            finally:
                if 'searched' in locals():
                    subprocess.run(['security', 'list-keychains', '-d', 'user', '-s', *searched],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                subprocess.run(['security', 'delete-keychain', str(keychain)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    meta_path.write_text(json.dumps(meta, indent=2) + '\n')
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'cs/scripts'))
    from node_publish import publish
    publish(bundle, destination, channel)


if __name__ == '__main__':
    try:
        main()
    except Exception:
        # Avoid traceback command arguments (security import contains a password).
        print('::error::macOS signing/notarization failed; release will not be uploaded.', file=sys.stderr)
        sys.exit(1)
