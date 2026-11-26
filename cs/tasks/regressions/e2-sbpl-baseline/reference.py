#!/usr/bin/env python3
"""Render production v1 requests against the frozen SBPL hashes and fixtures."""
import ast
import argparse
from contextlib import ExitStack
import hashlib
import difflib
import io
import json
import os
from pathlib import Path
import sys
import subprocess
import re
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

# Every absolute fixture path is virtual; real materialization is confined to target/.

ROOT = Path(__file__).resolve().parents[4]
OPS = ROOT / 'cs/ops/bin'
FIXTURES = ROOT / 'sandbox/tests/fixtures/e2-baseline'
TEMPLATES = json.loads((OPS / 'executor-templates.json').read_text())
ADAPTERS = json.loads((OPS / 'cli-profiles.json').read_text())
V = Path('/e2-baseline/owner +[test]')
WS, TMP = V / 'workspace', V / 'private'
CHECK_SCRATCH = V / 'check-scratch'
ROUTES = ('native-task', 'frozen-check', 'auth-refresh', 'muse', 'muse-check', 'cli-adapter', 'lead-host')
REPRESENTATIVES = {'native-task': 'pi', 'frozen-check': 'goose', 'auth-refresh': 'pi',
                   'muse': 'pi', 'muse-check': 'pi', 'lead-host': 'antigravity'}
STATES = ('unconfigured', 'cargo', 'php', 'uv', 'ssh-agent', 'repo-lane')
# Imports happen with a fake owner environment too (some modules cache HOME).
sys.path.insert(0, str(OPS))
with patch.dict(os.environ, {'HOME': str(V), 'PATH': '/e2-baseline/bin'}, clear=True):
    import _executor as executor
    import _cli_native as native
    import _toolchains as toolchains
    import _muse as muse
    import _muse_adapter as muse_adapter
    import _cli_adapter as adapter
    import _lead_host as lead
    import _lanes as lanes
    import _estate_io as estate_io


def inventory():
    """AST call-site inventory catches new routes, including inline builders.

    Function + callee + multiplicity is stable across unrelated line-number changes.
    Tracking sandbox-exec literals also catches a new standalone profile builder.
    """
    result = []
    tracked = {'sandbox_cmd', 'sandbox_command', 'sandbox_builder', 'check_command'}
    for path in sorted(OPS.glob('*.py')):
        tree = ast.parse(path.read_text())
        def visit(node, scope='module'):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                scope = node.name
            if isinstance(node, ast.Call):
                name = node.func.id if isinstance(node.func, ast.Name) else getattr(node.func, 'attr', '')
                if name in tracked:
                    result.append(f'{path.name}:{scope}:{name}')
            if isinstance(node, ast.Constant) and isinstance(node.value, str) and node.value in ('sandbox-exec', '/usr/bin/sandbox-exec'):
                result.append(f'{path.name}:{scope}:sandbox-exec')
            for child in ast.iter_child_nodes(node):
                visit(child, scope)
        visit(tree)
    return sorted(result)


class Profile:
    def __init__(self):
        self.text = ''
    def write_bytes(self, raw):
        self.text = raw.decode()


