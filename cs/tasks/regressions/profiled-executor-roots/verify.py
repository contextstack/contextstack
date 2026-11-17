"""A profiled template (opencode, free_local) through the executor with separate roots, as on an installed node."""
import hashlib, json, os, shutil, sqlite3, subprocess, sys, tempfile, time
from contextlib import contextmanager
from pathlib import Path
sys.path.insert(0, str(next(p / "tasks/regressions/_lib" for p in Path(__file__).resolve().parents if (p / "tasks/regressions/_lib").is_dir())))  # frozen copies too
from hermetic_env import hermetic_env

_fixture_env = hermetic_env()
os.environ.clear()
os.environ.update(_fixture_env)

SOURCE = Path(__file__).resolve().parents[3]
def isolated_env(home):
    env = {k: v for k, v in hermetic_env().items() if not k.startswith(('LAB_', 'GIT_', 'CSGATEWAY_'))}
    return {**env, 'HOME': str(home), 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1',
            'GIT_AUTHOR_NAME': 'fixture', 'GIT_AUTHOR_EMAIL': 'fixture@local',
            'GIT_COMMITTER_NAME': 'fixture', 'GIT_COMMITTER_EMAIL': 'fixture@local'}


def check_harvest_lock():
    # Exercise the real lock hook only in an independent scratch repo, never the checkout's common dir.
    # root sits one level down so the host-owned harvest repo beside it is removed with the scratch dir.
    with tempfile.TemporaryDirectory(prefix='harvest-lock-') as raw:
        root = Path(raw).resolve() / 'repo'; root.mkdir(); env = isolated_env(root)
        def git(*args, check=True):
            return subprocess.run(['git', '-C', str(root), *args], env=env, text=True,
                                  capture_output=True, check=check)
        git('init', '-q', '-b', 'main')
        common = Path(git('rev-parse', '--path-format=absolute', '--git-common-dir').stdout.strip())
        assert common == root / '.git', common
        hooks = root / 'hooks'; hooks.mkdir()
        shutil.copy2(SOURCE.parent / 'scripts/git-hooks/pre-commit', hooks / 'pre-commit')
        git('config', 'core.hooksPath', str(hooks))
        sleeper = subprocess.Popen(['sleep', '60'])
        try:
            (common / 'cs-land.lock').write_text(str(sleeper.pid) + '\n')
            (root / 'answer.txt').write_text('done\n')
            git('add', 'answer.txt')
            blocked = git('commit', '-qm', 'must refuse', check=False)
            assert blocked.returncode and 'cs land is running' in blocked.stderr, blocked
            harvested = subprocess.run([str(SOURCE / 'ops/bin/harvest')], env=env, text=True,
                                       capture_output=True, input=json.dumps({
                                           'workspace': str(root), 'slug': 'lock', 'run_id': 'fixture'}))
            assert harvested.returncode == 0, (harvested.stdout, harvested.stderr)
            # Harvest commits into its host-owned repo beside the workspace, never the workspace's .git.
            sys.path.insert(0, str(SOURCE / 'ops/bin'))
            from _harvest_git import git as harvest_git
            logged = harvest_git(root, 'log', '-1', '--format=%s')
            assert logged.stdout.strip() == 'lab: lock (fixture)', (logged.stdout, logged.stderr)
            assert git('rev-parse', '-q', '--verify', 'HEAD', check=False).returncode != 0, 'harvest wrote the workspace .git'
            # Plain setup directories nested under a repo must be copied, never discovered as that repo.
            sys.path.insert(0, str(SOURCE / 'ops/bin'))
            from _setup_workspace import materialize
            setup = root / 'setup'; setup.mkdir(); (setup / 'input.txt').write_text('fixture')
            workspace = root / 'workspace'
            materialize(setup, workspace, lab=root, env=env)
            assert not (workspace / '.git').exists(), 'plain setup inherited its containing repo'
            assert (workspace / 'input.txt').read_text() == 'fixture'
        finally:
            sleeper.terminate(); sleeper.wait()
    print('harvest: live scratch cs-land lock blocks ordinary commits but permits evidence commits; setup discovery isolated')


