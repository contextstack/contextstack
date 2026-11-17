"""Lane lifecycle and landing contracts against isolated, disposable Git repositories."""
import json
import os
import signal
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch, Mock

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE.parent / '_lib'), str(HERE.parents[2] / 'ops/bin')]
from hermetic_env import hermetic_env
from scratch import scratch_dir
import _lanes as lanes


class LanesTests(unittest.TestCase):
    def setUp(self):
        self.temp = scratch_dir(prefix='lanes-units-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()  # macOS /var is /private/var; repo_config resolves paths
        env = {k: v for k, v in hermetic_env().items() if not k.startswith('GIT_')}
        env.update(HOME=str(self.root), GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1',
                   CS_REPOS_FILE=str(self.root / 'repos.json'), PYTHONDONTWRITEBYTECODE='1')
        self.env = patch.dict(os.environ, env, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)
        self.main = self.root / 'main'
        self.main.mkdir()
        self.g('init', '-q', '-b', 'main')
        self.g('config', 'user.name', 'Repo Owner')
        self.g('config', 'user.email', 'owner@example.test')
        (self.main / 'file').write_text('seed\n')
        self.commit(self.main, 'seed')
        self.cfg = {**lanes.DEFAULTS, 'name': 'repo', 'path': self.main}
        self.configure()

    def g(self, *args, cwd=None):
        return subprocess.run(['git', '-C', str(cwd or self.main), '-c', 'user.name=Repo Owner',
                               '-c', 'user.email=owner@example.test', '-c', 'core.hooksPath=/dev/null', *args],
                              capture_output=True, text=True, check=True).stdout.strip()

    def configure(self, **kw):
        self.cfg.update(kw)
        Path(os.environ['CS_REPOS_FILE']).write_text(json.dumps({'repo': {
            k: str(v) if isinstance(v, Path) else v for k, v in self.cfg.items() if k != 'name'}}))

    def commit(self, where, message):
        self.g('add', '-A', cwd=where)
        self.g('commit', '-q', '-m', message, cwd=where)
        return self.g('rev-parse', 'HEAD', cwd=where)

    def lane(self, name='safe', content='lane\n', message='lane subject\n\nDetailed body'):
        info = lanes.ensure_lane(self.cfg, name)
        ws = Path(info['path'])
        (ws / 'file').write_text(content)
        lanes.commit_lane(ws, message, main=self.main, lane=name)
        return ws

    def assert_clean(self, before):
        self.assertEqual(self.g('rev-parse', 'HEAD'), before)
        self.assertEqual(self.g('status', '--porcelain'), '')

    def test_lane_slugs_pin_allowed_characters_length_and_main_collision(self):
        for name in ('a', 'a.b_c-9', 'a' * 63):
            lanes.check_lane(self.cfg, name)
        for name in ('', None, 'A', '-a', 'a/b', 'a..b', 'a' * 64, 'main'):
            with self.subTest(name=name), self.assertRaises(lanes.LaneError):
                lanes.check_lane(self.cfg, name)
        with self.assertRaisesRegex(lanes.LaneError, 'main checkout'):
            lanes.check_lane({**self.cfg, 'branch': 'release'}, 'main')
        with self.assertRaisesRegex(lanes.LaneError, 'invalid lane ref'):
            lanes._commit_lane_index(self.main, '../bad', self.main, 'bad')

    def test_repo_config_pins_short_form_defaults_and_invalid_entries(self):
        repos = Path(os.environ['CS_REPOS_FILE'])
        repos.write_text(json.dumps({'repo': str(self.main)}))
        self.assertEqual(lanes.repo_config('repo'), self.cfg)
        for value, reason in (({}, 'not in'), ({'repo': {}}, 'no path'),
                              ({'repo': {'path': str(self.main), 'unknown': 1}}, 'unknown keys'),
                              ({'repo': {'path': str(self.main), 'land': 'bad'}}, 'land must'),
                              ({'repo': {'path': str(self.main), 'hook_gated': 'bad'}}, 'hook_gated'),
                              ({'repo': {'path': str(self.root / 'absent')}}, 'not a git checkout'),
                              ({'repo': {'path': str(self.main), 'tracked_secret_allowlist': ['../bad']}},
                               'exact repo-relative')):
            repos.write_text(json.dumps(value))
            with self.subTest(value=value), self.assertRaisesRegex(lanes.LaneError, reason):
                lanes.repo_config('repo')
        repos.write_text('{')
        with self.assertRaisesRegex(lanes.LaneError, 'repos file'):
            lanes.repo_config('repo')
        repos.unlink()
        with self.assertRaisesRegex(lanes.LaneError, 'no repos file'):
            lanes.repo_config('repo')
        self.assertEqual(lanes.land('missing', 'safe')[0], 2)

    def test_worktree_creation_reuse_dirty_state_and_existing_branch(self):
        info = lanes.ensure_lane(self.cfg, 'safe')
        self.assertTrue(info['created'])
        self.assertFalse(info['dirty_at_start'])
        ws = Path(info['path'])
        (ws / 'untracked').write_text('pending')
        reused = lanes.ensure_lane(self.cfg, 'safe')
        self.assertFalse(reused['created'])
        self.assertTrue(reused['dirty_at_start'])
        self.g('worktree', 'remove', '--force', str(ws))
        self.assertTrue(lanes.ensure_lane(self.cfg, 'safe')['created'])
        self.g('worktree', 'remove', '--force', str(ws))
        ws.mkdir()
        with self.assertRaisesRegex(lanes.LaneError, 'not the lane'):
            lanes.ensure_lane(self.cfg, 'safe')
        ws.rmdir()
        self.g('worktree', 'add', '-q', '--detach', str(ws))
        with self.assertRaisesRegex(lanes.LaneError, 'detached HEAD'):
            lanes.ensure_lane(self.cfg, 'safe')

    def test_commit_lane_pins_noop_excluded_lab_files_message_and_fallback_identity(self):
        ws = Path(lanes.ensure_lane(self.cfg, 'safe')['path'])
        self.assertIsNone(lanes.commit_lane(ws, 'noop', main=self.main, lane='safe'))
        self.g('config', '--unset', 'user.email')
        self.g('config', '--unset', 'user.name')
        (ws / '.lab-private').write_text('excluded')
        (ws / 'new').write_text('included')
        oid = lanes.commit_lane(ws, 'subject\n\nbody', main=self.main, lane='safe')
        self.assertEqual(self.g('show', '-s', '--format=%B', oid), 'subject\n\nbody')
        self.assertEqual(self.g('show', '-s', '--format=%an <%ae>', oid),
                         'contextstack lane <lane@example.test>')
        self.assertNotIn('.lab-private', self.g('ls-tree', '-r', '--name-only', oid))

    def test_green_gate_commits_with_owner_identity_full_message_and_cleanup(self):
        ws = self.lane()
        self.g('commit', '--amend', '--no-edit', '--author=Lane Agent <agent@example.test>', cwd=ws)
        self.configure(gate='printf green')
        code, result = lanes.land('repo', 'safe')
        self.assertEqual((code, result['status']), (0, 'landed'))
        self.assertEqual(self.g('show', '-s', '--format=%B'), 'lane subject\n\nDetailed body')
        self.assertEqual(self.g('show', '-s', '--format=%an <%ae>|%cn <%ce>'),
                         'Repo Owner <owner@example.test>|Repo Owner <owner@example.test>')
        self.assertEqual(result['removed']['branch'], 'lane/safe')
        self.assertFalse((self.root / 'safe').exists())

    def test_red_gate_restores_main_and_keeps_lane_for_retry(self):
        self.lane()
        before = self.g('rev-parse', 'HEAD')
        self.configure(gate='echo FAIL deliberate; exit 7')
        code, result = lanes.land('repo', 'safe')
        self.assertEqual((code, result['status'], result['stage']), (1, 'red', 'gate'))
        self.assertEqual(result['failed'], ['FAIL deliberate'])
        self.assertTrue(result['main_unchanged'])
        self.assert_clean(before)
        self.assertTrue((self.root / 'safe').is_dir())

    def test_conflicting_lane_is_refused_and_main_is_restored(self):
        self.lane()
        (self.main / 'file').write_text('main diverged\n')
        before = self.commit(self.main, 'main edit')
        code, result = lanes.land('repo', 'safe')
        self.assertEqual((code, result['status'], result['files']), (1, 'conflict', ['file']))
        self.assert_clean(before)

    def test_policy_uses_lane_three_dot_and_ignores_main_only_protected_change(self):
        self.lane()
        (self.main / 'AGENTS.md').write_text('owner instructions')
        self.commit(self.main, 'owner policy')
        names = lanes.lane_changed_names(lanes.out, self.main, 'lane/safe')
        self.assertEqual(names, ['file', ''])
        self.assertEqual(lanes.land('repo', 'safe')[0], 0)

    def test_policy_blocks_lane_protected_file_and_symlink_target(self):
        for symlink in (False, True):
            name = 'link' if symlink else 'config'
            ws = self.lane(name)
            if symlink:
                (ws / 'alias').symlink_to('AGENTS.md')
            else:
                (ws / 'AGENTS.md').write_text('agent policy')
            lanes.commit_lane(ws, 'protected', main=self.main, lane=name)
            before = self.g('rev-parse', 'HEAD')
            code, result = lanes.land('repo', name)
            self.assertEqual(code, 2)
            self.assertIn('owner to land by hand', result['reason'])
            self.assert_clean(before)
        self.assertEqual(lanes.land('repo', 'config', allow_agent_config=True)[0], 0)

    def test_leftover_squash_is_detected_reset_and_landed_on_retry(self):
        self.lane()
        before = self.g('rev-parse', 'HEAD')
        self.g('merge', '--squash', 'lane/safe')
        self.assertTrue(lanes._recover_leftover_squash(self.main))
        self.assert_clean(before)
        self.assertFalse(lanes._recover_leftover_squash(self.main))
        self.g('merge', '--squash', 'lane/safe')
        self.assertEqual(lanes.land('repo', 'safe')[0], 0)

    def test_dirty_main_is_refused_without_discarding_owner_work(self):
        self.lane()
        (self.main / 'file').write_text('owner pending\n')
        code, result = lanes.land('repo', 'safe')
        self.assertEqual(code, 2)
        self.assertIn('main has uncommitted', result['reason'])
        self.assertEqual((self.main / 'file').read_text(), 'owner pending\n')

    def test_land_refuses_wrong_branch_missing_lane_untracked_lane_and_red_message(self):
        self.assertIn('no branch', lanes.land('repo', 'safe')[1]['reason'])
        ws = self.lane()
        (ws / 'new').write_text('pending')
        self.assertIn('untracked', lanes.land('repo', 'safe')[1]['reason'])
        (ws / 'new').unlink()
        self.assertIn('check-red:', lanes.land('repo', 'safe', 'check-red: no')[1]['reason'])
        self.g('checkout', '-q', '-b', 'other')
        self.assertIn('not main', lanes.land('repo', 'safe')[1]['reason'])

    def test_tracked_lane_edits_are_preserved_and_multiple_subjects_form_message(self):
        ws = self.lane(message='first')
        (ws / 'file').write_text('second\n')
        code, result = lanes.land('repo', 'safe')
        self.assertEqual(code, 0)
        self.assertIn('lane_commit', result)
        self.assertEqual(self.g('show', '-s', '--format=%B'),
                         'Land lane/safe\n\n- first\n- land lane/safe: tracked changes before landing')

    def test_landgit_environment_pins_identity_hooks_and_inert_attributes(self):
        self.configure(hooks_path='owner-hooks')
        landing = lanes.LandGit(self.cfg)
        with patch.dict(os.environ, {'GIT_DIR': '/untrusted'}):
            env = landing.environment({'CS_LAND_COMMIT': '1'})
        self.assertNotIn('GIT_DIR', env)
        self.assertEqual(env['GIT_CONFIG_GLOBAL'], '/dev/null')
        self.assertEqual(env['GIT_ATTR_SOURCE'], landing.attributes)
        self.assertEqual(landing.output(self.main, 'config', 'core.hooksPath'), str(self.main / 'owner-hooks'))
        self.assertEqual(landing.output(self.main, 'config', 'user.email'), 'owner@example.test')
        with self.assertRaises(lanes.LaneError):
            landing.output(self.main, 'rev-parse', '--verify', 'absent')

    def test_lane_admin_hardening_refuses_rewired_gitfile_admin_head_and_branch(self):
        ws = self.lane()
        admin = lanes.expected_lane_gitdir(self.main, 'safe')
        for path, content in ((ws / '.git', 'gitdir: /wrong\n'), (admin / 'commondir', '/wrong\n'),
                              (admin / 'gitdir', '/wrong\n'), (admin / 'HEAD', 'ref: refs/heads/main\n'),
                              (self.main / '.git/refs/heads/lane/safe', 'ref: refs/heads/main\n')):
            original = path.read_bytes()
            try:
                path.write_text(content)
                with self.subTest(path=path.name), self.assertRaises(lanes.LaneError):
                    lanes.lane_out(self.main, 'safe', ws, 'status')
            finally:
                path.write_bytes(original)
        self.assertEqual(len(lanes.sandbox_read_roots(self.main, 'safe', ws)), 3)
        from _estate_io import native_caller
        from sandbox_profile import render_profile
        caller, roots = native_caller({}, self.root, self.root / 'run', ws, self.root / 'tmp', dict(main=self.main, lane='safe'))
        rules = render_profile(self.root, ws, self.root / 'tmp', caller=caller, read_roots=roots)
        self.assertIn('config.worktree', rules)
        self.assertIn('deny file-write*', rules)

    def test_packed_lane_prepares_only_parent_directories_before_agent_and_host_commits(self):
        from _estate_io import native_caller
        for entry in ('agent', 'host'):
            with self.subTest(entry=entry):
                ws = self.lane()
                tip = self.g('rev-parse', 'HEAD', cwd=ws)
                self.g('pack-refs', '--all')
                common = lanes.git_common_dir(self.main)
                packed = (common / 'packed-refs').read_bytes()
                ref = common / 'refs/heads/lane/safe'
                if ref.parent.exists():
                    ref.parent.rmdir()
                log = common / 'logs/refs/heads/lane/safe'
                log.unlink()
                log.parent.rmdir()
                if entry == 'agent':
                    native_caller({}, self.root, self.root / 'run', ws, self.root / 'tmp',
                                  dict(main=self.main, lane='safe'))
                else:
                    lanes.lane_out(self.main, 'safe', ws, 'status', '--porcelain')
                self.assertTrue(ref.parent.is_dir())
                self.assertTrue(log.parent.is_dir())
                self.assertFalse(ref.exists())
                self.assertEqual(self.g('rev-parse', 'HEAD', cwd=ws), tip)
                self.g('commit', '--allow-empty', '-qm', 'after packing', cwd=ws)
                self.assertTrue(ref.is_file())
                self.assertTrue(log.is_file())
                self.assertEqual((common / 'packed-refs').read_bytes(), packed)

    def test_packed_nested_ref_prepares_all_ref_and_reflog_parents(self):
        ws = self.root / 'nested'
        self.g('worktree', 'add', '-qb', 'lane/group/worker', str(ws))
        admin = Path(self.g('rev-parse', '--absolute-git-dir', cwd=ws))
        self.g('pack-refs', '--all')
        common = lanes.git_common_dir(self.main)
        log = common / 'logs/refs/heads/lane/group/worker'
        log.unlink()
        log.parent.rmdir()
        log.parent.parent.rmdir()
        lanes.prepare_lane_refs(self.main, 'group/worker', admin)
        self.assertTrue((common / 'refs/heads/lane/group').is_dir())
        self.assertTrue(log.parent.is_dir())
        self.g('commit', '--allow-empty', '-qm', 'nested packed lane', cwd=ws)
        self.assertTrue((common / 'refs/heads/lane/group/worker').is_file())
        self.assertTrue(log.is_file())

    def test_workspace_changed_paths_pin_committed_staged_unstaged_and_untracked_union(self):
        base = self.g('rev-parse', 'HEAD')
        ws = self.lane()
        (ws / 'staged').write_text('stage')
        self.g('add', 'staged', cwd=ws)
        (ws / 'file').write_text('unstaged')
        (ws / 'new').write_text('untracked')
        for ctx in (None, {'main': self.main, 'lane': 'safe'}):
            self.assertEqual(set(lanes.workspace_changed_paths(ws, base, lane_ctx=ctx)),
                             {'file', 'staged', 'new'})
        with self.assertRaises(lanes.LaneError):
            lanes.workspace_out(self.main, 'rev-parse', '--verify', 'absent')

    def test_gate_timeout_and_interrupt_restore_main_and_signal_handlers(self):
        self.lane()
        self.configure(gate='fake gate')
        before = self.g('rev-parse', 'HEAD')
        for interrupt in (False, True):
            proc = Mock()
            proc.poll.return_value = None
            proc.returncode = -9
            def communicate(**kwargs):
                if not kwargs:
                    return ('timeout tail', None)
                if interrupt:
                    signal.getsignal(signal.SIGINT)(signal.SIGINT, None)
                    return ('interrupted', None)
                raise subprocess.TimeoutExpired('gate', 1, output='timeout ')
            proc.communicate.side_effect = communicate
            original = signal.getsignal(signal.SIGINT)
            real_popen = subprocess.Popen
            def popen(command, *args, **kw):
                return proc if command[0] == '/bin/sh' else real_popen(command, *args, **kw)
            with patch.object(lanes.subprocess, 'Popen', side_effect=popen):
                code, result = lanes.land('repo', 'safe')
            self.assertEqual(code, 2 if interrupt else 1)
            self.assertEqual(signal.getsignal(signal.SIGINT), original)
            proc.kill.assert_called()
            self.assert_clean(before)

    def test_secret_history_scan_cache_and_allowlist_pin_names_without_contents(self):
        # Decoy names only: construct the tree without writing a secret-named file.
        blob = self.g('rev-parse', 'HEAD:file')
        self.g('update-index', '--add', '--cacheinfo', '100644', blob, 'nested/decoy.env')
        self.g('commit', '-q', '-m', 'decoy name')
        found = lanes.scan_reachable_secrets(self.main)
        self.assertEqual(set(found), {'nested/decoy.env'})
        with self.assertRaisesRegex(lanes.LaneError, 'purge them from history'):
            lanes.refuse_tracked_secrets(self.cfg)
        lanes.refuse_tracked_secrets({**self.cfg, 'tracked_secret_allowlist': ['nested/decoy.env']})
        tips = lanes._ref_tips(self.main)
        self.assertEqual(lanes._read_secret_cache(self.main, tips), found)
        lanes._SECRET_SCAN_CACHE.pop(str(self.main), None)
        self.assertEqual(lanes.tracked_secret_index(self.main), found)
        cache = lanes._secret_cache_file(self.main)
        for data in ('bad json', '[]', json.dumps({'v': 1, 'tips': [list(t) for t in tips], 'paths': []}),
                     json.dumps({'v': 1, 'tips': [list(t) for t in tips], 'paths': {'x': [1]}})):
            cache.write_text(data)
            self.assertIsNone(lanes._read_secret_cache(self.main, tips))
        self.assertEqual(lanes._paths_for(b':100644 100644 a b R100', [b'old', b'new']), [b'new'])
        self.assertEqual(lanes._paths_for(b':100644 000000 a b D', [b'old']), [])
        self.assertEqual(lanes._parse_secret_log(b'COMMIT\0malformed\0COMMIT\0bad\n:meta\0'), {})
        self.assertFalse(lanes._secret_path('nested/DECOY.ENV.sample'))

    def test_dependency_seed_copies_independent_files_and_skips_existing_target(self):
        source = self.main / 'clients/web/vendor'
        source.mkdir(parents=True)
        (source / 'dependency').write_text('owner copy')
        target = self.root / 'dependency-lane'
        lanes.clone_lane_dependencies(self.main, target)
        copied = target / 'clients/web/vendor/dependency'
        self.assertEqual(copied.read_text(), 'owner copy')
        copied.write_text('lane copy')
        self.assertEqual((source / 'dependency').read_text(), 'owner copy')
        lanes.clone_lane_dependencies(self.main, target)
        self.assertEqual(copied.read_text(), 'lane copy')

    def test_merge_landing_and_explicit_message_pin_two_parents(self):
        self.lane()
        self.configure(land='merge')
        self.assertEqual(lanes.land('repo', 'safe', 'Owner message')[0], 0)
        self.assertEqual(len(self.g('show', '-s', '--format=%P').split()), 2)
        self.assertEqual(self.g('show', '-s', '--format=%B'), 'Owner message')

    def test_commit_hook_red_restores_main_and_inherited_red_subject_requires_override(self):
        self.lane(message='check-red: deliberate')
        before = self.g('rev-parse', 'HEAD')
        self.assertIn('land it with -m', lanes.land('repo', 'safe')[1]['reason'])
        self.assert_clean(before)
        hooks = self.root / 'owner-hooks'
        hooks.mkdir()
        hook = hooks / 'commit-msg'
        hook.write_text('#!/bin/sh\necho "error: owner hook refused" >&2\nexit 1\n')
        hook.chmod(0o755)
        self.configure(hooks_path=str(hooks))
        code, result = lanes.land('repo', 'safe', 'override')
        self.assertEqual((code, result['status'], result['stage']), (1, 'red', 'commit'))
        self.assert_clean(before)

    def test_leftover_recovery_preserves_merge_and_unrelated_staged_owner_work(self):
        (self.main / 'owner').write_text('pending')
        self.g('add', 'owner')
        self.assertFalse(lanes._recover_leftover_squash(self.main))
        self.assertIn('reset --hard HEAD', lanes._leftover_squash_recovery(self.main))
        self.assertTrue((self.main / 'owner').exists())
        self.g('reset', '--hard', 'HEAD')
        lock = self.main / '.git/cs-land.lock'
        for contents in ('', 'invalid', str(os.getpid())):
            lock.write_text(contents)
            self.assertEqual(lanes._land_lock_holder(self.main), os.getpid() if contents.isdigit() else None)
        (self.main / '.git/MERGE_HEAD').write_text(self.g('rev-parse', 'HEAD') + '\n')
        self.assertFalse(lanes._recover_leftover_squash(self.main))

    def test_second_landing_cannot_take_live_kernel_lock(self):
        import fcntl
        lock = self.main / '.git/cs-land.lock'
        with lock.open('w') as handle:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            code, result = lanes.land('repo', 'safe')
            self.assertEqual(code, 2)
            self.assertIn('another landing', result['reason'])
        self.assertIsNone(lanes._LAND_GIT.get())

    def test_identical_lane_changes_are_refused_without_a_duplicate_commit(self):
        self.lane()
        (self.main / 'file').write_text('lane\n')
        before = self.commit(self.main, 'owner independently applied')
        code, result = lanes.land('repo', 'safe')
        self.assertEqual(code, 2)
        self.assertIn('already has', result['reason'])
        self.assert_clean(before)

    def test_hook_gated_paths_skip_explicit_gate_and_keep_owner_hook(self):
        self.lane()
        hooks = self.root / 'hooks'
        hooks.mkdir()
        hook = hooks / 'commit-msg'
        hook.write_text('#!/bin/sh\ntest "$CS_LAND_COMMIT" = 1\n')
        hook.chmod(0o755)
        self.configure(hooks_path=str(hooks), hook_gated=['file'], gate='exit 99')
        code, result = lanes.land('repo', 'safe')
        self.assertEqual(code, 0)
        self.assertIsNone(result['gate']['command'])
