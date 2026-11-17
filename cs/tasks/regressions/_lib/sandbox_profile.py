"""Render real Rust profiles through the production v1 request builder in tests."""
import os
from pathlib import Path
from unittest.mock import patch

from hermetic_env import hermetic_env

ROOT = Path(__file__).resolve().parents[4]


def render_profile(home, workspace=None, tmpdir=None, *, binary='/usr/bin/true', login=None,
                   read_links=(), read_roots=(), home_reads=(), caller=None, env=None):
    from _estate_io import native_sandbox_request, render_sandbox_request
    home = Path(home)
    workspace = workspace or home / 'workspace'
    tmpdir = tmpdir or home / 'tmp'
    for path in (workspace, tmpdir):
        Path(path).mkdir(parents=True, exist_ok=True)
    environment = {**hermetic_env(), 'HOME': str(home), 'PATH': '/usr/bin:/bin',
                   'CS_TOOLCHAINS_FILE': str(home / 'no-toolchains.json')}
    if env is not None:
        environment.update(env)
    with patch.dict(os.environ, environment, clear=True):
        request = native_sandbox_request(str(binary), workspace, tmpdir, home, login=login,
                                         read_links=read_links, read_roots=read_roots,
                                         home_reads=home_reads, caller=caller, env=environment)
    sandbox = os.environ.get('CS_SANDBOX_BIN', str(ROOT / 'target/release/cs-sandbox'))
    return render_sandbox_request(sandbox, ROOT / 'cs/policy/sandbox.json', request).decode()


def bridge_denies(profile):
    """Select bridge socket denies from an already rendered profile."""
    return [line for line in profile.splitlines() if line.startswith('(deny ')
            and ('lead-mcp-[^/]+' in line or '"/lead-sessions(/' in line)]