def render(route, name, state):
    tmpl = TEMPLATES[name] if name in TEMPLATES else ADAPTERS[name]
    files = {V / '.local/share/opencode/opencode.db': b'fake database', V / 'code/INVARIANTS.md': b'fixture'}
    dirs = {V, WS, TMP, V / 'Library/Keychains', V / '.contextstack/state', V / 'code', V / 'config'}
    if route in ('frozen-check', 'muse-check'):
        dirs.add(CHECK_SCRATCH)
    links = {}
    binary = V / '.local/bin' / Path(tmpl['binary']).name
    files[binary] = b'#!/usr/bin/env node\n'
    node = V / '.local/bin/node'
    files[node] = b'node fixture'
    for src in tmpl.get('links', {}).values():
        if src.startswith('~/'):
            if src in native._DIR_LOGIN_LINKS:
                dirs.add(V / src[2:])
            else:
                files[V / src[2:]] = b'{}'
    entries = []
    if state == 'cargo':
        entries = [dict(name='cargo', bin=str(V / '.cargo/bin'), rustup=str(V / '.rustup'),
                        toolchain=str(V / '.rustup/toolchains/fixed'), registry=str(V / '.cargo/registry'))]
        files[V / '.cargo/bin/rustup'] = b'rustup'
        links.update({V / '.cargo/bin/cargo': V / '.cargo/bin/rustup', V / '.cargo/bin/rustc': V / '.cargo/bin/rustup'})
    elif state == 'php':
        entries = [dict(name='php', bin=str(V / 'Application Support/php/bin'))]
        files[V / 'Application Support/php/bin/php'] = b'php'
    elif state == 'uv':
        entries = [dict(name='uv', bin=str(V / '.local/uv'), python=str(V / '.local/python'))]
        files[V / '.local/uv/uv'] = b'uv'
    for entry in entries:
        dirs.update(Path(value) for key, value in entry.items() if key != 'name')
    for path in (*files, *dirs, *links):
        dirs.update(path.parents)
    def resolve(path, **_):
        path = Path(path)
        for _ in range(64):
            for parent in reversed((path, *path.parents)):
                if parent in links:
                    path = links[parent] / path.relative_to(parent)
                    break
            else:
                return path
        raise ValueError('virtual symlink loop')
    def open_file(path, mode='r', **_):
        raw = files.get(resolve(path))
        if raw is None:
            raise FileNotFoundError(str(path))
        return io.BytesIO(raw) if 'b' in mode else io.StringIO(raw.decode())
    env = {'HOME': str(V), 'PATH': str(node.parent), 'TMPDIR': str(TMP),
           'CS_STATE_ROOT': str(V / '.contextstack/state'), 'CS_CODE_ROOT': str(V / 'code'),
           'CS_CONFIG_ROOT': str(V / 'config'), 'CS_WORKSPACES_ROOT': str(WS)}
    if state == 'ssh-agent':
        env['SSH_AUTH_SOCK'] = '/e2-baseline/agent.sock'
    profile = Profile()
    with ExitStack() as stack:
        for obj, attr, value in (
            (sys, 'platform', 'darwin'), (executor, 'HOME', V),
            (Path, 'resolve', resolve), (Path, 'home', classmethod(lambda cls: V)),
            (Path, 'is_file', lambda p: resolve(p) in files or str(p) == '/usr/bin/sandbox-exec'),
            (Path, 'is_dir', lambda p: resolve(p) in dirs),
            (Path, 'exists', lambda p: resolve(p) in files or resolve(p) in dirs),
            (Path, 'lstat', lambda p: SimpleNamespace(st_uid=os.getuid())),
            (Path, 'is_symlink', lambda p: p in links), (Path, 'open', open_file),
            (os.path, 'isfile', lambda p: resolve(Path(p)) in files),
            (os.path, 'exists', lambda p: resolve(Path(p)) in files or resolve(Path(p)) in dirs),
            (os.path, 'realpath', lambda p: str(resolve(Path(p)))),
            (os.path, 'isdir', lambda p: resolve(Path(p)) in dirs),
            (os, 'getuid', lambda: 501),
            (os, 'readlink', lambda p: str(links[Path(p)])),
            (native.shutil, 'which', lambda command: str(node) if command == 'node' else command if Path(command).is_absolute() else None),
            (toolchains, '_entries', lambda: entries),
            (executor, 'require_executor_sandbox', lambda: None),
        ):
            stack.enter_context(patch.object(obj, attr, value))
        stack.enter_context(patch.dict(os.environ, env, clear=True))
        toolchains.discover.cache_clear()
        toolchains.discover_sandbox_links.cache_clear()
        captured = {}
        def capture_request(code_root, policy, request):
            captured.update(request)
            return b'(version 1)\n'
        stack.enter_context(patch.object(estate_io, 'render_native_sandbox', capture_request))
        extra = {}
        read_roots = []
        if state == 'repo-lane':
            common = V / 'repo.git'
            gitdir = common / 'worktrees/fixed'
            stack.enter_context(patch.object(lanes, 'git_common_dir', lambda _: common))
            stack.enter_context(patch.object(lanes, 'assert_lane_git_pointer', lambda *_: gitdir))
            stack.enter_context(patch.object(lanes, 'assert_lane_refs', lambda *_: None))
            stack.enter_context(patch.object(lanes, 'prepare_lane_refs', lambda *_: None))
            stack.enter_context(patch.object(lanes, 'worktrees', lambda _: [str(WS), str(V / 'other-lane')]))
            read_roots.extend((common, gitdir))
        # Staged links are the real template's source targets; production resolves
        # the private symlinks to these same targets before emitting any rule.
        staged = [V / src[2:] for src in tmpl.get('links', {}).values() if src.startswith('~/')]
        if tmpl.get('keychain') == 'login':
            staged.append(V / 'Library/Keychains')
        run = V / '.contextstack/state/runs/fixed'
        protected = run / 'budget-controls'
        login = tmpl
        selected = binary
        if route == 'frozen-check':
            selected = run / 'stimulus/check'
            login, staged = {}, []
            read_roots.insert(0, selected.parent)
        elif route == 'auth-refresh':
            refresh = run / 'claude-refresh-home'
            read_roots.insert(0, refresh)
        if route not in ('native-task', 'frozen-check', 'auth-refresh', 'lead-host'):
            caller = {}
            if state == 'repo-lane':
                caller, _ = estate_io.native_caller({}, V, run, WS, TMP, dict(main=common, lane='fixed'))
                caller.pop('run_protection')
            grants = [dict(path=str(p), access='read') for p in native.declared_home_reads(tmpl, V, TMP)]
            if state == 'repo-lane':
                grants += [dict(path=str(p), access='metadata') for p in common.parents if p.is_relative_to(V)]
            caller['caller_grants'] = grants
            extra = caller
        if route in ('native-task', 'frozen-check', 'auth-refresh'):
            caller, _ = estate_io.native_caller(tmpl, V, run, WS, TMP,
                                                dict(main=common, lane='fixed') if state == 'repo-lane' else None)
            if route == 'auth-refresh':
                caller['run_protection']['refresh_write_paths'] = [str(refresh)]
            executor.sandbox_cmd(str(selected), [], WS, TMP, profile, protected,
                                 read_links=staged, login_writes=login, read_roots=read_roots,
                                 env=env, caller=caller, policy=ROOT / 'cs/policy/sandbox.json',
                                 check_scratch=CHECK_SCRATCH if route == 'frozen-check' else None)
        elif route == 'lead-host':
            runtime = V / '.contextstack/state/lead-sessions/fixed/runtime-home'
            # make_session does not pass a template/login write grant.
            lead.sandbox_builder(WS, TMP, profile, read_links=[V / 'Library/Keychains'],
                                 write_roots=[runtime], allow_sockets=['/e2-baseline/lead-mcp-fixed'])(str(binary), [])
        elif route == 'muse-check':
            profile.text = muse_adapter.check_command(run / 'stimulus/check', WS, V, [WS / 'protected'],
                                                      extra=extra, tmp=TMP, check_scratch=CHECK_SCRATCH)[2]
        elif route == 'cli-adapter':
            profile.text = adapter.command(ADAPTERS[name], binary, WS, TMP, V, [WS / 'protected'], [], extra=extra)[2]
        else:
            profile.text = muse.sandbox_command([str(binary)], WS, V, [WS / 'protected'], extra=extra)[2]
    return captured, (files, dirs, links)


