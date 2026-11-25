"""Tick decisions with isolated shelves and injected process and clock boundaries."""
import importlib.machinery
import importlib.util
import io
import json
import os
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


class TickTests(unittest.TestCase):
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(scratch_dir(prefix='tick-units-')))
        self.env = {**hermetic_env(), 'CS_STATE_ROOT': str(self.root),
                    'CS_CONFIG_ROOT': str(self.root), 'CS_CODE_ROOT': str(OPS.parents[1])}
        self.stack.enter_context(patch.dict(os.environ, self.env, clear=True))
        loader = importlib.machinery.SourceFileLoader('unit_tick', str(OPS / 'tick'))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        self.m = importlib.util.module_from_spec(spec)
        loader.exec_module(self.m)
        for key, value in dict(CODE_ROOT=self.root, STATE=self.root, Q=self.root/'dispatch.jsonl',
                               LOCK=self.root/'dispatch.lock', LEDGER=self.root/'ledger.jsonl',
                               RUNS=self.root/'runs', RESLOCK=self.root/'resource-locks',
                               SPOOL_POLICY=self.root/'policy/spool.json', NODE=False).items():
            setattr(self.m, key, value)

    def patch(self, name, **kw):
        return self.stack.enter_context(patch.object(self.m, name, **kw))

    def rows(self, rows):
        self.m.Q.write_text(''.join(json.dumps(r)+'\n' for r in rows))

    def policy(self, raw):
        self.m.SPOOL_POLICY.parent.mkdir(exist_ok=True)
        self.m.SPOOL_POLICY.write_text(raw)

    def test_spool_config_receiver_and_disk_floor_variations(self):
        self.assertFalse(self.m.node_role(config_root=self.root))
        for raw, node, low in [('bad', False, None), ('{"receiver": []}', False, None),
                               ('{"receiver":{"kind":"director"},"min_free_bytes":10}', True, (9,10))]:
            self.policy(raw)
            self.assertEqual(self.m.node_role(config_root=self.root), node)
            self.assertEqual(self.m.spool_low(9, config_root=self.root), low)
        self.assertIsNone(self.m.spool_low(10))
        self.assertEqual(self.m.state_root_disk_free(7), 7)
        with patch.object(self.m.os, 'statvfs', side_effect=OSError('absent')):
            self.assertIsNone(self.m.state_root_disk_free())
        self.assertIsNone(self.m.resource_low())

    def test_side_passes_parse_last_line_and_preserve_failure_details(self):
        run = Mock(return_value=subprocess.CompletedProcess([], 1, 'progress\n{"ok":false,"pending":2,"failed":1,"accepted":3,"error":"offline"}', ''))
        self.assertEqual(self.m.spool_pass(run=run)['pending'], 2)
        self.assertEqual(self.m.job_pull_pass(run=run)['accepted'], 3)
        self.assertEqual(self.m.SPOOL['error'], 'offline')
        self.assertEqual(self.m.JOB_PULL['error'], 'offline')
        self.assertEqual(run.call_args.kwargs['timeout'], 300)
        for value in ['', 'bad']:
            run.return_value = subprocess.CompletedProcess([], 0, value, '')
            self.assertIn('error', self.m.spool_pass(run=run))
            self.assertIn('error', self.m.job_pull_pass(run=run))
        for error in [OSError('missing'), subprocess.TimeoutExpired('op', 1)]:
            run.side_effect = error
            self.assertEqual(self.m.spool_pass(run=run)['error'], str(error))
            self.assertEqual(self.m.job_pull_pass(run=run)['error'], str(error))
        run.side_effect = None
        run.return_value = subprocess.CompletedProcess([], 1)
        (self.root / 'terminal.json').touch()
        self.assertFalse(self.m.finalize_run(self.root, run=run))
        self.assertFalse(self.m.local_model_ready(run=run))
        run.return_value.returncode = 0
        self.assertTrue(self.m.finalize_run(self.root, run=run))
        self.assertTrue(self.m.local_model_ready('local-reasoner', run=run))

    def test_unfinished_runs_become_due_at_age_boundary_and_pending_carries_spool_error(self):
        d = self.m.RUNS/'a'
        d.mkdir(parents=True)
        term = d/'terminal.json'
        term.write_text(json.dumps({'spec':'specs/a.yaml','trial':1,'run_id':'a'}))
        os.utime(term, (100,100))
        self.assertEqual(self.m.unfinalized_runs(60, now=lambda:159), [])
        self.assertEqual(self.m.unfinalized_runs(60, now=lambda:160), [d])
        (d/'finalize.error').write_text('{"class":"spool_full","error":"full"}')
        self.patch('finalize_run', return_value=False)
        row = self.m.pending_row('specs/a.yaml','a')
        self.assertEqual(row['stop'], 'finalization_pending')
        self.assertEqual(row['error']['class'], 'spool_full')
        self.assertEqual(self.m.pending_row('other','other'), {})
        self.patch('sweep_orphan_cells', return_value={'removed':1, 'kept':0})
        self.assertEqual(self.m.finalize_sweep(0)['pending'], 1)
        (d/'finalized.json').touch()
        self.assertEqual(self.m.pending_row('a','a'), {})
        term.write_text('bad')
        self.assertEqual(self.m.terminal_of(d), {})

    def test_queue_suppresses_closed_live_and_busy_resource_jobs_and_recovers_orphans_once(self):
        self.rows([{'id':'closed','state':'pending'}, {'id':'closed','state':'done'},
                   {'id':'live','state':'running','pid':22,'resource':'busy'},
                   {'id':'live','state':'pending'}, {'id':'blocked','state':'pending','resource':'busy'},
                   {'id':'dead','state':'running','pid':0}, {'id':'ready','state':'pending'}])
        self.patch('pid_live', side_effect=lambda pid: pid==22)
        self.patch('runner_count', return_value=0)
        self.assertEqual([r['id'] for r in self.m.jobs()], ['ready'])
        self.assertEqual(self.m.occupied(), 1)
        self.assertEqual(self.m.recover_orphaned_jobs(), ['dead'])
        self.assertEqual(self.m.recover_orphaned_jobs(), [])
        self.assertEqual(json.loads(self.m.Q.read_text().splitlines()[-1])['stop'], self.m.ORPHANED_RUNNING_STOP)
        self.assertEqual(self.m.admit([{'id':'a'}])['id'], 'a')

    def test_child_environment_filters_mutation_and_binds_schedule_and_aliases(self):
        self.rows([{'id':'j','slot':'slot','schedule_date':'date','schedule_timezone':'UTC'}])
        self.m.POLICY_ENV.update(child_env=['PATH'], tool_prefix=None, browser_state='browser')
        parent = {'PATH':'/bin','CS_VIDEO_MUTATION':'1','CS_CHILD_ENV':'bad','CS_JOB_ID':'wrong'}
        result = self.m.child_env('j', ['youtube_playlist_mutation'], trial=2, env=parent)
        self.assertEqual(result['CS_JOB_ID'], 'j')
        self.assertEqual(result['CS_SCHEDULE_SLOT'], 'slot')
        self.assertEqual(result['CS_TRIAL'], '2')
        self.assertEqual(result['CS_VIDEO_MUTATION'], '1')
        self.assertEqual(result['CS_BROWSER_STATE'], 'browser')
        result = self.m.child_env(env=parent)
        self.assertNotIn('CS_VIDEO_MUTATION', result)
        self.assertNotIn('CS_JOB_ID', result)
        self.assertEqual(parent['CS_JOB_ID'], 'wrong')

    def test_heartbeat_writes_selected_path_and_swallows_filesystem_failure(self):
        target = self.root/'custom/beat'
        self.m.write_heartbeat(env={'CS_TICK_HEARTBEAT':str(target)})
        self.assertRegex(target.read_text(), r'^\d{4}-.*Z\n$')
        self.m.write_heartbeat(env={})
        self.assertTrue((self.root/'tick-heartbeat').is_file())
        self.m.write_heartbeat(env={'CS_TICK_HEARTBEAT':str(target/'impossible')})

    def test_stale_locks_respect_two_second_boundary_and_live_nonempty_holder(self):
        root = self.root/'.git'
        root.mkdir()
        self.patch('git_lock_roots', return_value=[root])
        for name, content in [('empty.lock',''), ('held.lock','held'), ('old.lock','old')]:
            p=root/name
            p.write_text(content)
            os.utime(p,(100,100))
        lock_held = self.m.lock_held
        self.patch('lock_held', side_effect=lambda p:p.name=='held.lock')
        self.assertEqual(self.m.sweep_stale_locks(now=lambda:101.9), [])
        self.assertEqual(set(self.m.sweep_stale_locks(now=lambda:102)), {str(root/'empty.lock'),str(root/'old.lock')})
        run=Mock(return_value=Mock(stdout='bad 0 22'))
        self.patch('pid_live', side_effect=lambda pid:pid==22)
        self.assertTrue(lock_held(root/'held.lock', run=run))
        run.return_value.stdout='bad 0'
        self.assertFalse(lock_held(root/'held.lock', run=run))

    def prepare_main(self):
        for name in ['sweep_stale_locks','finalize_sweep','recover_orphaned_jobs','job_pull_pass','spool_pass','write_heartbeat']:
            self.patch(name, return_value=[])
        self.patch('occupied', return_value=0)
        self.patch('spool_low', return_value=None)
        self.patch('resource_low', return_value=None)
        self.patch('_seat_candidates', side_effect=lambda p,r,t:p)
        self.patch('local_model', return_value=False)
        self.patch('refresh_job', side_effect=lambda j:j)
        self.output=self.patch('emit')

    def test_main_empty_locked_disk_resource_and_seat_refusals_write_no_running_row(self):
        self.prepare_main()
        self.rows([])
        self.m.main()
        self.assertEqual(self.output.call_args.args[0]['skip'],'empty')
        with self.m.lockf(True):
            self.m.main()
        self.assertEqual(self.output.call_args.args[0]['skip'],'dispatch_lock')
        self.rows([{'id':'j','state':'pending','spec':'s.yaml'}])
        with patch.object(self.m,'spool_low',return_value=(1,2)):
            self.m.main()
        self.assertEqual(self.output.call_args.args[0]['skip'],'spool_full')
        with patch.object(self.m,'resource_low',return_value={'reason':'memory'}):
            self.m.main()
        self.assertEqual(self.output.call_args.args[0]['skip'],'resource_held')
        with patch.object(self.m,'_seat_candidates',return_value=[]):
            self.m.main(turn_only=True)
        self.assertEqual(self.output.call_args.args[0]['skip'],'turn_seat_busy')
        with patch.object(self.m,'local_model',return_value='local-coder'), patch.object(self.m,'local_model_ready',return_value=False):
            self.m.main()
        self.assertEqual(self.output.call_args.args[0]['skip'],'local_coder_unavailable')
        self.assertNotIn('"state": "running"', self.m.Q.read_text())

    def test_main_preflight_refusal_holds_schedule_and_preserves_job_metadata(self):
        self.prepare_main()
        self.rows([{'id':'j','state':'pending','spec':'s.yaml','schedule_id':'s','task':'t'}])
        with patch.object(self.m,'refresh_job',side_effect=RuntimeError('refused')), patch.object(self.m,'hold_schedule') as hold:
            self.m.main()
        hold.assert_called_once_with('s','preflight_refused')
        row=json.loads(self.m.Q.read_text().splitlines()[-1])
        self.assertEqual(row['state'],'error')
        self.assertEqual(row['task'],'t')

    def test_main_completion_writes_terminal_result_and_probe_failure_exits_zero(self):
        self.prepare_main()
        for rc in [0,1]:
            self.rows([{'id':'j','state':'pending','spec':'s.yaml','lane':'probe','resource':'dest'}])
            with patch.object(self.m,'run_spec',return_value=(rc,{'verdict':'pass' if rc==0 else 'fail','stop':'failed','transcript':'runs/r','run':'r'})):
                with self.assertRaises(SystemExit) as exited:
                    self.m.main(turn_only=True)
            self.assertEqual(exited.exception.code,0)
            rows=[json.loads(l) for l in self.m.Q.read_text().splitlines()]
            self.assertEqual(rows[-1]['state'],'done' if rc==0 else 'error')
            self.assertEqual(rows[-1]['run_id'],'r')
            self.assertEqual(rows[-2]['dispatch_seat'],'reserved')

    def test_main_run_spec_crash_writes_a_first_terminal_error_row(self):
        # B1: an exception in run_spec must not leave the job running.
        self.prepare_main()
        self.rows([{'id':'j','state':'pending','spec':'s.yaml','task':'t'}])
        with patch.object(self.m,'run_spec',side_effect=KeyError('boom')):
            with self.assertRaises(SystemExit) as exited:
                self.m.main()
        self.assertEqual(exited.exception.code,1)
        rows=[r for r in map(json.loads,self.m.Q.read_text().splitlines()) if 'state' in r]
        self.assertEqual([r['state'] for r in rows],['pending','running','error'],self.m.Q.read_text())
        self.assertEqual(rows[-1]['verdict'],'void')
        self.assertIn('boom',rows[-1]['stop'])
        self.assertEqual(rows[-1]['task'],'t')

    def test_main_refresh_failure_of_any_kind_closes_only_that_job(self):
        # B2: a non-RuntimeError from refresh_job (bad op JSON, OSError) used to escape and
        # leave the job pending at the head of the queue forever.
        self.prepare_main()
        for error in (ValueError('bad json'), OSError('gone'), IndexError('no output')):
            self.rows([{'id':'bad','state':'pending','spec':'s.yaml','task':'t'}])
            with patch.object(self.m,'refresh_job',side_effect=error):
                self.m.main()
            row=json.loads(self.m.Q.read_text().splitlines()[-1])
            self.assertEqual((row['id'],row['state']),('bad','error'),error)
            self.assertIn(str(error),row['note'])

    def test_ledger_reader_skips_a_cut_off_line(self):
        # B3: one torn ledger line must not break every later read.
        self.m.LEDGER.write_text(json.dumps({'spec':'s','trial':1,'verdict':'pass'})+'\n{"spec": "s", "tri'
                                 +json.dumps({'spec':'s','trial':2,'verdict':'fail'})+'\n')
        self.assertEqual(self.m.execution_row('s','s')['trial'],1)
        self.m.LEDGER.write_text(self.m.LEDGER.read_text()+json.dumps({'spec':'s','trial':3,'verdict':'fail'})+'\n')
        self.assertEqual(self.m.execution_row('s','s')['trial'],3)

    def test_no_row_after_job_process_cleanup_failure_is_retryable(self):
        self.prepare_spec('id: s\nmodel: runner\n')
        self.patch('one_trial', return_value=1)
        self.patch('execution_row', return_value={})
        self.patch('pending_row', return_value={})
        self.m.TRIAL_STDERR[:]=['Traceback\nRuntimeError: job process cleanup deadline exceeded']
        rc,row=self.m.run_spec('s.yaml')
        self.assertTrue(row['error']['retryable'],row)
        self.m.TRIAL_STDERR[:]=['spec refused']
        rc,row=self.m.run_spec('s.yaml')
        self.assertFalse(row['error']['retryable'],row)

    def prepare_spec(self, text='id: s\nmodel: fake\ntrials: 2\ntasks: ["a", "b"]\n'):
        (self.root/'s.yaml').write_text(text)
        self.m.TEMPLATES={'fake':{}}
        self.patch('roots', return_value={'config':self.root})
        self.patch('resolve', return_value=[])
        self.patch('tool_prefix_value', return_value=None)
        self.patch('browser_state_value', return_value=None)
        self.patch('sandbox_reports_dir', return_value=self.root/'reports')
        self.patch('cell_prefix', return_value=None)
        self.patch('cancelled', return_value=False)

    def test_spec_runs_all_tasks_and_trials_retains_first_failure_and_completes_rows(self):
        self.prepare_spec()
        run=self.patch('one_executor_trial', side_effect=[1,0,0,0])
        self.patch('execution_row', return_value={'verdict':'fail','stop':'test'})
        completed=Mock()
        rc,row=self.m.run_spec('s.yaml',completion=completed,external_effects=['effect'],env={})
        self.assertEqual(rc,1)
        self.assertEqual([(c.args[2],c.args[3]) for c in run.call_args_list], [('a',1),('b',1),('a',2),('b',2)])
        self.assertEqual(completed.call_count,4)
        self.assertEqual(row['stop'],'test')

    def test_selected_codex_model_uses_executor_and_reaches_cli_argv(self):
        import _executor

        self.prepare_spec('id: s\nmodel: "codex:gpt-6.2-sol"\ntasks: [a]\n')
        self.m.TEMPLATES = json.loads((OPS / 'executor-templates.json').read_text())
        self.rows([])
        run = self.patch('run_teed', return_value=0)
        self.patch('execution_row', return_value={'verdict': 'pass'})
        rc, _ = self.m.run_spec('s.yaml', env={})
        self.assertEqual(rc, 0)
        self.assertEqual(run.call_args.args[0], [str(OPS / 'executor')])
        order = json.loads(run.call_args.kwargs['input'])
        self.assertEqual(order['executor'], 'codex:gpt-6.2-sol')
        template = _executor.executor_template(self.m.TEMPLATES, order['executor'])
        binary = template['binary']
        args = _executor.cli_args(template, self.root, '4', 'test prompt', 'test goal')
        self.assertEqual([binary, *args[:3]], ['codex', 'exec', '--model', 'gpt-6.2-sol'])
        self.assertEqual(args[-1], 'test prompt')
        self.assertNotIn('--model', self.m.TEMPLATES['codex']['args'])
        self.assertIsNone(_executor.executor_template(self.m.TEMPLATES, 'codex:unknown'))

    def test_spec_missing_row_reports_stderr_and_pending_finalization_withholds_completion(self):
        self.prepare_spec('id: s\nmodel: runner\n')
        self.patch('one_trial', return_value=0)
        self.patch('execution_row', return_value={})
        pending=self.patch('pending_row', return_value={})
        self.m.TRIAL_STDERR[:]=['first\nlast']
        rc,row=self.m.run_spec('s.yaml')
        self.assertEqual(rc,1)
        self.assertIn('first | last',row['stop'])
        pending.return_value={'verdict':'void','stop':'finalization_pending'}
        completed=Mock()
        self.assertEqual(self.m.run_spec('s.yaml',completion=completed)[0],3)
        completed.assert_not_called()
        pending.return_value={}
        with patch.object(self.m,'execution_row',side_effect=[{}, {'verdict':'pass'}]):
            self.assertEqual(self.m.run_spec('s.yaml')[0],0)

    def test_spec_policy_refusal_and_config_void_hold_schedule_and_launch_nothing_more(self):
        self.prepare_spec()
        hold=self.patch('hold_schedule')
        run=self.patch('one_executor_trial', return_value=1)
        with patch.object(self.m,'resolve',side_effect=self.m.EnvPolicyError('denied')):
            rc,row=self.m.run_spec('s.yaml',schedule_id='scheduled')
        self.assertEqual(row['error']['code'],'env_policy')
        run.assert_not_called()
        self.patch('execution_row', return_value={'verdict':'void','stop':'env_policy: denied'})
        with patch.object(self.m,'is_config_void',return_value=True):
            self.assertEqual(self.m.run_spec('s.yaml',schedule_id='scheduled')[0],1)
        self.assertEqual(run.call_count,1)
        self.assertEqual(hold.call_count,2)

    def test_spec_cancel_before_trial_launches_nothing_and_executor_requires_task(self):
        self.prepare_spec('id: s\nmodel: fake\n')
        with patch.object(self.m,'cancelled',return_value=True):
            self.assertEqual(self.m.run_spec('s.yaml'),(1,{}))
        with self.assertRaisesRegex(RuntimeError,'needs tasks'):
            self.m.run_spec('s.yaml')

    def test_ledger_selects_only_new_matching_terminal_trial_and_parses_spec_fields(self):
        self.assertEqual(self.m.ledger_size(),0)
        self.assertEqual(self.m.execution_row('s','s'),{})
        self.m.LEDGER.write_text('\n'+json.dumps({'spec':'s','trial':1,'verdict':'pass'})+'\n'+json.dumps({'spec':'other','trial':2,'verdict':'fail'})+'\n')
        self.assertEqual(self.m.ledger_size(),2)
        self.assertEqual(self.m.execution_row('specs/s.yaml','s')['trial'],1)
        self.assertEqual(self.m.execution_row('s','s',after=1),{})
        self.assertEqual(self.m.spec_tasks('tasks: [one, two]'),['one','two'])
        with self.assertRaises(ValueError):
            self.m.spec_tasks('tasks: [1]')
        self.assertEqual(self.m.spec_seconds('seconds: 12'),12)
        self.assertEqual(self.m.spec_seconds(''),300)
        self.assertFalse(self.m.local_model({}))
        (self.root/'local.yaml').write_text('model: local-coder\n')
        self.assertEqual(self.m.local_model({'spec':'local.yaml'}),'local-coder')

    def test_trial_builders_bind_task_trial_environment_and_exact_pending_authority(self):
        shared = {'task': 'task', 'spec': 's.yaml'}
        wrong_authority = {'grant': {'id': 'other'}, 'conversation_id': 'other', 'turn_id': 'other'}
        self.rows([
            {**shared, **wrong_authority, 'id': 'sibling', 'state': 'pending'},
            {**shared, **wrong_authority, 'id': 'j', 'state': 'running'},
            {**shared, 'id': 'j', 'state': 'pending', 'grant': {'id': 'g'},
             'conversation_id': 'c', 'turn_id': 't'},
        ])
        run = self.patch('run_teed', return_value=7)
        effects = ['youtube_playlist_mutation']
        self.assertEqual(self.m.one_trial('s.yaml', 2, 'task', 'j', effects), 7)
        self.assertEqual(run.call_args.args[0],
                         [str(OPS / 'run-api'), '-task', 'task', '-trial', '2', 's.yaml'])
        env = run.call_args.kwargs['env']
        self.assertEqual(env['CS_JOB_ID'], 'j')
        self.assertEqual(env['CS_TRIAL'], '2')
        self.assertEqual(env['CS_VIDEO_MUTATION'], '1')

        self.assertEqual(self.m.one_executor_trial('s.yaml', 'model: fake\nintent: hello\n',
                                                   'task', 1, 'j', effects), 7)
        self.assertEqual(run.call_args.args[0], [str(OPS / 'executor')])
        env = run.call_args.kwargs['env']
        self.assertEqual(env['CS_JOB_ID'], 'j')
        self.assertEqual(env['CS_VIDEO_MUTATION'], '1')
        self.assertNotIn('CS_TRIAL', env)
        self.assertEqual(json.loads(run.call_args.kwargs['input']), {
            **shared, 'executor': 'fake', 'trial': 1, 'timeout_s': 300,
            'conversation_id': 'c', 'turn_id': 't', 'grant': {'id': 'g'},
        })

    def test_teed_process_returns_status_retains_stderr_and_appends_job_log(self):
        log=self.root/'node.log'
        def run(cmd,stderr,**kw):
            os.write(stderr,b'child detail\n')
            return subprocess.CompletedProcess(cmd,4)
        with patch.object(sys,'stderr',io.TextIOWrapper(io.BytesIO())):
            self.assertEqual(self.m.run_teed(['fake'],log_path=log,run=run),4)
        self.assertEqual(self.m.TRIAL_STDERR,['child detail\n'])
        self.assertIn('child detail',log.read_text())
        self.m.TRIAL_STDERR[:]=['']
        self.assertEqual(self.m.no_row_reason(9),'exit 9, no stderr')

    def test_refresh_preflights_scheduled_setup_and_returns_updated_head(self):
        call=self.patch('call_op',return_value={'setup':'prepared','head':'new'})
        self.assertEqual(self.m.refresh_job({}),{})
        job={'task':'t','resource':'r','schedule_id':'s'}
        self.assertEqual(self.m.refresh_job(job)['expected_head'],'new')
        self.assertEqual([c.args[1] for c in call.call_args_list],['setup-preflight','setup-refresh','setup-preflight'])
        self.assertNotIn('expected_head',job)

    def test_resource_lock_contention_and_emit_include_only_active_side_pass_results(self):
        self.assertIsNone(self.m.lock_resource(''))
        held=self.m.lock_resource('r')
        try:
            self.assertFalse(self.m.lock_resource('r'))
        finally:
            self.m.unlock_resource(held)
        self.m.unlock_resource(None)
        self.m.FINALIZE_SWEEP.update(attempted=1)
        self.m.SPOOL.update(sent=1)
        self.m.JOB_PULL.update(accepted=1)
        with patch.object(sys,'stdout',io.StringIO()) as out:
            self.m.emit({'ok':True},['lock'])
        row=json.loads(out.getvalue())
        self.assertEqual(row['stale_locks'],1)
        self.assertEqual(row['spool']['sent'],1)
        self.assertEqual(row['job_pull']['accepted'],1)
