#!/usr/bin/env python3
"""Exercise the actual retry block with decoy logins and no sandbox process."""
import ast
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'ops/bin'))
from _cli_native import stage_claude_login, stage_copies, finish_copies
sys.path.insert(0, str(next(p / 'tasks/regressions/_lib' for p in Path(__file__).resolve().parents if (p / 'tasks/regressions/_lib').is_dir())))

source = (ROOT / 'ops/bin/_executor.py').read_text()
tree = ast.parse(source)
refresh_fn = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'claude_auth_refresh_cmd')
idx = source.index('row["cli_invoked"] = True')
start = source.rfind('\n', 0, idx) + 1
end = source.index('seconds = time.time() - started', start)
_block = source[start:end].splitlines()
_min = min(len(line) - len(line.lstrip()) for line in _block if line.strip())
code = compile('\n'.join(line[_min:] if line.strip() else '' for line in _block), 'executor-retry', 'exec')
original = {'claudeAiOauth': {'accessToken': 'decoy-old', 'refreshToken': 'decoy-refresh'}}
updated = {'claudeAiOauth': {'accessToken': 'decoy-new', 'refreshToken': 'decoy-rotated'}}

for case in ('success', 'failed', 'output', 'extra_output', 'json', 'json_constant', 'oauth', 'keys', 'symlink', 'hardlink', 'directory', 'missing', 'owner', 'owner_replace', 'auth_refresh', 'task_after_auth_refresh'):
    with tempfile.TemporaryDirectory(prefix='refresh-test-') as raw:
        root = Path(raw).resolve()
        home, tmp, run = (root / name for name in ('home', 'tmp', 'run'))
        for path in (home / '.claude', tmp, run):
            path.mkdir(parents=True)
        real = home / '.claude/.credentials.json'
        real.write_text(json.dumps(original))
        records = stage_claude_login(tmp / 'task-home', home)
        # Even a forged identity in a task copy must never authorize writeback.
        records[0]['copy'].write_text(json.dumps(updated))
        finish_copies(records)
        assert json.loads(real.read_text()) == original
        records = stage_claude_login(tmp / 'task-home', home)
        row, calls = {}, []
        def sandbox(binary, args, *unused, **kwargs):
            assert '--tools' in args and args[args.index('--tools') + 1] == ''
            assert args[args.index('--settings') + 1] == '{"disableAllHooks":true}'
            assert args[args.index('--mcp-config') + 1] == '{"mcpServers":{}}'
            return ['sandbox-exec', '-f', 'sandbox.sb', binary, *args]
        def run_cli(command, working, limit, stage, env=None):
            calls.append(stage)
            if stage == 'task':
                records[0]['copy'].write_text(json.dumps(updated))
                return subprocess.CompletedProcess(command, 1, '', 'OAuth access token has expired')
            if stage == 'auth_refresh':
                copy = scope['refresh_copies'][0]['copy']
                assert Path(env['HOME']) == copy.parents[1]
                copy.write_text(json.dumps(updated))
                if case == 'json': copy.write_text('{')
                if case == 'json_constant': copy.write_text(json.dumps({'claudeAiOauth': dict(updated['claudeAiOauth'], accessToken=float('nan'))}))
                if case == 'oauth': copy.write_text('{"claudeAiOauth": []}')
                if case == 'keys': copy.write_text('{"claudeAiOauth": {"extra": "decoy"}}')
                if case in ('symlink', 'hardlink', 'directory', 'missing'):
                    copy.unlink()
                    if case == 'symlink': copy.symlink_to(real)
                    if case == 'hardlink': os.link(real, copy)
                    if case == 'directory': copy.mkdir()
                if case == 'owner': real.write_text('{"owner": "wins"}')
                if case == 'owner_replace':
                    replacement = real.with_suffix('.new')
                    replacement.write_text(json.dumps(original))
                    replacement.replace(real)
                if case == stage: raise subprocess.TimeoutExpired(command, limit)
                return subprocess.CompletedProcess(command, 1 if case == 'failed' else 0,
                    'wrong' if case == 'output' else 'AUTH_OK\nextra' if case == 'extra_output' else 'AUTH_OK\n', '')
            if case == stage: raise subprocess.TimeoutExpired(command, limit)
            expected = updated if case == 'success' else {'owner': 'wins'} if case == 'owner' else original
            assert json.loads(records[-1]['copy'].read_text()) == expected
            return subprocess.CompletedProcess(command, 0, '', '')
        scope = dict(Path=Path, row=row, run_cli=run_cli, cmd=['claude'], ws=tmp, timeout=1,
            binary='claude', args=['--model', 'sonnet'], cost_class='subscription', limits=None,
            AUTH_EXPIRED=re.compile('OAuth access token has expired'), LOGIN_REFRESH_CLIS=('pi', 'codex', 'grok'), cli_env={}, cli_tmp=tmp,
            run_dir=run, shutil=shutil, json=json,
            sandbox_caller={'run_protection': {}}, sandbox_policy=tmp / 'policy.json', sandbox_read_roots=[], sandbox_cmd=sandbox, budget_broker=None,
            native_links=[], tmpl={'copies': {str(tmp / 'task-home/.claude/.credentials.json'): '~/.claude/.credentials.json'}},
            HOME=home, login_snapshot={}, verify_login_files=lambda *args: None,
            finish_copies=finish_copies, stage_copies=stage_copies, native_copies=records)
        # stage_home appends to the retry block's own refreshed list.
        def stage_home(name, root=tmp):
            target = root / name
            target.mkdir()
            scope['refresh_copies'].extend(stage_claude_login(target, home))
            return str(target)
        scope['stage_claude_home'] = stage_home
        exec(compile(ast.Module(body=[refresh_fn], type_ignores=[]), 'refresh-command', 'exec'), scope)
        try:
            exec(code, scope)
        except subprocess.TimeoutExpired:
            assert case in ('auth_refresh', 'task_after_auth_refresh')
        else:
            assert case not in ('auth_refresh', 'task_after_auth_refresh')
        written = case in ('success', 'task_after_auth_refresh')
        reasons = {'failed': 'refresh_failed', 'output': 'unexpected_output',
            'extra_output': 'unexpected_output', 'json': 'invalid_json', 'json_constant': 'invalid_json', 'oauth': 'oauth_missing',
            'keys': 'oauth_keys_changed', 'symlink': 'invalid_copy', 'hardlink': 'invalid_copy',
            'directory': 'invalid_copy', 'missing': 'invalid_copy', 'owner': 'owner_changed',
            'owner_replace': 'owner_changed', 'auth_refresh': 'refresh_incomplete'}
        assert row['auth_refresh_writeback'] == ('written' if written else 'skipped:' + reasons[case]), (case, row)
        expected = updated if written else {'owner': 'wins'} if case == 'owner' else original
        assert json.loads(real.read_text()) == expected, case
        if written: assert real.stat().st_mode & 0o777 == 0o600
        finish_copies(records)
print('check: trusted refresh writeback guards, fresh retry copies and retry timeouts')
