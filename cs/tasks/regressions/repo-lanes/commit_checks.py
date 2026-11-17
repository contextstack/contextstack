"""Distinguish Git's nonfatal AUTO_MERGE cleanup from a lane/maintenance failure."""
import errno
import json
import os
import subprocess
from pathlib import Path
from unittest.mock import patch


def accepted(probe):
    if (probe.get('agent_commit') != 'ok' or probe.get('packed_unchanged') is not True
            or probe.get('maintenance_config') != ['false', '0']):
        return False
    events = [json.loads(line) for line in probe.get('commit_trace2', '').splitlines()]
    if (any(e.get('event') == 'child_start' for e in events)
            or not any(e.get('event') == 'exit' and e.get('code') == 0 for e in events)):
        return False
    stderr = probe.get('commit_stderr', '')
    if not stderr:
        return True
    # Git's files backend locks packed-refs before checking whether a deleted
    # ref exists. sequencer_post_commit_cleanup unconditionally deletes AUTO_MERGE.
    # Accept only the independently reproduced cleanup error, AFTER the lane ref
    # transaction finished. Never accept maintenance or a failed lane update.
    trace = probe.get('commit_refs', '')
    success = trace.find('finish: 0')
    failure = trace.find('transaction_prepare: -1')
    return (stderr == probe.get('cleanup_stderr')
            and stderr == "error: Unable to create '%s/packed-refs.lock': %s\n" % (
                probe.get('common'), probe.get('lock_error'))
            and probe.get('cleanup_exit') == 1
            and probe.get('cleanup_absent') is True
            and 'refs/heads/lane/' in trace[:success]
            and 0 <= success < failure
            and trace.count('transaction_prepare: -1') == 1
            and trace.count('finish: 0') == 1)


def run(tmp, gwt, git, private_git, expect):
    for packed_lane in (False, True):
        _run(tmp, gwt, git, private_git, expect, packed_lane)


def _run(tmp, gwt, git, private_git, expect, packed_lane):
    """Reproduce even when nested Seatbelt is unavailable: common dir is read-only."""
    main = gwt(tmp / ('commit-cleanup-packed' if packed_lane else 'commit-cleanup-loose'))
    lane = main.parent / 'worker'
    git(main, 'worktree', 'add', '-q', '-b', 'lane/worker', str(lane))
    common = main.parent / '.bare'
    if packed_lane:
        git(common, 'pack-refs', '--all')
    from _lanes import prepare_lane_refs
    prepare_lane_refs(main, 'worker', Path(git(lane, 'rev-parse', '--absolute-git-dir')))
    snapshot = (common / 'packed-refs').read_bytes()
    traces = {name: main.parent / ('cleanup-' + name) for name in ('trace', 'refs', 'trace2')}
    from _executor import allowlist_env
    hook = dict(GIT_DIR=str(common), GIT_WORK_TREE=str(main), GIT_PREFIX='', GIT_EDITOR=':',
                GIT_INDEX_FILE=str(tmp / 'hook.index'), GIT_CONFIG_COUNT='2',
                GIT_CONFIG_KEY_0='maintenance.auto', GIT_CONFIG_VALUE_0='true',
                GIT_CONFIG_KEY_1='gc.auto', GIT_CONFIG_VALUE_1='1',
                GIT_CONFIG_PARAMETERS="'maintenance.auto'='true' 'gc.auto'='1'")
    with patch.dict(os.environ, hook):
        child = allowlist_env([], tmp / 'agent-tmp', lane)
    expect(not any(key in child for key in hook), 'executor drops inherited hook Git controls')
    command_env = {**child, **private_git}
    settings = [subprocess.check_output(['git', '-C', str(lane), 'config', '--get', key],
                                       env=command_env, text=True).strip()
                for key in ('maintenance.auto', 'gc.auto')]
    absent = all(subprocess.run(['git', '-C', str(lane), 'rev-parse', '-q', '--verify', ref],
                                env=command_env, capture_output=True).returncode == 1
                 for ref in ('AUTO_MERGE', 'CHERRY_PICK_HEAD', 'REVERT_HEAD'))
    mode = common.stat().st_mode
    common.chmod(0o555)
    try:
        cleanup = subprocess.run(['git', '-C', str(lane), 'update-ref', '-d', 'AUTO_MERGE'],
                                 env=command_env, capture_output=True, text=True)
        result = subprocess.run(['git', '-C', str(lane), '-c', 'core.hooksPath=/dev/null',
                                 'commit', '--allow-empty', '-qm', 'packed lane commit'],
                                env={**command_env, 'GIT_TRACE': str(traces['trace']),
                                     'GIT_TRACE_REFS': str(traces['refs']),
                                     'GIT_TRACE2_EVENT': str(traces['trace2'])},
                                capture_output=True, text=True)
    finally:
        common.chmod(mode)
    probe = dict(agent_commit='ok' if result.returncode == 0 else 'failed',
                 commit_stderr=result.stderr, cleanup_stderr=cleanup.stderr,
                 cleanup_exit=cleanup.returncode, cleanup_absent=absent, common=str(common),
                 maintenance_config=settings,
                 lock_error=os.strerror(errno.EACCES), packed_unchanged=(common / 'packed-refs').read_bytes() == snapshot,
                 commit_refs=traces['refs'].read_text(), commit_trace2=traces['trace2'].read_text())
    expect(accepted(probe), ('packed' if packed_lane else 'loose') + ' lane commits with shared ref writes denied',
           (probe, traces['trace'].read_text()))
    expect(git(lane, 'log', '-1', '--format=%s') == 'packed lane commit',
           'read-only common dir still permits the lane ref transaction')
    if result.stderr:
        expect(not accepted({**probe, 'cleanup_stderr': ''})
               and not accepted({**probe, 'commit_refs': ''})
               and not accepted({**probe, 'commit_stderr': result.stderr + 'other error\n'})
               and not accepted({**probe, 'packed_unchanged': False})
               and not accepted({**probe, 'maintenance_config': ['true', '1']})
               and not accepted({**probe, 'commit_trace2': probe['commit_trace2']
                                 + json.dumps(dict(event='child_start', argv=['git', 'maintenance'])) + '\n'}),
               'cleanup exception rejects unrelated errors, missing proof and shared ref changes')
        print('Git cleanup evidence:', git(main, '--version'), result.stderr.strip(),
              traces['trace'].read_text(),
              '\n'.join(line for line in probe['commit_refs'].splitlines()
                        if any(word in line for word in ('transaction', 'finish:', 'AUTO_MERGE', 'refs/heads/lane/'))),
              '\n'.join(line for line in probe['commit_trace2'].splitlines()
                        if json.loads(line).get('event') in ('error', 'child_start', 'exit')), sep='\n')