def check_private_db_staging():
    sys.path.insert(0, str(SOURCE / 'ops/bin'))
    from _cli_adapter import stage_opencode_db
    with tempfile.TemporaryDirectory(prefix='opencode-staging-') as raw:
        root = Path(raw).resolve(); home = root / 'personal'; staged = root / 'private'
        db = home / '.local/share/opencode/opencode.db'; db.parent.mkdir(parents=True)
        with sqlite3.connect(db) as conn:
            conn.execute('CREATE TABLE credential (provider_id TEXT, token TEXT)')
            conn.executemany('INSERT INTO credential VALUES (?, ?)', [
                ('opencode-go', 'go-fixture'), ('opencode', 'other-fixture'),
                ('anthropic', 'foreign-fixture'), (None, 'null-fixture')])
            conn.execute('CREATE TABLE other_auth (token TEXT)')
            conn.execute("INSERT INTO other_auth VALUES ('unrelated-fixture')")
        before = db.read_bytes()  # Synthetic fixture only.
        dest = stage_opencode_db(home, staged)
        with sqlite3.connect(dest) as conn:
            assert conn.execute('SELECT * FROM credential').fetchall() == [('opencode-go', 'go-fixture')]
            # Every other table is copied as schema only: no unrelated credential or history rows.
            assert conn.execute('SELECT COUNT(*) FROM other_auth').fetchone() == (0,), 'unrelated rows copied'
        assert db.read_bytes() == before, 'source fixture changed'
        assert dest.stat().st_mode & 0o777 == 0o600
        # OpenCode 2.3 names the service in integration_id (its id column is a row id).
        db.unlink()
        with sqlite3.connect(db) as conn:
            conn.execute('CREATE TABLE credential (id TEXT, integration_id TEXT, label TEXT, value TEXT, active INTEGER)')
            conn.execute('CREATE TABLE session (id TEXT, title TEXT)')
            conn.execute("INSERT INTO session VALUES ('ses_1', 'private chat')")
            conn.execute('CREATE TABLE __drizzle_migrations (id INTEGER, hash TEXT)')
            conn.execute("INSERT INTO __drizzle_migrations VALUES (1, 'm1')")
            conn.executemany('INSERT INTO credential VALUES (?, ?, ?, ?, ?)', [
                ('cr1', 'opencode-go', 'OpenCode Go', 'go-fixture', 1), ('cr2', 'opencode', 'Default', 'other-fixture', 1),
                ('cr3', 'openrouter', 'API key', 'foreign-fixture', None)])
        dest = stage_opencode_db(home, staged)
        with sqlite3.connect(dest) as conn:
            assert conn.execute('SELECT integration_id, value FROM credential').fetchall() == [('opencode-go', 'go-fixture')]
            # OpenCode 2.3 refuses a non-empty store without its session table: schema and migration history come
            # across, session rows do not.
            assert conn.execute('SELECT COUNT(*) FROM session').fetchone() == (0,), 'session rows copied'
            assert conn.execute('SELECT * FROM __drizzle_migrations').fetchall() == [(1, 'm1')], 'migration history missing'
        # Unknown schema, malformed databases and absent subscription login fail closed.
        for mode in ('schema', 'malformed', 'no-go'):
            db.unlink()
            if mode == 'malformed':
                db.write_text('not-a-database')
            else:
                with sqlite3.connect(db) as conn:
                    conn.execute('CREATE TABLE credential (%s TEXT, token TEXT)' % ('unknown' if mode == 'schema' else 'provider_id'))
                    conn.execute("INSERT INTO credential VALUES ('anthropic', 'foreign-fixture')")
            try:
                stage_opencode_db(home, staged)
            except ValueError:
                pass
            else:
                raise AssertionError('unsupported login accepted: ' + mode)
            assert not dest.exists(), 'failed staging left credential store behind'
    print('OpenCode staging: only subscription rows, fresh database, unsupported inputs refused')


