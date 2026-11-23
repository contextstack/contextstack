"""Listener decisions without a Director, child processes, or clock waits."""
import importlib.machinery
import importlib.util
import io
import json
import os
import signal
import subprocess
import sys
import unittest
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import Mock, patch

HERE = Path(__file__).resolve().parent
OPS = HERE.parents[2] / 'ops/bin'
sys.path[:0] = [str(HERE.parent / '_lib'), str(OPS)]
from hermetic_env import hermetic_env
from scratch import scratch_dir
from _director_client import Client, FALLBACK, OPENER, ProtocolRefused
import _node_control as control


def load_listener(config):
    loader = importlib.machinery.SourceFileLoader('unit_node_listen', str(OPS / 'node-listen'))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    with patch.dict(os.environ, {**hermetic_env(), 'CS_CONFIG_ROOT': str(config)}, clear=True):
        loader.exec_module(module)
    return module


class NodeCancelOwnershipTests(unittest.TestCase):
    def test_recycled_group_is_not_signalled_after_term(self):
        identities = {10: 100, 11: 101}
        rows = {11: (10, 11)}
        sent = []

        def send(pid, birth, sig):
            if identities.get(pid) != birth:
                return False
            sent.append((pid, sig))
            if sig == signal.SIGTERM:
                identities[11] = 102  # group ID and leader PID now belong to an unrelated process
                rows[11] = (1, 11)
            return True

        with patch.object(control, "process_rows", side_effect=lambda: dict(rows)), \
             patch.object(control, "_pid_identity", side_effect=identities.get), \
             patch.object(control, "_signal_pid", side_effect=send), patch.object(control, "GRACE_S", 0):
            control.stop_children(10, 100)
        self.assertEqual(sent, [(11, signal.SIGTERM)])

    def test_escalation_uses_live_member_to_verify_new_fork_after_leader_exits(self):
        rows = {12: (1, 11), 13: (12, 11)}
        with patch.object(control, "process_rows", return_value=rows), \
             patch.object(control, "_pid_identity", side_effect={12: 102, 13: 103}.get):
            self.assertEqual(control.escalation_members(11, {11: 101, 12: 102}),
                             {11: 101, 12: 102, 13: 103})


