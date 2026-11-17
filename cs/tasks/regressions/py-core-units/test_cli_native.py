"""Native sandbox rules and login lifecycles, using only temporary owner homes."""
import json
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

HERE = Path(__file__).resolve().parent
OPS = HERE.parents[2] / 'ops/bin'
sys.path[:0] = [str(HERE.parent / '_lib'), str(OPS)]
from hermetic_env import hermetic_env
from scratch import scratch_dir
import _cli_native as m
from sandbox_profile import render_profile


class NativeTests(unittest.TestCase):
    def setUp(self):
        self.scratch = scratch_dir(prefix='native-units-', dir='/tmp')
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.__enter__()).resolve()
        self.home = self.root / 'owner'
        self.tmp = self.root / 'run'
        self.home.mkdir()
        self.tmp.mkdir()
        self.environment = patch.dict(os.environ, {**hermetic_env(), 'HOME': str(self.home),
                                    'CS_TOOLCHAINS_FILE': str(self.root / 'absent.json')}, clear=True)
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def file(self, rel, data=b'login', home=None):
        path = (home or self.home) / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def profile(self, **fields):
        return render_profile(self.home, self.tmp / 'workspace', self.tmp, **fields)

    def test_caller_grants_pin_operations_escaping_and_order(self):
        text = self.profile(caller={'caller_grants': [dict(path='/a"b', access='read_write')]})
        self.assertIn('(allow file-read* file-write* (subpath "/a\\"b"))', text)

    def test_home_grants_metadata_and_cargo_cache_variations(self):
        for cargo in (False, True):
            with patch('_toolchains.has_cargo_toolchain', return_value=cargo):
                actual = self.profile()
            rels = ['.contextstack/current'] + ([] if cargo else ['.cargo/registry', '.cargo/git', '.rustup']) + ['.npm', 'Library/Caches/pip', 'go/pkg/mod']
            for rel in rels:
                self.assertIn(f'(allow file-read* (subpath "{self.home}/{rel}"))', actual)
                for parent in (self.home/rel).parents:
                    if parent.is_relative_to(self.home):
                        self.assertIn(f'(allow file-read-metadata (literal "{parent}"))', actual)
        for root in (self.home, self.root):
            with self.assertRaises(RuntimeError):
                self.profile(read_roots=[root])
        self.assertEqual(m.declared_home_reads({'home_reads': ['~/src', '{{tmp}}/cache']}, self.home, self.tmp),
                         [self.home/'src', self.tmp/'cache'])
        for raw in ('/outside', 1, '../bad'):
            with self.assertRaises(ValueError):
                m.declared_home_reads({'home_reads': [raw]}, self.home, self.tmp)

    def test_read_profile_preserves_frozen_login_directory_and_keychain_rules(self):
        directory = self.home/'Library/Keychains'
        directory.mkdir(parents=True)
        login = self.file('.grok/auth.json')
        (self.home/'.aws').symlink_to(self.home/'aws-target')
        actual = self.profile(read_links=[login, login, directory, self.home/'missing'])
        expected = (HERE/'fixtures/native-read-rules.txt').read_text().replace('@REGEX_ROOT@', re.escape(str(self.root))).replace('@ROOT@', str(self.root))
        for line in expected.splitlines():
            if line != '(allow process-fork)':
                self.assertIn(line, actual)

    def test_login_filters_and_inode_protection(self):
        tmpl = {'login_writes': [{'literal': '~/.grok/auth.json'}, {'subpath': '~/.grok/locks'},
                                  {'regex': '~/.grok/auth[0-9]+'}]}
        actual = self.profile(login=tmpl)
        expected = (HERE/'fixtures/native-write-rules.txt').read_text().replace('@REGEX_ROOT@', re.escape(str(self.root))).replace('@ROOT@', str(self.root)).rstrip('\n')
        self.assertIn(expected, actual)
        for entry in ({'bad': '~/a/b'}, {'literal': '/a'}, {'literal': '~/../a'}, {'literal': '~/a/'}, {'literal': '~/a/b', 'regex': '~/a/c'}):
            with self.assertRaises((RuntimeError, ValueError)):
                self.profile(login={'login_writes': [entry]})

    def test_ssh_socket_environment_and_symlink_targets(self):
        sock = self.home/'socket'
        sock.symlink_to(self.home/'actual')
        text = self.profile(env={'SSH_AUTH_SOCK': str(sock)})
        for path in (sock, self.home/'actual'):
            for op in ('file-read*', 'file-write*', 'network-outbound'):
                self.assertIn(f'(deny {op} (literal "{path}"))', text)
        (self.home/'.ssh').symlink_to(self.home/'ssh-target')
        self.assertIn('ssh\\-target/', self.profile())
        for raw in ('relative', '/a/../b'):
            with self.assertRaises(RuntimeError):
                self.profile(login={'sockets_allow': [raw]})
        text = self.profile(login={'sockets_allow': ['/x', '/x']})
        self.assertEqual(text.count('(allow network-outbound (literal "/x"))'), 1)

    def test_stage_creates_private_home_runtime_directories_links_and_optional_keychain(self):
        src = self.file('.grok/auth.json')
        for platform in ('darwin', 'linux'):
            run = self.tmp/platform
            run.mkdir()
            tmpl = {'home': 'private', 'keychain': 'login', 'env': {'RUNTIME': '{{tmp}}/cache', 'FLAG': 7},
                    'links': {'{{tmp}}/home/auth': '~/.grok/auth.json', '{{tmp}}/missing': '~/missing', '{{tmp}}/relative': 'relative'}}
            with patch.object(m.sys, 'platform', platform):
                env, links = m.stage(tmpl, run, self.home)
            self.assertEqual(env, {'RUNTIME': str(run/'cache'), 'FLAG': '7', 'HOME': str(run/'home')})
            self.assertTrue((run/'cache').is_dir())
            self.assertEqual((run/'home').stat().st_mode & 0o777, 0o700)
            self.assertEqual((run/'home/auth').resolve(), src)
            self.assertEqual(len(links), 2 if platform == 'darwin' else 1)
            m.unstage(links)
            m.unstage(links)
            self.assertFalse((run/'home/auth').exists())
        with patch.object(m.sys, 'platform', 'darwin'):
            _, links = m.stage({'keychain': 'login'}, self.tmp, self.home)
        self.assertEqual(links, [self.tmp/'login-keychain'])
        m.unstage(links)
        for tmpl in ({'env': {'BAD': '{{tmp}}/../escape'}}, {'links': {'{{tmp}}/../escape': '~/missing'}}):
            with self.assertRaises(ValueError):
                m.stage(tmpl, self.tmp, self.home)

    def test_stage_rejects_symlinked_login_leaf_parent_and_directory_child_and_skips_missing(self):
        src = self.file('real')
        link = self.home/'linked'
        link.symlink_to(src)
        for rel in ('linked', 'parent/auth'):
            if rel.startswith('parent'):
                (self.home/'parent').symlink_to(self.home)
            with self.assertRaisesRegex(ValueError, 'symlink'):
                m.stage({'links': {'{{tmp}}/auth': '~/'+rel}}, self.tmp, self.home)
        goose = self.home/'.config/goose/chatgpt_codex'
        goose.mkdir(parents=True)
        self.file('.config/goose/chatgpt_codex/token')
        tmpl = {'links': {'{{tmp}}/goose': '~/.config/goose/chatgpt_codex'}}
        snapshot = m.snapshot_login_files(tmpl, self.home)
        m.verify_login_files(tmpl, self.home, snapshot)
        (goose/'bad').symlink_to(src)
        with self.assertRaisesRegex(ValueError, 'symlink'):
            m.stage(tmpl, self.tmp, self.home)
        missing = {'links': {'{{tmp}}/auth': '~/absent'}}
        self.assertEqual(m.snapshot_login_files(missing, self.home)[self.home/'absent'][0], 'missing')
        self.assertEqual(m.stage(missing, self.tmp, self.home), ({}, []))
        m.verify_login_files(missing, self.home)

    def test_snapshot_and_verify_restore_replaced_login_and_remove_lock_symlinks(self):
        src = self.file('.grok/auth.json')
        tmpl = {'links': {'{{tmp}}/auth': '~/.grok/auth.json'}, 'login_writes': [
            {'literal': '~/.grok/auth.json'}, {'subpath': '~/.grok/locks'}, {'regex': '~/.grok/temp.*'}]}
        snapshot = m.snapshot_login_files(tmpl, self.home)
        src.unlink()
        with self.assertRaisesRegex(ValueError, 'removed'):
            m.verify_login_files(tmpl, self.home, snapshot)
        self.assertEqual(src.read_bytes(), b'login')
        src.unlink()
        src.symlink_to(self.home/'absent')
        with self.assertRaisesRegex(ValueError, 'symlink'):
            m.verify_login_files(tmpl, self.home, snapshot)
        self.assertEqual(src.read_bytes(), b'login')
        locks = self.home/'.grok/locks'
        locks.mkdir()
        (locks/'bad').symlink_to(src)
        (self.home/'.grok/temp1').symlink_to(src)
        with self.assertRaisesRegex(ValueError, 'symlink'):
            m.verify_login_files(tmpl, self.home, snapshot)
        self.assertFalse((locks/'bad').is_symlink())
        locks.rmdir()
        locks.symlink_to(self.home)
        with self.assertRaisesRegex(ValueError, 'lock path'):
            m.verify_login_files(tmpl, self.home, snapshot)

    def test_login_source_rejects_hardlinks_directories_and_foreign_ownership(self):
        src = self.file('auth')
        os.link(src, self.home/'hardlink')
        with self.assertRaisesRegex(ValueError, 'multiple hardlinks'):
            m._check_login_source(src, self.home)
        (self.home/'hardlink').unlink()
        with patch.object(m.os, 'getuid', return_value=-1):
            with self.assertRaisesRegex(ValueError, 'not owned'):
                m._check_login_source(src, self.home)
        with self.assertRaisesRegex(ValueError, 'not a regular file'):
            m._check_login_source(self.home, self.home)

    def test_private_login_copies_pin_permissions_stale_link_replacement_and_cleanup_statuses(self):
        src = self.file('.claude/.credentials.json', b'{"claudeAiOauth":{"token":"old"}}')
        dest = self.tmp/'.claude/.credentials.json'
        dest.parent.mkdir()
        dest.symlink_to(src)
        records = m.stage_claude_login(self.tmp, self.home)
        self.assertFalse(dest.is_symlink())
        self.assertEqual(dest.read_bytes(), src.read_bytes())
        self.assertEqual(dest.stat().st_mode & 0o777, 0o600)
        self.assertEqual(m.finish_copies(records), [{'source': '~/.claude/.credentials.json', 'status': 'unchanged'}])
        self.assertEqual(records, [])
        for source, original, changed, refresh, expected in [
            ('~/.codex/auth.json', b'{"tokens":{"account_id":"a"}}', b'{}', False, 'task_copy'),
            ('~/.claude/.credentials.json', src.read_bytes(), b'bad', True, 'invalid_json'),
            ('~/.claude/.credentials.json', src.read_bytes(), b'{}', True, 'oauth_missing'),
            ('~/.claude/.credentials.json', src.read_bytes(), b'{"claudeAiOauth":{"other":1}}', True, 'oauth_keys_changed'),
            ('~/.claude/.credentials.json', src.read_bytes(), b'{"claudeAiOauth":{"token":"new"}}', True, 'written_back')]:
            owner = self.file(source[2:], original)
            copy = self.tmp/'copy'
            records = m.stage_copies({'copies': {str(copy): source}}, self.tmp, self.home)
            copy.write_bytes(changed)
            self.assertEqual(m.finish_copies(records, claude_refresh=refresh)[0]['status'], expected)
            self.assertEqual(owner.read_bytes(), changed if expected == 'written_back' else original)
            self.assertFalse(copy.exists())
        records = m.stage_claude_login(self.tmp, self.home)
        dest.unlink()
        self.assertEqual(m.finish_copies(records)[0]['status'], 'invalid_copy')
        self.assertEqual(m.stage_copies({'copies': {str(self.tmp/'missing'): '~/.pi/agent/auth.json'}}, self.tmp, self.home), [])
        for copies in ({str(self.tmp/'bad'): '~/unsupported'}, {str(self.root/'escape'): '~/.claude/.credentials.json'}):
            with self.assertRaises(ValueError):
                m.stage_copies({'copies': copies}, self.tmp, self.home)

    def test_credential_identity_pins_known_accounts_and_unknown_json_shapes(self):
        for source, data, expected in [('~/.codex/auth.json', {'tokens': {'account_id': 'a'}}, 'a'),
                                       ('~/.pi/agent/auth.json', {'p': {'accountId': 'a'}}, {'p': 'a'}),
                                       ('~/.pi/agent/auth.json', {'p': {}}, None),
                                       ('~/.codex/auth.json', [], None), ('other', {}, None)]:
            self.assertEqual(m._credential_identity(source, json.dumps(data)), expected)

    def test_executable_payload_symlink_hops_and_narrow_local_bin(self):
        path = self.file('.local/share/tool/versions/v1/cli', b'#!/bin/sh\n')
        link = self.home/'cli'
        link.symlink_to(path)
        text = self.profile(binary=link)
        for target in (link, path):
            self.assertIn(f'(allow file-read* (literal "{target}"))', text)
        self.assertIn(f'(allow file-read* (subpath "{path.parent}"))', text)
        local = self.file('.local/bin/cli', b'\xff')
        self.assertNotIn(f'(allow file-read* (subpath "{local.parent}"))', self.profile(binary=local))
        local.chmod(0o755)
        env_script = self.file('script', b'#!/usr/bin/env -S local-interpreter\n')
        (local.parent/'local-interpreter').symlink_to(local)
        text = self.profile(binary=env_script, env={'PATH': str(local.parent)})
        self.assertIn(f'(allow file-read* (literal "{local}"))', text)
        loop = self.home/'loop'
        loop.symlink_to(loop)
        with self.assertRaisesRegex(RuntimeError, 'loop'):
            self.profile(binary=loop)

    def test_stage_git_pins_environment_allowlist_identity_ignore_copy_and_call_arguments(self):
        ignore = self.file('.config/git/ignore', b'*.secret\n')
        calls = []
        def run(args, **kwargs):
            calls.append((args, kwargs))
            value = {'user.name': 'Owner\n', 'user.email': 'owner@example.test\n'}
            return subprocess.CompletedProcess(args, 0 if args[-1] in value else 1, value.get(args[-1], ''), '')
        env = m.stage_git(self.tmp, run=run, env={'LANG': 'C', 'SECRET': 'hidden'}, home_dir=lambda: self.home)
        self.assertEqual(env, {'GIT_CONFIG_GLOBAL': str(self.tmp/'gitconfig'), 'GIT_CONFIG_NOSYSTEM': '1',
                               'GIT_CONFIG_COUNT': '2', 'GIT_CONFIG_KEY_0': 'maintenance.auto',
                               'GIT_CONFIG_VALUE_0': 'false', 'GIT_CONFIG_KEY_1': 'gc.auto',
                               'GIT_CONFIG_VALUE_1': '0'})
        self.assertEqual((self.tmp/'gitignore').read_bytes(), ignore.read_bytes())
        self.assertEqual((self.tmp/'gitconfig').stat().st_mode & 0o777, 0o600)
        self.assertEqual([args for args, _ in calls[:3]], [['git', 'config', '--global', '--get', 'user.name'],
                         ['git', 'config', '--global', '--get', 'user.email'], ['git', 'config', '--global', '--path', '--get', 'core.excludesFile']])
        self.assertEqual(calls[0][1]['env'], {'LANG': 'C', 'PATH': '/usr/bin:/bin', 'HOME': str(self.home), 'GIT_CONFIG_NOSYSTEM': '1'})
        self.assertEqual(calls[-1][0][-2:], ['core.excludesFile', str(self.tmp/'gitignore')])
        self.assertTrue(calls[-1][1]['check'])
        self.assertEqual(calls[0][1]['stdin'], subprocess.DEVNULL)

    def test_stage_git_defaults_resolve_patched_process_environment_and_home_at_call_time(self):
        fake = Mock(return_value=subprocess.CompletedProcess([], 1, '', ''))
        with patch.object(m.subprocess, 'run', fake), patch.object(m.Path, 'home', return_value=self.home), \
             patch.dict(os.environ, {'PATH': '/fixture/bin', 'LC_ALL': 'C'}, clear=True):
            m.stage_git(self.tmp)
        self.assertEqual(fake.call_count, 3)
        self.assertEqual(fake.call_args.kwargs['env'], {'PATH': '/fixture/bin', 'LC_ALL': 'C', 'HOME': str(self.home), 'GIT_CONFIG_NOSYSTEM': '1'})
        with self.assertRaisesRegex(ValueError, 'cannot read owner'):
            m.stage_git(self.tmp, self.home, env={}, run=Mock(return_value=subprocess.CompletedProcess([], 2, '', '')))

    def test_stage_git_pins_relative_explicit_and_xdg_ignore_sources_and_empty_environment(self):
        self.file('relative-ignore', b'explicit', home=self.tmp)
        for value, env, expected in [('relative-ignore', {}, b'explicit'), ('', {'XDG_CONFIG_HOME': str(self.home/'xdg')}, b'xdg')]:
            self.file('xdg/git/ignore', b'xdg')
            def run(args, **kwargs):
                return subprocess.CompletedProcess(args, 0 if args[-1] == 'core.excludesFile' and value else 1, value, '')
            m.stage_git(self.tmp, self.home, env=env, run=run)
            self.assertEqual((self.tmp/'gitignore').read_bytes(), expected)

    def test_terminal_receipts_pin_missing_success_error_and_malformed_final_records(self):
        self.assertIsNone(m.terminal_stop(None, ''))
        for kind, good, bad in [
            ('result_event', {'type': 'result', 'is_error': False, 'subtype': 'success'}, {'type': 'result'}),
            ('cursor_result', {'type': 'result', 'is_error': False, 'subtype': 'success'}, {'type': 'result'}),
            ('antigravity', {'event': 'result', 'result': {'status': 'SUCCESS'}}, {'event': 'result', 'result': {}}),
            ('gemini_result', {'type': 'result', 'status': 'success'}, {'type': 'result'})]:
            self.assertIsNone(m.terminal_stop(kind, 'bad\n[]\n'+json.dumps(good)))
            self.assertEqual(m.terminal_stop(kind, json.dumps(bad)), 'native_terminal_error')
            self.assertEqual(m.terminal_stop(kind, ''), 'native_terminal_missing')
        assistant = json.dumps({'type': 'assistant', 'session_id': 's', 'message': {'role': 'assistant', 'content': [{'type': 'text', 'text': 'done'}]}})
        self.assertIsNone(m.terminal_stop('cursor_result', assistant))
        self.assertEqual(m.terminal_stop('cursor_result', assistant+'\nbad'), 'native_terminal_missing')
        with self.assertRaises(ValueError):
            m.terminal_stop('unknown', '')
        with patch('_harness_usage.terminal_stop', return_value='delegated'):
            self.assertEqual(m.terminal_stop('pi_json', ''), 'delegated')