def check_keychain_profiles():
    # Profile generation is testable without launching a nested Seatbelt sandbox.
    from unittest.mock import patch
    sys.path.insert(0, str(SOURCE / 'ops/bin'))
    from _cli_adapter import command
    from _muse import sandbox_command
    with tempfile.TemporaryDirectory(prefix='profiled-keychain-') as raw:
        root = Path(raw).resolve(); home = root / 'personal'; home.mkdir()
        ws = root / 'workspace'; ws.mkdir()
        keychains = home / 'Library/Keychains'; keychains.mkdir(parents=True)
        with patch.object(sys, 'platform', 'darwin'), patch.object(Path, 'is_file', return_value=True):
            muse = sandbox_command(['fixture'], ws, home)[2]
            assert '(allow file-read* (subpath ' + json.dumps(str(keychains)) + '))' not in muse
            profiles = json.loads((SOURCE / 'ops/bin/cli-profiles.json').read_text())
            for name, profile in profiles.items():
                if profile.get('auth') == 'opencode_db':
                    continue  # Database staging is exercised separately above.
                text = command(profile, Path('/usr/bin/true'), ws, root / 'private', home, (), [])[2]
                deny = '(deny file-read* (subpath %s))' % json.dumps(str(keychains))
                assert deny in text, name
                assert not any(line.startswith('(allow file-read') and 'Keychains' in line
                               for line in text[text.rindex(deny):].splitlines()), name
    print('profiled roots: Muse Keychain scope narrowed; other profiles retain final Keychain denial')


check_private_db_staging()
check_harvest_lock()
check_keychain_profiles()
if sys.platform != 'darwin' or '--offline' in sys.argv:
    print('profiled executor fixtures need the macOS sandbox; skipped')
    raise SystemExit(0)
OP = [{'type': 'step_start', 'sessionID': 's', 'part': {'type': 'step-start'}},
      {'type': 'step_finish', 'sessionID': 's', 'part': {'type': 'step-finish', 'reason': 'stop'}}]


def listing(path):
    return {str(f.relative_to(path)) for f in path.rglob('*') if '__pycache__' not in f.parts}


@contextmanager
def failure_diagnostics(root):
    try:
        yield
    except Exception:
        # Print before TemporaryDirectory removes the evidence, including failures after a passing run.
        for run in sorted((root / 'state/runs').glob('*')):
            print('\n=== ' + str(run) + ' ===', file=sys.stderr)
            log = run / 'cli.log'
            print('cli.log tail:\n' + ('\n'.join(log.read_text().splitlines()[-60:])
                  if log.is_file() else '(missing)'), file=sys.stderr)
            profile = run / 'sandbox.sb'
            print('sandbox.sb lane section:\n' + (profile.read_text()
                  if profile.is_file() else '(missing; launch may have failed before profile generation)'), file=sys.stderr)
        raise