class ListenerTests(unittest.TestCase):
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(scratch_dir(prefix='listener-units-')))
        self.stack.enter_context(patch.dict(os.environ, {**hermetic_env(),
            'HOME': str(self.root), 'CS_REPOS_FILE': str(self.root / 'repos.json')}, clear=True))
        self.m = load_listener(self.root)
        self.say = self.stack.enter_context(patch.object(self.m, 'say'))
        self.client = Mock(protocol=self.m.PROTOCOL)
        self.listener = self.m.Listener(self.client, self.root, self.root, self.root)

    def rows(self, rows):
        (self.root / 'dispatch.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in rows))

    def test_protocol_reprobe_boundary(self):
        self.client.protocol = 1
        self.assertFalse(self.listener.speaks2(now=lambda: 100))
        self.assertFalse(self.listener.speaks2(now=lambda: 699))
        self.assertEqual(self.say.call_count, 1)
        self.assertTrue(self.listener.speaks2(now=lambda: 700))
        self.assertIsNone(self.listener.downgraded_at)
        self.assertTrue(self.listener.speaks2(now=lambda: 701))

    def test_downgrade_between_duty_check_and_request(self):
        client = Client({'url': 'http://127.0.0.1:1'}, token='fixture')
        self.listener.client = client
        self.assertTrue(self.listener.speaks2())
        client.protocol = FALLBACK
        with patch.object(OPENER, 'open') as send:
            for path in ('status', 'schedules'):
                with self.subTest(path=path), self.assertRaises(ProtocolRefused):
                    client.call('POST', path, {})
            send.assert_not_called()

    def test_op_result_and_failures(self):
        run = Mock(return_value=subprocess.CompletedProcess([], 1, 'progress\n{"ok": false}\n', 'failed'))
        self.assertEqual(self.listener.op('fake', timeout=12, run=run), {'ok': False})
        self.assertEqual(run.call_args.kwargs['timeout'], 12)
        self.assertEqual(run.call_args.kwargs['cwd'], str(self.root))
        self.assertEqual(run.call_args.args[0], [sys.executable, str(OPS / 'fake')])
        for stdout in ['', 'bad json']:
            run.return_value = subprocess.CompletedProcess([], 0, stdout, '')
            self.assertFalse(self.listener.op('fake', run=run)['ok'])
        for error in [OSError('missing'), subprocess.TimeoutExpired('fake', 1)]:
            run.side_effect = error
            result = self.listener.op('fake', run=run)
            self.assertFalse(result['ok'])
            self.assertEqual(result['error'], str(error))

    def test_call_only_swallows_unreachable(self):
        self.client.call.return_value = {'ok': True}
        self.assertEqual(self.listener.call('status', 'GET', 'status'), {'ok': True})
        self.client.call.side_effect = self.m.DirectorUnreachable('offline')
        self.assertIsNone(self.listener.call('status', 'GET', 'status'))
        self.client.call.side_effect = ValueError('bad')
        with self.assertRaises(ValueError):
            self.listener.call('status', 'GET', 'status')

    def test_pending_retry_boundaries(self):
        self.listener.kicked_at = 100
        with patch.object(self.listener, 'kick') as kick:
            self.listener.retry_pending([{'state': 'queued'}], now=lambda: 114)
            kick.assert_not_called()
            self.listener.retry_pending([{'state': 'queued'}], now=lambda: 115)
            kick.assert_called_once_with('jobs pending')
            kick.reset_mock()
            self.listener.retry_pending([{'state': 'queued', 'interactive': True}], now=lambda: 101)
            kick.assert_called_once()
            kick.reset_mock()
            self.listener.retry_pending([{'state': 'running'}], now=lambda: 200)
            kick.assert_not_called()

    def test_kick_idle_reserved_and_throttle(self):
        proc = Mock(pid=123)
        proc.poll.return_value = None
        popen = Mock(return_value=proc)
        with patch.object(self.listener, 'active', return_value=[]) as active, patch.object(self.m.threading, 'Thread') as thread:
            self.listener.dispatch = False
            self.listener.kick('disabled', now=lambda: 100, popen=popen)
            popen.assert_not_called()
            self.listener.dispatch = True
            self.listener.kick('idle', now=lambda: 100, popen=popen)
            self.assertEqual(self.listener.kicked_at, 100)
            self.assertNotIn('--turn-seat', popen.call_args.args[0])
            thread.return_value.start.assert_called_once()
            self.listener.kick('busy', now=lambda: 100, popen=popen)
            self.assertEqual(popen.call_count, 1)
            active.return_value = [{'state': 'queued', 'interactive': True}]
            self.listener.kick('turn', now=lambda: 101, popen=popen)
            self.assertIn('--turn-seat', popen.call_args.args[0])
            self.listener.kick('throttled', now=lambda: 101.5, popen=popen)
            self.assertEqual(popen.call_count, 2)
            self.listener.kick('retry', now=lambda: 102, popen=popen)
            self.assertEqual(popen.call_count, 3)
            active.return_value.append({'state': 'running', 'reserved': True})
            self.listener.kick('reserved busy', now=lambda: 104, popen=popen)
            self.assertEqual(popen.call_count, 3)
        self.listener.kicked = None
        with patch.object(self.listener, 'active', return_value=[]):
            with self.assertRaises(OSError):
                self.listener.kick('missing', popen=Mock(side_effect=OSError('missing')))

    def test_active_terminal_wins_and_dead_processes_disappear(self):
        self.assertEqual(self.listener.active(), [])
        self.rows([{'id': 'a', 'state': 'pending'}, {'id': 'a', 'state': 'done'},
                   {'id': 'a', 'state': 'pending'}, {'id': 'b', 'state': 'running', 'pid': 0},
                   {'id': 'c', 'director_job': 'dc', 'state': 'running', 'pid': 22, 'dispatch_seat': 'reserved'},
                   {'id': 'd', 'state': 'pending', 'conversation_id': 'con', 'turn_id': 'turn'}])
        with patch.object(self.m, 'pid_live', side_effect=lambda pid: pid == 22):
            active = self.listener.active()
        self.assertEqual([r['job_id'] for r in active], ['dc', 'd'])
        self.assertTrue(active[0]['reserved'])
        self.assertTrue(active[1]['interactive'])

    def test_recover_orphans_once(self):
        self.assertEqual(self.listener.recover_orphaned_jobs(), [])
        self.rows([{'id': 'a', 'state': 'running', 'pid': 0},
                   {'id': 'a', 'state': 'running', 'pid': 0},
                   {'id': 'live', 'state': 'running', 'pid': 22}])
        with patch.object(self.m, 'pid_live', side_effect=lambda pid: pid == 22):
            self.assertEqual(self.listener.recover_orphaned_jobs(), ['a'])
            self.assertEqual(self.listener.recover_orphaned_jobs(), [])
        row = json.loads((self.root / 'dispatch.jsonl').read_text().splitlines()[-1])
        self.assertEqual(row['stop'], self.m.ORPHANED_RUNNING_STOP)
        self.assertEqual(row['state'], 'error')

    def test_work_cursor_backoff_and_controls(self):
        answers = [self.m.DirectorUnreachable('offline'), {'cursor': 'a/b', 'changed': False},
                   {'cursor': 'next', 'jobs': [{}], 'control': [{'control_id': 'c'}]}, {}]
        def call(*args):
            answer = answers.pop(0)
            if not answers:
                self.listener.stop.set()
            if isinstance(answer, Exception):
                raise answer
            return answer
        self.client.call.side_effect = call
        with patch.object(self.listener.stop, 'wait') as wait, patch.object(self.m, 'take', return_value=([], ['job'], [])), \
             patch.object(self.m, 'prepare') as prepare, patch.object(self.listener, 'control') as control, \
             patch.object(self.listener, 'kick') as kick:
            self.listener.work_loop()
        wait.assert_called_once_with(1)
        self.assertEqual(self.client.call.call_args_list[2].args[1], 'work?after=a%2Fb')
        prepare.assert_called_once()
        control.assert_called_once()
        kick.assert_called_once_with('job offered')

    def test_settle_cancel_and_error_release_inflight(self):
        message = {'control_id': 'c'}
        with patch.object(self.m.threading, 'Thread') as thread:
            self.listener.control(message)
            self.listener.control(message)
            thread.return_value.start.assert_called_once()
        with patch.object(self.m, 'handle', return_value={'control_id': 'c', 'kind': 'cancel', 'applied_now': True}), \
             patch.object(self.listener, 'op') as op:
            self.listener.settle(message)
            op.assert_called_once_with('spool-send')
        self.assertNotIn('c', self.listener.inflight)
        with patch.object(self.m, 'handle', side_effect=ValueError('bad')):
            self.listener.settle(message)
        self.assertEqual(self.say.call_args.kwargs['what'], 'control')

    def test_status_interval_and_outcomes(self):
        clock = [0]
        times = iter([100, 159, 160])
        def wait(_):
            clock[0] = next(times, None)
            return clock[0] is None
        with patch.object(self.listener.stop, 'wait', side_effect=wait), \
             patch.object(self.listener, 'active', return_value=[{'job_id': 'j', 'state': 'running', 'run_id': None}]), \
             patch.object(self.m, 'live_runs', return_value={'run': (self.root, 'j')}), \
             patch.object(self.m, 'send_events') as events, \
             patch.object(self.m, 'status_extras', return_value={'held': {'reason': 'disk'}}), \
             patch.object(self.m, 'status_limits', return_value={}), \
             patch.object(self.m, 'report_pending') as report, patch.object(self.listener, 'retry_pending'):
            self.client.call.return_value = {}
            self.listener.duties_loop(now=lambda: clock[0])
        self.assertEqual(self.client.call.call_count, 2)
        self.assertEqual(report.call_count, 2)
        self.assertEqual(events.call_count, 3)
        self.assertEqual(self.client.call.call_args.args[2]['active'][0]['run_id'], 'run')

    def test_repo_inventory_changes_trigger_status_without_jobs(self):
        repos = [{'name': 'project', 'path': '/project', 'branch': 'main'}]
        calls = iter([[], repos, []])
        waits = iter([False, False, False, True])
        with patch.object(self.listener.stop, 'wait', side_effect=lambda _: next(waits)), \
             patch.object(self.listener, 'active', return_value=[]), \
             patch.object(self.m, 'live_runs', return_value={}), \
             patch.object(self.m, 'status_extras', return_value={}), \
             patch.object(self.m, 'status_limits', return_value={}), \
             patch.object(self.m, 'repo_inventory', side_effect=lambda: next(calls)), \
             patch.object(self.m, 'report_pending'), patch.object(self.listener, 'retry_pending'):
            self.client.call.return_value = {}
            self.listener.duties_loop(now=lambda: 100)
        reports = [call.args[2]['repos'] for call in self.client.call.call_args_list]
        self.assertEqual(reports, [[], repos, []])

    def test_schedule_dispatch_and_reporting(self):
        (self.root / 'schedules.jsonl').touch()
        for rc, stdout in [(1, 'failed'), (0, '{"admitted": [{}]}')]:
            self.listener.stop.clear()
            run = Mock(return_value=subprocess.CompletedProcess([], rc, stdout, 'error'))
            with patch.object(self.listener.stop, 'is_set', side_effect=[False, True]), \
                 patch.object(self.listener.wake_schedules, 'wait'), \
                 patch.object(self.m, 'schedules_report', return_value=({'schedules': [], 'firings': []}, {})), \
                 patch.object(self.listener, 'kick') as kick:
                self.listener.schedules_loop(now=lambda: 1000, run=run)
            self.assertEqual(run.call_args.kwargs['input'], '{"missed_default": "skip"}')
            self.assertEqual(kick.call_count, int(rc == 0))
        self.assertTrue((self.root / 'node/schedules-reported.json').is_file())

    def test_main_config_variations(self):
        env = {**hermetic_env(), 'CS_STATE_ROOT': str(self.root)}
        with patch.object(sys, 'stdin', io.StringIO('{}')):
            self.assertEqual(self.m.main(config_root=self.root, env=env), 0)
        policy = self.root / 'policy'
        policy.mkdir()
        for raw, expected in [('invalid', 2), ('{"receiver": []}', 0)]:
            (policy / 'spool.json').write_text(raw)
            with patch.object(sys, 'stdin', io.StringIO('{}')):
                self.assertEqual(self.m.main(config_root=self.root, env=env), expected)
        (policy / 'spool.json').write_text(json.dumps({'receiver': {'kind': 'director'}}))
        with (self.root / 'test.lock').open('a') as held, \
             patch.object(self.m, 'open', return_value=held, create=True), \
             patch.object(sys, 'stdin', io.StringIO('{"dispatch": false, "seconds": 1}')), \
             patch.object(self.m, 'receiver_config', return_value={'url': 'http://127.0.0.1:1'}), \
             patch.object(self.m, 'Client'), patch.object(self.m, 'Listener') as listener, \
             patch.object(self.m.signal, 'signal'), patch.object(self.m.threading, 'Thread'), \
             patch.object(self.m.threading, 'Timer') as timer:
            listener.return_value.recover_orphaned_jobs.return_value = ['old']
            self.assertEqual(self.m.main(config_root=self.root, env=env), 0)
            self.assertFalse(listener.call_args.kwargs['dispatch'])
            timer.return_value.start.assert_called_once()
            listener.return_value.duties_loop.assert_called_once()
