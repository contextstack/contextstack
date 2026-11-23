#!/usr/bin/env python3
"""Measure every first-party crate linked to cs, including path dependencies."""
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
CAP = 3500
metadata = json.loads(subprocess.check_output(
    ['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=ROOT))
packages = {p['id']: p for p in metadata['packages']}
nodes = {n['id']: n for n in metadata['resolve']['nodes']}
pending = [p['id'] for p in packages.values() if p['name'] == 'cs-harness' and p['source'] is None]
seen, files = set(), {}
while pending:
    key = pending.pop()
    if key in seen:
        continue
    seen.add(key)
    pending.extend(nodes[key]['dependencies'])
    package = packages[key]
    if package['source'] is not None:
        continue
    base = Path(package['manifest_path']).parent
    for path in (base / 'src').rglob('*.rs'):
        source = path.read_text()
        if 'rustfmt::skip' in source:
            raise SystemExit('cs-harness cap forbids rustfmt::skip: ' + str(path))
        # Local test modules are cfg(test), at the end of each production file.
        production = source.split('\n#[cfg(test)]\nmod tests {', 1)[0]
        files[str(path.relative_to(ROOT))] = sum(
            bool(line.strip()) and not line.lstrip().startswith('//')
            for line in production.splitlines())
# The runner's size budget: formatted non-test lines across the linked first-party crates.
cap = CAP
result = {'non_test_nonblank_noncomment': sum(files.values()), 'cap': cap,
          'files': files, 'linked_first_party_crates': sum(packages[k]['source'] is None for k in seen),
          'resolved_third_party_crates': sum(packages[k]['source'] is not None for k in seen)}
print(json.dumps(result, sort_keys=True))
if result['non_test_nonblank_noncomment'] > cap:
    raise SystemExit('Rust runner exceeds its measured cap')