with tempfile.TemporaryDirectory(prefix='profiled-executor-roots-') as raw, failure_diagnostics(Path(raw).resolve()):
    root = Path(raw).resolve()
    lab = root / 'release/cs'; ops = lab / 'ops/bin'; ops.mkdir(parents=True)
    for path in (SOURCE / 'ops/bin').iterdir():
        if path.is_file():
            shutil.copy2(path, ops / path.name)
    from native_runtime import stage_native_runtime
    stage_native_runtime(ops)
    (lab / 'ESTATE').write_text('dev\n')
    state, config, workspaces = root / 'state', root / 'config', root / 'workspaces'
    for d in (state, config, workspaces):
        d.mkdir()
    (config / 'ESTATE').write_text('dev\n')  # finalize-run reads it from the config root
    home = root / 'personal'; home.mkdir()

    binary = root / 'fake-opencode'
    binary.write_text('#!' + sys.executable + '\nOP=' + repr(OP) + '\n' + r'''import json,sys
from pathlib import Path
if '--version' in sys.argv:print('fixture');sys.exit()
Path('answer.txt').write_text('done\n')
for event in OP:print(json.dumps(event))
''')
    binary.chmod(0o755)
    profiles = json.loads((ops / 'cli-profiles.json').read_text())
    profiles['opencode'].update(binary=str(binary))
    (ops / 'cli-profiles.json').write_text(json.dumps(profiles))

    # A Director job's task: under state/jobs/<id>/tasks/, outside the code root (m4, 2026-11-11).
    task = state / 'jobs/job-1/tasks/smoke-opencode'; (task / 'setup').mkdir(parents=True)
    (task / 'goal.md').write_text('Write answer.txt containing done.\n')
    (task / 'check').write_text('#!/bin/sh\n[ "$(cat answer.txt)" = done ]\n'); (task / 'check').chmod(0o755)

    # Retain each launch's actual profile beside cli.log, before temporary evidence is cleaned up.
    with (ops / '_cli_adapter.py').open('a') as stream:
        stream.write("\n_original_command = command\ndef command(*args, **kwargs):\n"
                     "    cmd = _original_command(*args, **kwargs)\n"
                     "    home_name = args[3].name\n"
                     "    run_id = home_name.removeprefix('cli-home-') if home_name.startswith('cli-home-') else args[2].parent.name\n"
                     "    run = Path(" + repr(str(state / 'runs')) + ") / run_id\n"
                     "    (run / 'sandbox.sb').write_text(cmd[2])\n"
                     "    return cmd\n")

    before = listing(lab)
    env = isolated_env(home)
    # Bound discovery even for copied lab tools running from a nested fixture directory.
    subprocess.run(['git', '-C', str(root), 'init', '-q'], env=env, check=True)
    env.setdefault('CS_TOOLS_BIN', str(SOURCE.parent / 'target/release/cs-tools'))
    env.update(HOME=str(home), LAB_STATE_ROOT=str(state), LAB_CONFIG_ROOT=str(config), LAB_WORKSPACES_ROOT=str(workspaces))
    r = subprocess.run([sys.executable, str(ops / '_executor.py')], cwd=lab, env=env, text=True, capture_output=True,
                       input=json.dumps({'executor': 'opencode', 'task': str(task), 'exp': 'FIXTURE-ROOTS', 'timeout_s': 15}),
                       timeout=60)
    assert r.stdout.strip(), (r.returncode, r.stderr)
    out = json.loads(r.stdout.splitlines()[-1]); run_id = out['run_id']; run = state / 'runs' / run_id
    log = run / 'cli.log'
    assert r.returncode == 0 and out['verdict'] == 'pass' and out['stop'] == 'done', (
        out['stop'], r.stderr, log.read_text() if log.is_file() else '')
    rows = [json.loads(s) for s in (state / 'ledger.jsonl').read_text().splitlines()]
    assert len(rows) == 1, rows
    row = rows[0]
    assert row['task'] == str(task), row['task']
    assert row['transcript'] == 'ledger/runs/' + run_id, row['transcript']
    assert row['capture_state'] == 'complete' and row['evidence_sha'] and row['branch'], row
    evidence = json.loads((run / 'evidence/manifest.json').read_text())
    assert evidence['capture_state'] == 'complete' and [f['path'] for f in evidence['files']] == ['answer.txt'], evidence
    assert (workspaces / run_id / 'workspace/answer.txt').read_text() == 'done\n'
    assert (workspaces / run_id / 'workspace/.git').is_dir(), 'scratch workspace must own its git metadata'
    assert not (lab / 'ledger').exists(), 'a node run never lands under the code root'
    after = listing(lab)
    assert after == before, sorted(after ^ before)
    # Profiled repo lanes use pinned host git, preserve their diff and commit on the lane branch.
    sys.path.insert(0, str(ops))
    import _lanes
    main = home / 'repo/main'; main.mkdir(parents=True)
    git_env = {**env, 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_AUTHOR_NAME': 'fixture',
               'GIT_AUTHOR_EMAIL': 'fixture@local', 'GIT_COMMITTER_NAME': 'fixture',
               'GIT_COMMITTER_EMAIL': 'fixture@local'}

    def git(path, *args):
        return subprocess.check_output(['git', '-C', str(path), *args], env=git_env, text=True).strip()

    git(main, 'init', '-q', '-b', 'main')
    (main / 'answer.txt').write_text('before\n')
    (main / '.env.example').write_text('example-ok\n')
    (main / '.gitignore').write_text('.env*\n*.env\n*.env.*\n!.env.example\n')
    git(main, 'add', '-A'); git(main, 'commit', '-qm', 'seed')
    base = git(main, 'rev-parse', 'HEAD')
    repos = config / 'repos.json'; repos.write_text(json.dumps({'demo': {'path': str(main)}}))
    env.update(CS_REPOS_FILE=str(repos))
    os.environ['CS_REPOS_FILE'] = str(repos)
    sibling = _lanes.ensure_lane(_lanes.repo_config('demo'), 'sibling')
    lane_info = _lanes.ensure_lane(_lanes.repo_config('demo'), 'profiled')
    lane = Path(lane_info['path'])
    env_decoys = ('.env', '.env.local', '.env_prod', '.env-local', '.envrc', '.envrc.local', 'foo.env', 'x.env.bak')
    task.joinpath('setup').rmdir()
    (lab / 'specs').mkdir()
    spec = lab / 'specs/lane.yaml'
    spec.write_text('id: profiled-lane\nmodel: opencode\nrepo: demo\nlane: profiled\n')
    targets = [main / 'answer.txt', Path(sibling['path']) / 'answer.txt', main / '.git/config']
    # Probes run under the same lane restrictions as the agent, including protected staged config and .env denies.
    binary.write_text('#!' + sys.executable + '\nOP=' + repr(OP) + '\nTARGETS=' + repr(list(map(str, targets))) + '\nDECOYS=' + repr(list(env_decoys)) + '\nREAL_HOME=' + repr(str(home)) + '\n' + r'''import json,os,sqlite3,sys,subprocess
from pathlib import Path
probe = {}
for target in TARGETS + [os.environ['HOME'] + '/opencode.json']:
    try:
        with open(target, 'a') as f:f.write('ESCAPE')
        probe[target] = 'written'
    except OSError:probe[target] = 'denied'
for name in DECOYS:
    try:
        Path(name).read_text()
        probe['env:' + name] = 'read'
    except PermissionError:
        probe['env:' + name] = 'denied'
# Moving/linking the source or replacing a secret destination must be refused.
for name in DECOYS:
    for tool in ('mv', 'ln', 'cp'):
        dest = Path('escape-' + tool)
        result = subprocess.run(['/bin/' + tool, name, str(dest)], capture_output=True)
        assert result.returncode != 0, (tool, name, result.stderr)
        assert not dest.exists() or dest.read_bytes() == b'', (tool, name)
        dest.unlink(missing_ok=True)
    source = Path('rename-source')
    source.write_text('public')
    for tool in ('mv', 'ln', 'cp'):
        result = subprocess.run(['/bin/' + tool, str(source), name], capture_output=True)
        assert result.returncode != 0, ('replace', tool, name, result.stderr)
    source.unlink()
    try:
        Path(name).write_text('created-over')
    except PermissionError:
        pass
    else:
        raise AssertionError('secret overwrite allowed: ' + name)
fresh = Path('.lab-env-probe')
fresh.mkdir()
for name in ('.env', '.env.local'):
    target = fresh / name
    try:
        target.write_text('created')
    except PermissionError:
        pass
    else:
        raise AssertionError('secret create allowed: ' + str(target))
    source = fresh / 'rename-source'
    source.write_text('public')
    result = subprocess.run(['/bin/mv', str(source), str(target)], capture_output=True)
    assert result.returncode != 0, ('create via rename', name, result.stderr)
    source.unlink()
fresh.rmdir()
assert Path('.env.example').read_text() == 'example-ok\n'
Path(os.environ['TMPDIR'], 'probe-version.json' if '--version' in sys.argv else 'probe-run.json').write_text(json.dumps(probe))
if '--version' in sys.argv:print('fixture');sys.exit()
status = subprocess.run(['git', 'status', '--porcelain'], capture_output=True, text=True)
assert status.returncode == 0, (os.getcwd(), status.stdout, status.stderr)
if 'OPENCODE_DB' in os.environ:
    staged_db = Path(os.environ['OPENCODE_DB'])
    assert staged_db == Path(os.environ['HOME']) / 'data/opencode/opencode.db', staged_db
    real_db = Path(REAL_HOME) / '.local/share/opencode/opencode.db'
    for fn in (lambda: real_db.read_bytes(), lambda: real_db.write_text('tampered')):
        try: fn()
        except PermissionError: pass
        else: raise AssertionError('real opencode.db accessible inside sandbox')
    with sqlite3.connect(staged_db) as conn:
        rows = conn.execute('SELECT provider_id, token FROM credential ORDER BY provider_id').fetchall()
    assert rows == [('opencode-go', 'go-fixture-auth')], rows
    Path('go.txt').write_text('subscription lane\n')
Path('answer.txt').write_text('done\n')
for event in OP:print(json.dumps(event))
''')
    (task / 'check').write_text('#!' + sys.executable + '\nREAL_HOME=' + repr(str(home)) + '\nDECOYS=' + repr(list(env_decoys)) + '\nSURVIVOR=' + repr(str(root / 'check-survivor')) + '\n' + r'''import os,signal,subprocess,sys
from pathlib import Path
assert os.getpgrp() == os.getpid(), 'check must run in its own process group'
for fn in (lambda: Path('.env').read_text(), lambda: (Path(REAL_HOME) / 'check-escape').write_text('escaped')):
    try: fn()
    except PermissionError: pass
    else: raise AssertionError('check ran unconfined')
subprocess.Popen([sys.executable, '-c', 'import signal,time;from pathlib import Path;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(1.5);Path(' + repr(SURVIVOR) + ').write_text("survived")'])
assert Path('answer.txt').read_text() == 'done\n'
''')
    (task / 'check').chmod(0o755)
    # The smoke run pinned the previous fake binary. This script is a new fixture CLI, so first use pins it here.
    (config / 'cli-pins.json').unlink(missing_ok=True)
    for template in ('opencode', 'opencode-go'):
        for name in env_decoys:
            (lane / name).write_text('SECRET=1\n')
        profiles[template] = {**profiles['opencode']}
        (ops / 'cli-profiles.json').write_text(json.dumps(profiles))
        # Keep opencode-go's subscription auth path covered with a multi-provider SQLite fixture database.
        if template == 'opencode-go':
            db = home / '.local/share/opencode/opencode.db'; db.parent.mkdir(parents=True, exist_ok=True)
            with sqlite3.connect(db) as conn:
                conn.execute('CREATE TABLE credential (provider_id TEXT, token TEXT)')
                conn.executemany('INSERT INTO credential VALUES (?, ?)', [
                    ('opencode-go', 'go-fixture-auth'),
                    ('anthropic', 'foreign-fixture-auth'),
                ])
                conn.commit()
            profiles[template].update(auth='opencode_db', cost_class='subscription')
            (ops / 'cli-profiles.json').write_text(json.dumps(profiles))
        spec.write_text('id: profiled-lane\nmodel: %s\nrepo: demo\nlane: profiled\n' % template)
        r = subprocess.run([sys.executable, str(ops / '_executor.py')], cwd=lab, env=env, text=True,
                           capture_output=True, timeout=60, input=json.dumps({
                               'executor': template, 'task': str(task), 'spec': 'specs/lane.yaml',
                               'exp': 'FIXTURE-LANE', 'timeout_s': 15}))
        assert r.stdout.strip(), (r.returncode, r.stderr)
        out = json.loads(r.stdout.splitlines()[-1])
        run = state / 'runs' / out['run_id']
        assert r.returncode == 0 and out['verdict'] == 'pass', (out, r.stderr, (run / 'cli.log').read_text())
        recorded = json.loads((config / 'cli-pins.json').read_text())[template]
        assert recorded['sha256'] == hashlib.sha256(binary.read_bytes()).hexdigest(), recorded
        assert out['branch'] == 'lane/profiled' and out['lane']['path'] == str(lane), out
        assert git(lane, 'symbolic-ref', '--short', 'HEAD') == 'lane/profiled'
        assert out['lane']['commit'] == git(lane, 'rev-parse', 'HEAD')
        rows = [json.loads(line) for line in (state / 'ledger.jsonl').read_text().splitlines()]
        assert rows[-1]['lane'] == out['lane'] and rows[-1]['branch'] == out['branch']
        assert rows[-1].get('memory_cap_bytes', 0) > 0, rows[-1]
        if template == 'opencode':
            assert out['lane']['commit'] == git(lane, 'rev-parse', 'HEAD') != base
            evidence = json.loads((run / 'evidence/manifest.json').read_text())
            assert evidence['base']['commit'] == base and [f['path'] for f in evidence['files']] == ['answer.txt'], evidence
            diff = (run / 'evidence/candidate.diff').read_text()
            assert '-before' in diff and '+done' in diff, diff
        else:
            with sqlite3.connect(db) as conn:
                real_rows = sorted(conn.execute('SELECT provider_id, token FROM credential').fetchall())
            assert real_rows == [('anthropic', 'foreign-fixture-auth'), ('opencode-go', 'go-fixture-auth')], real_rows
            assert not list((lane / '.lab-tmp').glob('*/data/opencode/opencode.db*')), 'staged opencode.db left behind'
        assert git(main, 'rev-parse', 'HEAD') == base and (main / 'answer.txt').read_text() == 'before\n'
        assert not git(lane, 'diff', 'HEAD'), 'node committed the changes'
        probe_paths = list((lane / '.lab-tmp').glob('cli-home-*/tmp/probe-*.json'))
        assert len(probe_paths) == (2 if template == 'opencode' else 4), probe_paths
        for probe_path in probe_paths:
            probe = json.loads(probe_path.read_text())
            assert probe and set(probe.values()) == {'denied'}, probe
        profile = (run / 'sandbox.sb').read_text()
        for pat in (r'/\.env$', r'/\.env\.[^/]+$', r'/\.env_[^/]+$', r'/\.env-[^/]+$', r'/\.envrc[^/]*$', r'/[^/]+\.env$', r'/[^/]+\.env\.[^/]+$'):
            assert '(deny file-read* file-write* file-link (regex #"%s"))' % pat in profile, (template, pat)
        assert r'(allow file-read* (require-all (subpath %s) (regex #"/\.env\.example$")))' % json.dumps(str(lane.resolve())) in profile
        if template == 'opencode-go':
            assert str(db) not in [line for line in profile.splitlines() if line.startswith('(allow ')]
            assert '(deny file-read* file-write* (subpath %s))' % json.dumps(str(db.parent)) in profile
        for parent in (main / '.git').parents:
            if parent.is_relative_to(home):
                assert '(allow file-read-metadata (literal %s))' % json.dumps(str(parent)) in profile, parent
        from _estate_io import native_caller
        from sandbox_profile import render_profile
        caller, roots = native_caller({}, home, run, lane, run / 'tmp', dict(main=main, lane='profiled'))
        expected = render_profile(home, lane, run / 'tmp', caller=caller, read_roots=roots)
        without_lane = {key: value for key, value in caller.items() if key != 'lane'}
        baseline = render_profile(home, lane, run / 'tmp', caller=without_lane, read_roots=roots)
        lane_rules = [line for line in expected.splitlines() if line not in baseline.splitlines()]
        assert lane_rules
        for rule in lane_rules:
            assert rule in profile, rule
    time.sleep(1.8)
    assert not (root / 'check-survivor').exists(), 'check process group was not cleaned up on exit'
    # A failing task check must leave the branch head unchanged.
    head = git(lane, 'rev-parse', 'HEAD')
    assert git(lane, 'rev-list', '--count', base + '..HEAD') == '2', 'one commit per passing lane run'
    for name in env_decoys:  # refresh synthetic secrets for the failing-check run
        (lane / name).write_text('SECRET=1\n')
    (task / 'check').write_text('#!/bin/sh\nexit 1\n')
    r = subprocess.run([sys.executable, str(ops / '_executor.py')], cwd=lab, env=env, text=True,
                       capture_output=True, timeout=60, input=json.dumps({
                           'executor': 'opencode-go', 'task': str(task), 'spec': 'specs/lane.yaml',
                           'exp': 'FIXTURE-RED', 'timeout_s': 15}))
    out = json.loads(r.stdout.splitlines()[-1])
    assert r.returncode != 0 and out['stop'] == 'check', (out, r.stderr)
    assert git(lane, 'rev-parse', 'HEAD') == head and 'commit' not in out['lane'], out
    # Profiled templates enforce memory_cap_mb on run_process.
    binary.write_text('#!' + sys.executable + '\nimport sys,time\nif "--version" in sys.argv:print("fixture");sys.exit()\nbuf=bytearray(16*1024*1024)\ntime.sleep(6)\n')
    (config / 'cli-pins.json').unlink(missing_ok=True)
    (ops / 'cli-profiles.json').write_text(json.dumps(profiles))
    spec.write_text('id: profiled-lane\nmodel: opencode\nrepo: demo\nlane: profiled\nbudget:\n  memory_mb: 1\n')
    r = subprocess.run([sys.executable, str(ops / '_executor.py')], cwd=lab, env=env, text=True,
                       capture_output=True, timeout=60, input=json.dumps({
                           'executor': 'opencode', 'task': str(task), 'spec': 'specs/lane.yaml',
                           'exp': 'FIXTURE-MEMCAP', 'timeout_s': 15}))
    out = json.loads(r.stdout.splitlines()[-1])
    assert r.returncode != 0 and out['stop'] == 'memory_cap', (out, r.stderr)
    assert out.get('memory_cap_bytes') == 1024 * 1024 and out.get('peak_rss_bytes', 0) > 1024 * 1024, out
    print('profiled repo lanes: opencode and opencode-go edit, publish and commit; probes and runs deny outside writes')
print('profiled executor with separate roots: task outside the code root, row and complete evidence in the state root')