def cases():
    for route in ROUTES:
        names = ADAPTERS if route == 'cli-adapter' else TEMPLATES
        for name in sorted(names):
            for state in STATES:
                yield dict(id=f'{route}/{name}/{state}', route=route, template=name, state=state,
                           rust=True)


def rust_report(binary_path, selected):
    """Materialize the identical fake filesystem only for the Rust subprocess.

    Normalize the materialized owner paths and the two host-derived scratch paths.
    PATH and sockets stay fake.
    """
    report = []
    darwin_temp = Path(subprocess.check_output(
        ['/usr/bin/getconf', 'DARWIN_USER_TEMP_DIR'], text=True).strip()).resolve(strict=True)
    scratch_paths = (
        (str(darwin_temp.parent), '/private/var/folders/xx/e2-baseline-owner'),
        (f'/private/tmp/claude-{os.getuid()}', '/private/tmp/claude-501'),
    )
    for case in selected:
        request, (files, dirs, links) = render(case['route'], case['template'], case['state'])
        with tempfile.TemporaryDirectory(prefix='e2-sbpl-', dir=ROOT / 'target') as scratch:
            home = str(Path(scratch).resolve() / 'owner +[test]')
            def mapped(path):
                return Path(str(path).replace(str(V), home))
            for directory in sorted(dirs, key=lambda p: len(p.parts)):
                if directory.is_relative_to(V):
                    mapped(directory).mkdir(parents=True, exist_ok=True)
            for path, raw in files.items():
                mapped(path).parent.mkdir(parents=True, exist_ok=True)
                mapped(path).write_bytes(raw)
                mapped(path).chmod(0o700)
            for path, target in links.items():
                mapped(path).parent.mkdir(parents=True, exist_ok=True)
                mapped(path).symlink_to(mapped(target))
            mapped_request = json.loads(json.dumps(request).replace(str(V), home))
            # This path need not exist for either renderer. Keeping its logical
            # ancestry avoids introducing the scratch directory's real parents.
            if request.get('protected'):
                mapped_request['protected'] = request['protected']
            actual = estate_io.render_sandbox_request(
                binary_path, ROOT / 'cs/policy/sandbox.json', mapped_request).decode()
            actual = actual.replace(re.escape(home), re.escape(str(V)))
            actual = actual.replace(home, str(V))
            for host, fixture in scratch_paths:
                actual = actual.replace(f'(subpath "{host}")', f'(subpath "{fixture}")')
            actual = actual.encode()
        hashes = json.loads((FIXTURES / 'profiles.sha256.json').read_text())
        representative = case['route'] == 'cli-adapter' or REPRESENTATIVES[case['route']] == case['template']
        reference = FIXTURES / (case['id'] + '.sb')
        if hashlib.sha256(actual).hexdigest() != hashes[case['id']] or (representative and reference.read_bytes() != actual):
            report.append(case['id'])
            drift = Path(tempfile.mkdtemp(prefix='e2-sbpl-drift-', dir=ROOT / 'target'))
            rendered = drift / 'rendered.sb'
            rendered.write_bytes(actual)
            print(f'Rust SBPL drift: {case["id"]}; rendered profile: {rendered}', file=sys.stderr)
            name = case['template'] if case['route'] == 'cli-adapter' else REPRESENTATIVES[case['route']]
            reference = FIXTURES / case['route'] / name / (case['state'] + '.sb')
            diff = ''.join(difflib.unified_diff(reference.read_text().splitlines(True),
                           actual.decode().splitlines(True), fromfile=str(reference), tofile=str(rendered)))
            (drift / 'representative.diff').write_text(diff)
            print(diff, file=sys.stderr)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--production', action='store_true', help='compatibility alias; all renders use production')
    parser.add_argument('--rust-report', metavar='BINARY', help='use this Rust binary')
    parser.add_argument('--case', choices=[case['id'] for case in cases()])
    parser.add_argument('--diff', action='store_true', help='drift diffs are always emitted and saved')
    args = parser.parse_args()
    manifest = dict(routes=list(ROUTES), templates=sorted(TEMPLATES), adapters=sorted(ADAPTERS),
                    states=list(STATES), environment=dict(home=str(V), workspace=str(WS), tmpdir=str(TMP),
                    path=str(V / '.local/bin'), ssh_auth_sock='unset except /e2-baseline/agent.sock in ssh-agent'),
                    dispatch_inventory=inventory(), cases=list(cases()))
    assert json.loads((FIXTURES / 'manifest.json').read_text()) == manifest, 'route/template/state inventory changed; review baseline completeness'
    hashes = json.loads((FIXTURES / 'profiles.sha256.json').read_text())
    assert set(hashes) == {c['id'] for c in cases()}, 'orphan/missing profile hash'
    representatives = {c['id'] for c in cases() if c['route'] == 'cli-adapter' or REPRESENTATIVES[c['route']] == c['template']}
    assert {str(p.relative_to(FIXTURES))[:-3] for p in FIXTURES.rglob('*.sb')} == representatives
    binary = args.rust_report or os.environ.get('CS_SANDBOX_BIN', str(ROOT / 'target/release/cs-sandbox'))
    (ROOT / 'target').mkdir(exist_ok=True)
    selected = [case for case in cases() if args.case is None or case['id'] == args.case]
    failures = rust_report(binary, selected)
    assert not failures, f'Rust SBPL drift in {len(failures)} cases'
    print(f'e2-sbpl-baseline: PASS ({len(selected)} production profiles)')


if __name__ == '__main__':
    main()
