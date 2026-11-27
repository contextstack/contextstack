"""Hosted session contracts, with isolated roots and in-memory CLI/Director peers."""
import io
import json
import os
import socket
import subprocess
import sys
import threading
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import Mock, patch

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE.parent / '_lib'), str(HERE.parents[2] / 'ops/bin'),
                str(HERE.parents[2] / 'scripts')]
from hermetic_env import hermetic_env
from scratch import scratch_dir, short_tmp
import _lead_host as m
import _lead_wake as wake
import cs_lead


class FakeSession:
    def __init__(self, reply='answer'):
        self.alive = True
        self.session_id = 'cli-session'
        self.reply = reply
        self.sent = []
        self.stopped = False

    def start(self):
        return self

    def send(self, text):
        self.sent.append(text)

    def collect_turn(self, timeout):
        if isinstance(self.reply, Exception):
            raise self.reply
        return self.reply

    def run_turn(self, text, timeout):
        self.send(text)
        return self.collect_turn(timeout)

    def stop(self):
        self.stopped = True
        self.alive = False


class HostTests(unittest.TestCase):
    def setUp(self):
        self.scratch = scratch_dir(prefix='lead-host-units-', dir=short_tmp())
        self.root = Path(self.scratch.__enter__()).resolve()
        self.addCleanup(self.scratch.cleanup)
        self.state = self.root / 'state'
        self.config = self.root / 'config'
        self.config.mkdir()
        self.env = patch.dict(os.environ, {**hermetic_env(), 'HOME': str(self.root / 'home')}, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)
        self.sandbox = patch.object(m, 'require_host_sandbox', side_effect=lambda value: value)
        self.sandbox.start()
        self.addCleanup(self.sandbox.stop)
        # Match the lifecycle regression's fixture namespace: the outer sandbox
        # reserves lead-sessions even under temporary state roots.
        sessions = patch.object(m, 'sessions_root', side_effect=lambda state: state / 'fixture-sessions')
        sessions.start()
        self.addCleanup(sessions.stop)
        self.host = m.ConversationHost('conversation', self.state, self.config)
        self.sf = m.state_file(self.state, 'conversation')
        m.save_state(self.sf, {'conversation_id': 'conversation'})
        self.cred = m.session_credential_path(self.state, 'conversation')
        self.cred.parent.mkdir(parents=True)
        self.cred.write_text('old-token\n')
        self.ws = self.state / 'jobs/job-1'
        self.ws.mkdir(parents=True)
        self.write('job.json', {'job_id': 'job-1', 'task': 'tasks/turn'})
        self.client = Mock()
        self.client.request.return_value = (200, {'turns': [None, {'job_id': 'job-1'}]})
        self.client.record_wake_reply.return_value = True
        self.director = patch.object(m, 'director_client_for_host', return_value=self.client)
        self.director.start()
        self.addCleanup(self.director.stop)

    def write(self, rel, value, root=None):
        p = (root or self.ws) / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(value) if isinstance(value, dict) else value)
        return p

    def exit_reason(self, **kw):
        return m.serve_should_exit(self.state, 'conversation', last_turn_at=kw.pop('last', 0),
                                   started_at=10, host=self.host, **kw)

    def test_serve_exit_pins_removed_state_sessions_conversation_and_dead_parent(self):
        with patch.object(m, '_watch_parent_pid', return_value=123), patch.object(m, '_pid_alive', return_value=False):
            self.assertEqual(self.exit_reason(), 'parent process gone')
        self.sf.unlink()
        self.assertEqual(self.exit_reason(), 'conversation state removed')
        import shutil
        shutil.rmtree(m.sessions_root(self.state))
        self.assertEqual(self.exit_reason(), 'lead-sessions removed')
        shutil.rmtree(self.state)
        self.assertEqual(self.exit_reason(), 'state root removed')

    def test_idle_exit_uses_last_completed_turn_or_start_and_can_be_disabled(self):
        with patch.object(m.time, 'monotonic', return_value=100), patch.dict(os.environ, {'LEAD_HOST_IDLE_EXIT_S': '90'}):
            self.assertEqual(self.exit_reason(), 'idle timeout')
            self.assertIsNone(self.exit_reason(last=11))
            self.assertEqual(self.exit_reason(last=10), 'idle timeout')
        with patch.dict(os.environ, {'LEAD_HOST_IDLE_EXIT_S': '0'}):
            self.assertIsNone(self.exit_reason())

    def test_idle_exit_waits_for_running_turn_and_unended_watched_job(self):
        with patch.object(m.time, 'monotonic', return_value=100), patch.object(m, '_idle_exit_seconds', return_value=1):
            self.host._turn_depth = 1
            self.assertIsNone(self.exit_reason())
            self.host._turn_depth = 0
            coordinator = wake.WakeCoordinator(self.host, director_url='http://director.invalid',
                                               credential_file=self.cred, client_factory=lambda *args: self.client)
            self.host._wake = coordinator
            coordinator._watch['job-1'] = 'lane'
            for state in ('', 'queued', 'running'):
                coordinator._known_state['job-1'] = state
                self.assertIsNone(self.exit_reason())
            coordinator._known_state['job-1'] = 'done'
            self.assertEqual(self.exit_reason(), 'idle timeout')

    def test_workspace_refuses_outside_nested_symlink_and_unresolved_paths(self):
        link = self.state / 'jobs/link'
        link.symlink_to(self.ws, target_is_directory=True)
        for p in (self.root, self.ws / 'nested', link, self.ws / '..' / 'job-1'):
            with self.subTest(path=p), self.assertRaisesRegex(SystemExit, 'conversation job bundle'):
                self.host.validate_workspace(p)
        self.client.request.assert_not_called()

    def test_workspace_refuses_missing_wrong_job_or_task_and_unowned_director_turn(self):
        for record in ({}, {'job_id': 'other', 'task': 'tasks/turn'}, {'job_id': 'job-1', 'task': 'other'}):
            self.write('job.json', record)
            with self.assertRaisesRegex(SystemExit, 'turn job bundle'):
                self.host.validate_workspace(self.ws)
        self.write('job.json', {'job_id': 'job-1', 'task': 'tasks/turn'})
        for response in ((403, {}), (200, []), (200, {'turns': [{'job_id': 'other'}]})):
            self.client.request.return_value = response
            with self.assertRaisesRegex(SystemExit, 'not a turn of this conversation'):
                self.host.validate_workspace(self.ws)
        self.client.request.return_value = (200, {'turns': [{'job_id': 'job-1'}]})
        self.assertEqual(self.host.validate_workspace(self.ws), self.ws)

    def test_credential_path_prefers_session_migrates_legacy_and_refuses_missing(self):
        self.assertEqual(self.host.wake_credential_path(), self.cred)
        self.cred.unlink()
        self.write('lead-credential', 'legacy', self.config)
        self.write('hosted-lead.json', {'conversation_id': 'conversation', 'credential_file': 'config/lead-credential',
                                      'director': 'http://director.invalid'}, self.config)
        self.assertEqual(m.credential_path(self.config, 'conversation', self.state).read_text(), 'legacy')
        self.assertEqual(self.cred.stat().st_mode & 0o777, 0o600)
        with self.assertRaisesRegex(SystemExit, 'missing for conversation'):
            m.credential_path(self.config, 'other', self.state)

    def test_credential_resolver_refuses_absolute_traversal_missing_and_escaping_symlinks(self):
        outside = self.write('outside', 'secret', self.root)
        (self.config / 'link').symlink_to(outside)
        for rel in ('', str(outside), '../outside', 'missing', 'link'):
            with self.subTest(rel=rel), self.assertRaises(SystemExit):
                m.resolve_credential_under_config(self.config, rel)

    def test_401_refetch_retries_with_new_token_after_atomic_write_under_exclusive_lock(self):
        self.director.stop()
        self.write('meta.json', {'director': 'http://director.invalid'}, self.cred.parent)
        attempts = []
        response = Mock(status=200, headers={'Content-Type': 'application/json'})
        response.__enter__ = Mock(return_value=response)
        response.__exit__ = Mock(return_value=False)
        response.read.return_value = b'{"turns": []}'
        def open_request(req, **kwargs):
            attempts.append(req.get_header('Authorization'))
            if len(attempts) == 1:
                raise urllib.error.HTTPError(req.full_url, 401, 'expired', {}, io.BytesIO(b'{}'))
            return response
        real_write = cs_lead.write_atomic
        locked = []
        flock = m.fcntl.flock
        def lock(fd, operation):
            locked.append(operation)
            return flock(fd, operation)
        def atomic(path, data):
            self.assertEqual(locked, [m.fcntl.LOCK_EX])
            with (path.parent / m.CRED_LOCK_NAME).open('a') as contender:
                with self.assertRaises(BlockingIOError):
                    flock(contender.fileno(), m.fcntl.LOCK_EX | m.fcntl.LOCK_NB)
            self.assertEqual(path.read_text(), 'old-token\n')
            real_write(path, data)
        with patch.object(m, '_mint_lead_credential_from_node', return_value=' new-token '), \
             patch.object(m.fcntl, 'flock', side_effect=lock), \
             patch.object(cs_lead, 'write_atomic', side_effect=atomic), \
             patch.object(m.os, 'replace', wraps=os.replace) as replace, \
             patch.object(wake.urllib.request, 'urlopen', side_effect=open_request):
            client = m.director_client_for_host(self.host)
            self.assertEqual(client.request('GET', 'thread'), (200, {'turns': []}))
        source, destination = replace.call_args.args
        self.assertEqual(destination, self.cred)
        self.assertEqual(Path(source).parent, self.cred.parent)
        self.assertNotEqual(Path(source), destination)
        self.assertFalse(Path(source).exists())
        self.assertEqual(attempts, ['Bearer old-token', 'Bearer new-token'])
        self.assertEqual(self.cred.read_text(), 'new-token\n')
        self.assertEqual((self.cred.parent / m.CRED_LOCK_NAME).stat().st_mode & 0o777, 0o600)
        with patch.object(m, '_mint_lead_credential_from_node', return_value=None):
            self.assertFalse(m.refetch_lead_credential_from_director('conversation', self.config, self.state))
        self.assertEqual(self.cred.read_text(), 'new-token\n')

    def test_nudge_refuses_blank_busy_absent_and_dead_sessions_without_sending(self):
        sess = FakeSession()
        self.host._session = sess
        self.assertFalse(self.host.deliver_nudge(' '))
        self.host._turn_depth = 1
        self.assertFalse(self.host.deliver_nudge('wake'))
        self.host._turn_depth = 0
        sess.alive = False
        self.assertFalse(self.host.deliver_nudge('wake'))
        self.host._session = None
        self.assertFalse(self.host.deliver_nudge('wake'))
        self.assertEqual(sess.sent, [])

    def test_nudge_records_redacted_prompt_reply_and_log_on_director(self):
        self.host._session = FakeSession('answer sk-ant-private')
        with patch.object(m.time, 'monotonic', return_value=42):
            self.assertTrue(self.host.deliver_nudge('wake sk-ant-private\nnext'))
        self.client.record_wake_reply.assert_called_once_with('conversation', 'wake <redacted>\nnext', 'answer <redacted>')
        log = (self.sf.parent / 'conversation-serve.log').read_text()
        self.assertEqual(log, 'NUDGE: wake <redacted>\nNUDGE: next\n')
        self.assertEqual(self.host._turn_depth, 0)
        self.assertEqual(self.host._last_turn_at, 42)

    def test_nudge_timeout_stops_session_and_is_delivered_once_even_without_reply(self):
        self.host._session = FakeSession(TimeoutError('sk-ant-private'))
        self.assertTrue(self.host.deliver_nudge('wake'))
        self.assertTrue(self.host._session.stopped)
        self.assertFalse(self.host.deliver_nudge('wake'))
        self.assertEqual(self.host._turn_depth, 0)
        self.client.record_wake_reply.assert_not_called()
        log = (self.sf.parent / 'conversation-serve.log').read_text()
        self.assertIn('WAKE-REPLY-FAILED: TimeoutError', log)
        self.assertNotIn('sk-ant-private', log)

    def test_nudge_uses_wake_coordinator_and_drops_watches_after_send(self):
        self.host._session = FakeSession()
        self.host._wake = Mock()
        self.assertTrue(self.host.deliver_nudge('wake'))
        self.host._wake.drop_watches_for_nudge.assert_called_once_with('wake')
        self.host._wake.post_wake_reply.assert_called_once_with('conversation', 'wake', 'answer')
        self.host.watch_job('job', lane='lane')
        self.host._wake.watch_job.assert_called_once_with('job', lane='lane')

    def test_mcp_config_ignores_workspace_commands_and_pins_host_command_request_land_max_wait(self):
        for name in ('session/mcp.json', 'mcp.json', '.mcp.json'):
            self.write(name, {'mcpServers': {'evil': {'command': '/evil'}}})
        config = m.build_mcp_config(self.ws, self.cred, {'node': 'node', 'director': 'http://director.invalid'})
        self.assertEqual(config, {'mcpServers': {'contextstack': {'command': sys.executable, 'args': [
            str(m.HERE.parents[1] / 'scripts/cs'), 'mcp', '--request-land', '--max-wait', '30',
            '--credential-file', str(self.cred), '--node', 'node', '--director', 'http://director.invalid']}}})
        self.assertEqual(len(m.build_mcp_config(self.ws, self.cred)['mcpServers']), 1)

    def test_cli_extra_accepts_only_bounded_delay_in_json_or_nul_encoding(self):
        self.assertEqual(m.parse_cli_extra(' '), [])
        for value in ('0', '10', '0.25'):
            for raw in (json.dumps(['--turn-delay', value]), '--turn-delay\0' + value):
                self.assertEqual(m.parse_cli_extra(raw), ['--turn-delay', value])
        for raw in ('[', '["--turn-delay", 1]', '["--turn-delay", "11"]',
                    '["--turn-delay", "-1"]', '["--turn-delay", "nan"]',
                    '["--mcp-config", "evil"]', '--turn-delay 1', '["--turn-delay", "1", "extra"]'):
            with self.subTest(raw=raw), self.assertRaisesRegex(SystemExit, 'permits only'):
                m.parse_cli_extra(raw)

    def test_redact_text_removes_credential_shapes_and_preserves_plain_text(self):
        for secret in ('sk-ant-private', 'cslead_private', 'csl_' + 'a' * 64,
                       'ghp_private', 'Bearer private-token', 'APP_API_KEY=private'):
            self.assertIn('<redacted>', m.redact_text(secret))
            self.assertNotIn('private', m.redact_text(secret))
        self.assertEqual(m.redact_text('ordinary reply\nnext'), 'ordinary reply\nnext')

    def test_turn_timeout_stops_session_restores_depth_and_flushes_wakes(self):
        self.host._session = FakeSession(TimeoutError())
        self.host._workspace = self.ws
        self.host._tmp = self.root / 'runtime'
        self.host._wake = Mock()
        with self.assertRaises(TimeoutError):
            self.host.handle_turn(self.ws, message='turn')
        self.assertTrue(self.host._session.stopped)
        self.assertEqual(self.host._turn_depth, 0)
        self.host._wake.force_flush.assert_called_once_with()

    def test_turn_lock_serializes_concurrent_turns_and_depth_persists_session(self):
        entered = threading.Event()
        release = threading.Event()
        second_started = threading.Event()
        calls = []
        errors = []
        sess = FakeSession()
        def run(text, timeout):
            calls.append(text)
            self.assertEqual(self.host._turn_depth, 1)
            if text == 'first':
                entered.set()
                self.assertTrue(release.wait(5))
            return text
        sess.run_turn = run
        self.host._session = sess
        self.host._workspace = self.ws
        self.host._tmp = self.root / 'runtime'
        def turn(text):
            try:
                if text == 'second':
                    second_started.set()
                self.host.handle_turn(self.ws, message=text)
            except BaseException as e:
                errors.append(e)
        first = threading.Thread(target=turn, args=('first',))
        second = threading.Thread(target=turn, args=('second',))
        first.start()
        try:
            self.assertTrue(entered.wait(5))
            second.start()
            self.assertTrue(second_started.wait(5))
            self.assertFalse(self.host._lock.acquire(blocking=False))
            self.assertEqual(calls, ['first'])
        finally:
            release.set()
            first.join(5)
            if second.ident:
                second.join(5)
        self.assertFalse(first.is_alive() or second.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(calls, ['first', 'second'])
        self.assertEqual(self.host._turn_depth, 0)
        self.assertEqual(m.load_state(self.sf)['cli_session_id'], sess.session_id)

    def test_new_session_resumes_only_ids_present_in_conversation_home_and_reuses_live_cli(self):
        for local in (False, True):
            with self.subTest(local=local):
                host = m.ConversationHost('conversation', self.state, self.config)
                host._sock_path = self.root / 'socket'
                m.save_state(self.sf, {'cli_session_id': 'saved'})
                if local:
                    self.write('.claude/saved.jsonl', '{}', self.cred.parent / 'home')
                sess = FakeSession()
                with patch.object(m.tempfile, 'mkdtemp', return_value=str(self.root / 'runtime')), \
                     patch.object(m, 'make_session', return_value=sess) as make:
                    self.assertEqual(host.handle_turn(self.ws, message='first'), 'answer')
                    self.assertEqual(host.handle_turn(self.ws, message='second'), 'answer')
                make.assert_called_once()
                self.assertEqual(make.call_args.kwargs['resume'], 'saved' if local else None)
                self.assertEqual(make.call_args.kwargs['extra_env'], {'LEAD_HOST_SOCKET': str(host._sock_path)})
                self.assertEqual(sess.sent, ['first', 'second'])
                self.assertFalse(host._first_turn)

    def test_session_setup_failure_never_leaves_turn_depth_busy(self):
        self.host._workspace = self.ws
        self.host._tmp = self.root / 'runtime'
        dead = FakeSession()
        dead.alive = False
        self.host._session = dead
        with patch.object(m, 'make_session', side_effect=RuntimeError('setup')):
            with self.assertRaisesRegex(RuntimeError, 'setup'):
                self.host.handle_turn(self.ws, message='turn')
        self.assertTrue(dead.stopped)
        self.assertEqual(self.host._turn_depth, 0)

    def test_turn_message_reads_each_bundles_decision_log(self):
        self.write('tasks/turn/goal.md', ' goal\n')
        self.write('conversation/context.md', ' transcript\n')
        self.write('conversation/decision-log.md', '# Decision log\nFirst revision\n')
        self.assertEqual(m.turn_message(self.ws, first_turn=True),
                         'transcript\n\n# Decision log\nFirst revision\n\ngoal')
        self.write('conversation/decision-log.md', '# Decision log\nSecond revision\n')
        self.assertEqual(m.turn_message(self.ws, first_turn=False),
                         '# Decision log\nSecond revision\n\n goal\n')
        (self.ws / 'conversation/decision-log.md').unlink()
        self.assertEqual(m.turn_message(self.ws, first_turn=False), ' goal\n')

    def test_turn_message_includes_context_only_on_first_turn_and_bundle_prefers_session_file(self):
        self.write('tasks/turn/goal.md', ' goal\n')
        self.assertEqual(m.turn_message(self.ws, first_turn=True), 'goal')
        self.write('prompts/system.md', ' system\n')
        self.write('conversation/context.md', ' context\n')
        self.assertEqual(m.turn_message(self.ws, first_turn=True), 'system\n\ncontext\n\ngoal')
        self.assertEqual(m.turn_message(self.ws, first_turn=False), ' goal\n')
        other = self.root / 'other'
        self.assertEqual(m.turn_bundle(self.ws / 'tasks/turn', other), other)
        self.write('session/mcp.json', '{}')
        self.assertEqual(m.turn_bundle(self.ws / 'tasks/turn', other), self.ws)
        self.assertTrue(m.is_hosted_turn_task(self.ws / 'tasks/turn', {'id': 'hosted-lead'}))
        self.assertFalse(m.is_hosted_turn_task(self.ws / 'tasks/turn', {'id': 'other'}))
        self.assertTrue(m.authorize_hosted_turn({}, self.ws, {}, self.root))
        self.assertTrue(m.authorize_hosted_turn({'conversation_id': 'c', 'turn_id': 't'}, self.ws, {}, self.root))
        self.assertEqual(m.authorize_hosted_turn({'conversation_id': 'c', 'turn_id': 't'},
                                               self.ws / 'tasks/turn', {'id': 'hosted-lead'}, self.root), '')

    def test_metadata_falls_back_to_install_and_spool_without_cross_conversation_legacy(self):
        self.assertEqual(m.conversation_meta(self.config, self.state, 'unknown'), {})
        self.write('install.json', {'director': 'http://install.invalid'}, self.config)
        meta = m.conversation_meta(self.config, self.state, 'new')
        self.assertEqual(meta, {'director': 'http://install.invalid', 'conversation_id': 'new'})
        self.assertEqual(m.conversation_meta(self.config, self.state, 'new'), meta)
        self.write('policy/spool.json', {'receiver': {'url': 'http://spool.invalid'}}, self.config)
        self.assertEqual(m.conversation_meta(self.config, self.state, 'spool')['director'], 'http://spool.invalid')
        self.write('hosted-lead.json', 'broken', self.config)
        self.assertIsNone(m.resolve_conversation_id_or_none(self.config))
        with self.assertRaisesRegex(SystemExit, 'cannot read'):
            m.resolve_conversation_id(self.config)
        self.write('hosted-lead.json', {}, self.config)
        with self.assertRaisesRegex(SystemExit, 'no conversation_id'):
            m.resolve_conversation_id(self.config)
        self.write('hosted-lead.json', {'conversation_id': 'legacy'}, self.config)
        self.assertEqual(m.resolve_conversation_id(self.config), 'legacy')
        self.assertEqual(m.resolve_conversation_id(self.config, 'override'), 'override')

    def test_node_credential_mint_uses_node_file_token_and_handles_unreachable_or_absent_config(self):
        import _director_client
        self.assertIsNone(m._mint_lead_credential_from_node('conversation', self.config, self.state))
        self.write('install.json', {'director': 'http://node.invalid', 'node': 'node'}, self.config)
        with patch('_node_env.node_env', return_value={'CONTEXTSTACK_NODE_TOKEN': 'fixture-node'}), \
             patch.object(_director_client, 'Client') as client:
            client.return_value.call.return_value = {'token': ' minted '}
            self.assertEqual(m._mint_lead_credential_from_node('conversation', self.config, self.state), 'minted')
            self.assertEqual(client.call_args.kwargs['token'], 'fixture-node')
            client.return_value.call.assert_called_once_with('POST', 'conversations/conversation/lead-credential')
            client.return_value.call.return_value = {}
            self.assertIsNone(m._mint_lead_credential_from_node('conversation', self.config, self.state))
        self.assertTrue(m.fetch_lead_credential_from_director('conversation', self.config, self.state))
        self.cred.unlink()
        with patch.object(m, '_mint_lead_credential_from_node', return_value=None):
            self.assertFalse(m.fetch_lead_credential_from_director('conversation', self.config, self.state))
        with patch.object(m, '_mint_lead_credential_from_node', return_value='minted'):
            self.assertTrue(m.fetch_lead_credential_from_director('conversation', self.config, self.state))
        self.assertEqual(self.cred.read_text(), 'minted\n')

    def test_owner_mint_requires_hosted_lead_identity_and_owner_credential(self):
        m.ensure_session_credential('conversation', self.config, self.state, 'sonnet')
        self.cred.unlink()
        with self.assertRaisesRegex(SystemExit, 'no Director URL'):
            m.ensure_lead_ready('conversation', self.config, self.state, 'sonnet')
        self.write('meta.json', {'director': 'http://director.invalid'}, self.cred.parent)
        with self.assertRaisesRegex(SystemExit, 'without the owner credential'):
            m.ensure_lead_ready('conversation', self.config, self.state, 'sonnet')
        self.write('director/local-credential', 'owner', self.state)
        with patch.object(cs_lead, 'OwnerClient') as owner:
            for answer, message in (((404, {}, {}), 'could not read'),
                                    ((200, {'conversation': {}}, {}), 'not a hosted lead')):
                owner.return_value.request.return_value = answer
                with self.assertRaisesRegex(SystemExit, message):
                    m.ensure_lead_ready('conversation', self.config, self.state, 'sonnet')
            owner.return_value.request.side_effect = [(200, {'conversation': {'lead': True}}, {}),
                                                       (201, {'token': 'minted'}, {})]
            with patch.object(m, 'fetch_lead_credential_from_director', return_value=False):
                m.ensure_session_credential('conversation', self.config, self.state, 'sonnet')
        self.assertEqual(self.cred.read_text(), 'minted\n')

    def test_idle_parent_and_ready_environment_parsing_has_safe_defaults(self):
        for raw, idle, ready, parent in (('', 1800, 60, None), ('bad', 1800, 60, None),
                                         ('-1', 0, 5, -1), ('12', 12, 12, 12)):
            with patch.dict(os.environ, {'LEAD_HOST_IDLE_EXIT_S': raw, 'LEAD_HOST_SERVE_READY_S': raw,
                                        'LEAD_HOST_PARENT_PID': raw}):
                self.assertEqual(m._idle_exit_seconds(), idle)
                self.assertEqual(m._serve_ready_seconds(), ready)
                self.assertEqual(m._watch_parent_pid(), parent)
        for error, alive in ((None, True), (ProcessLookupError(), False), (PermissionError(), True)):
            with patch.object(m.os, 'kill', side_effect=error):
                self.assertEqual(m._pid_alive(999), alive)

    def test_socket_auth_rejects_foreign_peer_missing_or_wrong_token_and_dispatches_authorized_ops(self):
        self.host._token = 'private-fixture-token'
        cases = [(None, {}, 'forbidden peer'), (os.getuid(), {}, 'unauthenticated request'),
                 (os.getuid(), {'token': 3}, 'unauthenticated request'),
                 (os.getuid(), {'token': 'wrong'}, 'unauthenticated request'),
                 (os.getuid(), {'token': self.host._token, 'op': 'other'}, 'unknown op')]
        for peer, req, error in cases:
            conn = Mock()
            conn.recv.return_value = (json.dumps(req) + '\n').encode()
            with patch.object(m, '_peer_uid', return_value=peer):
                m._serve_conn(conn, self.host)
            self.assertEqual(json.loads(conn.sendall.call_args.args[0]), {'error': error})
            conn.close.assert_called_once()
        self.host._session = FakeSession()
        for req, expected in (({'op': 'ping'}, {'ok': True}), ({'op': 'watch_job', 'job_id': 'j'}, {'ok': True}),
                              ({'op': 'turn', 'workspace': str(self.ws), 'timeout': 4},
                               {'reply': '<redacted>', 'session_id': 'cli-session'})):
            conn = Mock()
            conn.recv.return_value = (json.dumps(dict(req, token=self.host._token)) + '\n').encode()
            with patch.object(m, '_peer_uid', return_value=os.getuid()), \
                 patch.object(self.host, 'handle_turn', return_value='sk-ant-private'), \
                 patch.object(self.host, 'watch_job') as watch:
                m._serve_conn(conn, self.host)
            self.assertEqual(json.loads(conn.sendall.call_args.args[0]), expected)
            if req['op'] == 'watch_job':
                watch.assert_called_once_with('j', lane=None)

    def test_socket_errors_are_redacted_and_empty_connections_close_without_dispatch(self):
        self.host._token = 'token'
        for data in (b'', b'x' * 65537, b'invalid\n', b'{"token":"token","op":"turn"}\n'):
            conn = Mock()
            conn.recv.return_value = data
            with patch.object(m, '_peer_uid', return_value=os.getuid()), \
                 patch.object(self.host, 'handle_turn', side_effect=SystemExit('sk-ant-private')), \
                 patch('builtins.print') as output:
                m._serve_conn(conn, self.host)
            conn.close.assert_called_once()
            if data:
                self.assertNotIn('sk-ant-private', str(output.call_args))
                self.assertIn('error', json.loads(conn.sendall.call_args.args[0]))
            else:
                conn.sendall.assert_not_called()

    def test_socket_request_injects_private_token_and_reads_fragmented_reply(self):
        conn = Mock()
        conn.__enter__ = Mock(return_value=conn)
        conn.__exit__ = Mock(return_value=False)
        conn.recv.side_effect = [b'{"ok":', b'true}\n']
        payload = {'op': 'ping'}
        with patch.object(m.socket, 'socket', return_value=conn), \
             patch.object(m, 'read_socket_token', return_value='fixture-token'):
            self.assertEqual(m.socket_request(self.root / 'socket', payload), {'ok': True})
        self.assertEqual(payload, {'op': 'ping'})
        self.assertEqual(json.loads(conn.sendall.call_args.args[0]), {'op': 'ping', 'token': 'fixture-token'})

    def test_socket_request_relays_each_event_before_returning_the_reply(self):
        conn = Mock()
        conn.__enter__ = Mock(return_value=conn)
        conn.__exit__ = Mock(return_value=False)
        conn.recv.side_effect = [b'{"cli_event":{"type":"system"}}\n{"cli_ev',
                                 b'ent":{"type":"result"}}\n{"reply":"answer"}\n']
        events = []
        with patch.object(m.socket, 'socket', return_value=conn), \
             patch.object(m, 'read_socket_token', return_value='fixture-token'):
            response = m.socket_request(self.root / 'socket', {'op': 'turn'}, on_event=events.append)
        self.assertEqual(events, [{'type': 'system'}, {'type': 'result'}])
        self.assertEqual(response, {'reply': 'answer'})

    def test_socket_request_closed_without_reply_reports_host_log_location(self):
        conn = Mock()
        conn.__enter__ = Mock(return_value=conn)
        conn.__exit__ = Mock(return_value=False)
        conn.recv.return_value = b''
        with patch.object(m.socket, 'socket', return_value=conn), \
             patch.object(m, 'read_socket_token', return_value='fixture-token'):
            with self.assertRaisesRegex(RuntimeError, 'closed the connection without a reply'):
                m.socket_request(self.root / 'socket', {})

    def test_ensure_serving_spawns_detached_host_preserving_only_explicit_watcher(self):
        sock = self.root / 'socket'
        with patch.object(m, 'socket_file', return_value=sock), \
             patch.object(m, 'private_socket_dir'), patch.object(m, '_wait_socket_ping', return_value=True), \
             patch.object(m.subprocess, 'Popen') as spawn, \
             patch.dict(os.environ, {'LEAD_HOST_PARENT_PID': '123', 'LEAD_HOST_WATCH_PID': '456',
                                     'LEAD_HOST_IDLE_EXIT_S': ' 12 '}):
            self.assertEqual(m.ensure_serving('conversation', self.state, self.config,
                                             model='sonnet', cli_binary='fake', sandbox=True), sock)
        env = spawn.call_args.kwargs['env']
        self.assertEqual(env['LEAD_HOST_PARENT_PID'], '456')
        self.assertEqual(env['LEAD_HOST_IDLE_EXIT_S'], '12')
        self.assertEqual(env['CS_STATE_ROOT'], str(self.state))
        self.assertTrue(spawn.call_args.kwargs['start_new_session'])
        self.assertEqual(spawn.call_args.args[0][-2:], ['--cli', 'fake'])
        with patch.object(m, 'socket_file', return_value=sock), \
             patch.object(m, 'private_socket_dir'), patch.object(m, '_wait_socket_ping', return_value=False), \
             patch.object(m.subprocess, 'Popen') as spawn:
            with self.assertRaisesRegex(TimeoutError, 'ensure_serving timed out'):
                m.ensure_serving('conversation', self.state, self.config, model='s', cli_binary='fake', sandbox=False)
        self.assertNotIn('LEAD_HOST_PARENT_PID', spawn.call_args.kwargs['env'])
        self.assertIn('--no-sandbox', spawn.call_args.args[0])

    def test_client_turn_validates_bundle_before_request_and_surfaces_host_refusal(self):
        with patch.object(m, 'ensure_serving', return_value=self.root / 'socket') as ensure, \
             patch.object(m, 'socket_request', return_value={'reply': 'answer'}) as request:
            self.assertEqual(m.client_turn(self.ws, conversation_id='conversation', config_root=self.config,
                                           state=self.state), 'answer')
            self.assertEqual(request.call_args.args[1]['workspace'], str(self.ws))
            ensure.assert_called_once()
            request.return_value = {'error': 'refused'}
            with self.assertRaisesRegex(SystemExit, 'refused'):
                m.client_turn(self.ws, conversation_id='conversation', config_root=self.config, state=self.state)

    def test_stop_host_closes_bridge_escalates_stuck_child_and_finalizes_login_copies(self):
        sess = Mock()
        self.host._session = sess
        sess.proc.poll.side_effect = [None, 0]
        sess.proc.wait.side_effect = [m.subprocess.TimeoutExpired('fake', 1), None]
        with patch.object(m.os, 'killpg') as kill:
            m._stop_host_session(self.host)
        self.assertEqual([c.args[1] for c in kill.call_args_list], [m.signal.SIGTERM, m.signal.SIGKILL])
        sess.mcp_bridge.close.assert_called_once()
        sess._finish_login_copies.assert_called_once()
        self.host._session = None
        m._stop_host_session(self.host)

    def test_serve_lifecycle_publishes_private_token_dispatches_connection_and_cleans_up(self):
        sock = self.root / 'transport/socket'
        sock.parent.mkdir()
        server = Mock()
        server.bind.side_effect = lambda path: Path(path).touch()
        conn = Mock()
        server.accept.return_value = (conn, None)
        self.host._tmp = self.root / 'runtime'
        self.host._tmp.mkdir()
        coordinator = Mock()
        with patch.object(m, 'socket_file', return_value=sock), \
             patch.object(m, 'sweep_socket_dirs'), patch.object(m, 'temporary_socket_dir', return_value=False), \
             patch.object(m, 'private_socket_dir'), patch.object(m, 'remove_stale_socket'), \
             patch.object(m, 'remove_socket_dir') as remove, \
             patch.object(m, 'ConversationHost', return_value=self.host), \
             patch.object(m, 'start_wake', return_value=coordinator), \
             patch.object(m.socket, 'socket', return_value=server), \
             patch.object(m.signal, 'signal'), patch.object(m, '_stop_host_session') as stop, \
             patch.object(m, 'serve_should_exit', side_effect=[None, None, 'idle timeout']), \
             patch.object(m.select, 'select', side_effect=[([], [], []), ([server], [], [])]), \
             patch.object(m.threading, 'Thread') as thread, patch('builtins.print'):
            self.assertEqual(m.serve_loop('conversation', self.config, self.state,
                                         model='sonnet', cli_binary='fake', sandbox=True), 0)
        server.listen.assert_called_once_with(5)
        thread.assert_called_once_with(target=m._serve_conn, args=(conn, self.host), daemon=True)
        thread.return_value.start.assert_called_once()
        self.assertEqual(coordinator.tick_batch.call_count, 2)
        coordinator.stop.assert_called_once()
        server.close.assert_called_once()
        stop.assert_called_once_with(self.host)
        remove.assert_called_once_with(sock)
        self.assertFalse(sock.exists())
        self.assertFalse(self.host._tmp.exists())
        self.assertEqual((sock.parent / 'token').read_text().strip(), self.host._token)
        self.assertEqual((sock.parent / 'token').stat().st_mode & 0o777, 0o600)
        data = m.load_state(self.sf)
        self.assertEqual(data['serve_pid'], os.getpid())
        self.assertTrue(data['serve_sandboxed'])
        self.assertIn('exit: idle timeout', (self.sf.parent / 'conversation-serve.log').read_text())

    def test_serve_lifetime_lock_refuses_duplicate_server_without_starting_session(self):
        sock = self.root / 'transport/socket'
        sock.parent.mkdir()
        with patch.object(m, 'socket_file', return_value=sock), \
             patch.object(m, 'sweep_socket_dirs'), patch.object(m, 'private_socket_dir'), \
             patch.object(m.fcntl, 'flock', side_effect=BlockingIOError()), \
             patch.object(m, 'start_wake') as start:
            self.assertEqual(m.serve_loop('conversation', self.config, self.state,
                                         model='s', cli_binary='fake', sandbox=True), 0)
        start.assert_not_called()

    def test_session_build_scopes_home_bridge_socket_and_cli_configuration(self):
        import _lead_mcp
        tmp = self.root / 'runtime'
        home = self.root / 'session-home'
        bridge = Mock()
        bridge.path = self.root / 'bridge/socket'
        bridge.config = {'mcpServers': {'bridge': {}}}
        mcp = m.build_mcp_config(self.ws, self.cred)
        with patch.object(m, 'prepare_runtime_home') as prepare, \
             patch.object(_lead_mcp, 'HostMCPBridge', return_value=bridge) as bridge_type, \
             patch.object(m, 'sandbox_builder', return_value='builder') as builder, \
             patch.object(m, 'ClaudeStreamSession') as session:
            result = m.make_session(self.ws, tmp, model='sonnet', resume='saved', binary='fake',
                                    mcp_config=mcp, transcript=None, sandbox=True,
                                    cli_extra=['--turn-delay', '1'], extra_env={'FIXTURE': 'yes'}, runtime_home=home)
        prepare.assert_called_once_with(home, stage_login=False)
        self.assertEqual(bridge_type.call_args.args, (mcp, self.ws))
        self.assertEqual(bridge_type.call_args.kwargs['env']['FIXTURE'], 'yes')
        self.assertEqual(builder.call_args.kwargs['write_roots'], (home,))
        self.assertEqual(builder.call_args.kwargs['allow_sockets'], (bridge.path,))
        self.assertEqual(session.call_args.kwargs['tools'], '')
        self.assertEqual(session.call_args.kwargs['mcp_config'], bridge.config)
        self.assertEqual(session.call_args.kwargs['resume'], 'saved')
        self.assertEqual(result.mcp_bridge, bridge)
        with patch.object(m, 'prepare_runtime_home') as prepare, patch.object(m, 'ClaudeStreamSession') as session:
            m.make_session(self.ws, tmp, model='s', resume=None, binary='fake', mcp_config={},
                           transcript=None, sandbox=False)
        prepare.assert_called_once_with(tmp / 'runtime-home', stage_login=False)
        self.assertIsNone(session.call_args.kwargs['sandbox_argv_builder'])

    def test_sandbox_profile_denies_other_session_homes_and_grants_only_own_bridge_last(self):
        tmp = self.root / 'runtime'
        tmp.mkdir()
        profile = tmp / 'profile.sb'
        home = self.state / 'lead-sessions/conversation/home'
        bridge = self.root / 'bridge/socket'
        sandbox = Mock(return_value=['sandbox', 'fake'])
        with patch.object(m, '_load_sandbox_cmd', return_value=sandbox), \
             patch.object(m, 'roots', return_value={'state': self.state}):
            builder = m.sandbox_builder(self.ws, tmp, profile, write_roots=(home,), allow_sockets=(bridge,))
            self.assertEqual(builder('fake', ['arg']), ['sandbox', 'fake'])
        # Rust renders the lead suffix from this typed record (sandbox/src/cli_executor_variants.rs).
        lead = sandbox.call_args.kwargs['caller']['lead']
        sessions = str((self.state / 'lead-sessions').resolve())
        self.assertIn(sessions, lead['denied_session_roots'])
        self.assertEqual(lead['own_session_roots'], [str(home.resolve().parent)])
        self.assertEqual(lead['write_roots'], [str(home.resolve())])
        # Bridge sockets reach Rust absolute but unresolved, so a symlinked socket path keeps its literal.
        self.assertEqual(lead['bridge_sockets'], [os.path.abspath(str(bridge))])

    def test_sandbox_policy_requires_both_development_flags_and_refuses_unsupported_platform(self):
        self.sandbox.stop()
        with patch.object(m.sys, 'platform', 'linux'):
            self.assertTrue(m.require_host_sandbox(True))
            with self.assertRaisesRegex(SystemExit, 'require executor confinement'):
                m.require_host_sandbox(False)
        with patch.object(m.sys, 'platform', 'unsupported'):
            with self.assertRaisesRegex(SystemExit, 'sandbox unavailable'):
                m.require_host_sandbox(True)
        with patch.object(m.sys, 'platform', 'darwin'), self.assertRaisesRegex(SystemExit, 'requires both'):
            m.require_host_sandbox(False)
        with patch.dict(os.environ, {'LEAD_HOST_NO_SANDBOX': '1', 'CS_EXECUTOR_ALLOW_UNSANDBOXED': '1'}), \
             patch('sys.stderr', new_callable=io.StringIO), patch.object(m.sys, 'platform', 'darwin'):
            self.assertFalse(m.require_host_sandbox(False))
        with patch.object(m.sys, 'platform', 'darwin'):
            self.assertTrue(m.require_host_sandbox(True))

    def test_wait_socket_ping_tolerates_missing_socket_and_transient_error_until_ready(self):
        sock = Mock()
        sock.is_socket.side_effect = [False, True, True]
        with patch.object(m.time, 'monotonic', return_value=0), patch.object(m.time, 'sleep'), \
             patch.object(m, 'socket_request', side_effect=[OSError(), {'ok': True}]):
            self.assertTrue(m._wait_socket_ping(sock, 1))
        with patch.object(m.time, 'monotonic', return_value=2):
            self.assertFalse(m._wait_socket_ping(sock, 1))

    def test_hosted_executor_persists_redacted_reply_checks_and_publishes_evidence(self):
        import _run_record
        code_root = HERE.parents[2]
        run = self.state / 'runs/run'
        run.mkdir(parents=True)
        trace = _run_record.ObservedTrace(run, code_root, 'tasks/turn', 1, _run_record.trace_clock())
        row = {'stimulus_sha': 'stim', 'transcript': 'ledger/runs/run'}
        env = dict(os.environ, CS_CODE_ROOT=str(code_root), CS_CONFIG_ROOT=str(self.config),
                   CS_STATE_ROOT=str(self.state))

        def emit(verdict, stop):
            # Mirror executor.emit: the executor already owns the ledger row.
            row.update(verdict=verdict, stop=stop)
            self.write('ledger.jsonl', json.dumps(row) + '\n', self.state)
            _run_record.terminal(run, self.ws, row, run.name, 'hosted-lead', 'tasks/turn',
                                 values['started'], values['check_window'], trace=trace)
            finalized = _run_record.finalize(code_root, run, env)
            self.assertEqual(finalized.get('state'), 'finalized', finalized)
            self.assertFalse(finalized['row_appended'])
            raise SystemExit(0)

        def turn(*args, trace, cost_per_1k, **kwargs):
            self.assertEqual(cost_per_1k, 0)
            trace.event(b'{"type":"result"}')
            return 'answer sk-ant-private'

        def publish(*args):
            self.write('evidence/manifest.json', {'evidence_sha': 'sha', 'capture_state': 'complete'}, run)
            self.write('ready_for_inspection', '', run)
            return {'evidence_sha': 'sha', 'capture_state': 'complete'}, ''

        from _machine_finish import hosted, record_check

        execute_check = Mock(return_value=Mock(returncode=0, stdout='check ok', stderr=''))
        publish_evidence = Mock(side_effect=publish)
        emit_result = Mock(side_effect=emit)

        def hosted_finish():
            hosted(run, self.ws, row, lambda: record_check(run, row, 60, execute_check, hosted=True),
                   emit_result, publish_evidence, code_root)

        values = {'ws': self.ws, 'task': self.ws / 'tasks/turn', 'run_dir': run, 'emit': emit_result,
                  'trace': trace, 'started': m.time.time(), 'check_window': 60, 'CODE_ROOT': code_root,
                  'a': {'conversation_id': 'conversation'}, 'row': row, 'hosted_finish': hosted_finish}
        with patch.object(m, 'roots', return_value={'config': self.config, 'state': self.state}), \
             patch.object(m, 'client_turn', side_effect=turn), self.assertRaises(SystemExit) as exited:
            m.run_hosted_lead_turn(values)
        self.assertEqual(exited.exception.code, 0)
        self.assertEqual((run / 'lead-reply.txt').read_text(), 'answer <redacted>\n')
        for name in ('cli.log', 'transcript.md', 'messages.jsonl'):
            self.assertNotIn('sk-ant-private', (run / name).read_text())
        self.assertEqual(values['row']['turns'], 1)
        self.assertEqual((values['row']['est_usd'], values['row']['est_basis']), (0, 'unreported'))
        self.assertEqual(values['row']['evidence_sha'], 'sha')
        execute_check.assert_called_once()
        publish_evidence.assert_called_once()
        emit_result.assert_called_once_with('pass', 'done')
        directory_map = self.write('trace-directories.json', {'run_directories': {run.name: str(run)}}, self.root)
        checked = subprocess.run([sys.executable, str(code_root.parent / 'tracing/check-trace'),
                                  str(trace.path), str(directory_map)],
                                 env=env, capture_output=True, text=True, timeout=10)
        self.assertEqual(checked.returncode, 0, (checked.stdout, checked.stderr))
        self.assertEqual(json.loads(checked.stdout), {'result': 'complete', 'findings': [], 'missing': []})
        spans = {span['kind']: span for span in map(json.loads, trace.path.read_text().splitlines())}
        self.assertEqual(spans['finalization']['attributes']['evidence_id'], 'sha')

    def test_gateway_cancel_waits_for_hosted_turn_stop(self):
        connection = Mock()
        connection.__enter__ = Mock(return_value=connection)
        connection.__exit__ = Mock(return_value=False)
        connection.recv.side_effect = [
            socket.timeout(), b'{"error":"turn cancelled","exit_status":143,"outcome":"cancelled"}\n',
        ]
        with patch.object(m.socket, "socket", return_value=connection), \
             patch.object(m, "read_socket_token", return_value="fixture"):
            result = m.socket_request(self.root / "socket", {"op": "turn"}, timeout=1,
                                      cancelled=Mock(side_effect=[False, True]))
        self.assertEqual(result["outcome"], "cancelled")
        connection.shutdown.assert_called_once_with(socket.SHUT_WR)
        self.assertEqual(connection.recv.call_count, 2)

        self.host._token = "fixture"
        connection = Mock()
        connection.recv.return_value = b'{"op":"turn","token":"fixture","cancel_on_eof":true}\n'

        def cancel_turn(*args, cancelled, **kwargs):
            self.assertTrue(cancelled())
            raise m.TurnCancelled("turn cancelled")

        with patch.object(m, "_peer_uid", return_value=os.getuid()), \
             patch.object(m.select, "select", return_value=([connection], [], [])), \
             patch.object(self.host, "handle_turn", side_effect=cancel_turn):
            m._serve_conn(connection, self.host)
        self.assertEqual(json.loads(connection.sendall.call_args.args[0])["outcome"], "cancelled")

    def test_failed_hosted_turn_records_only_the_failure(self):
        import _run_record
        trace = _run_record.ObservedTrace(self.ws, self.root, 'tasks/turn', 1, _run_record.trace_clock())
        values = {'ws': self.ws, 'task': self.ws / 'tasks/turn', 'run_dir': self.ws, 'emit': Mock(),
                  'trace': trace, 'started': 0, 'check_window': 60, 'child_env': {}, 'CODE_ROOT': self.root,
                  'hosted_finish': Mock(), 'a': {'conversation_id': 'conversation'}}
        error = m.HostedTurnError('turn timed out', 124, 'timeout')
        with patch.object(m, 'roots', return_value={'config': self.config, 'state': self.state}), \
             patch.object(m, 'client_turn', side_effect=error), patch.object(trace, 'exited', wraps=trace.exited) as exited:
            m.run_hosted_lead_turn(values)
        exited.assert_called_once()
        self.assertEqual((trace.status, trace.process_outcome), (124, 'timeout'))
        self.assertEqual((values['row']['est_usd'], values['row']['est_basis']), (0, 'unreported'))
        self.assertIsNotNone(trace.finalization_start)
        values['emit'].assert_called_once_with('fail', 'lead_host', extra={'note': 'turn timed out'})
        values['hosted_finish'].assert_not_called()
