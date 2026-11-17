"""A real commit triggers shared ref packing, unless agent Git disables maintenance."""
import hashlib
import subprocess


def seed(main, git, env):
    """Trigger modern ref maintenance and older Git's gc --auto with small fixtures."""
    for n in range(100):
        git(main, 'update-ref', 'refs/heads/lane/fixture-%d' % n, 'HEAD')
    git(main, 'config', 'gc.auto', '1')
    git(main, 'config', 'gc.autoDetach', 'false')
    sampled = 0
    for n in range(10000):
        data = str(n).encode()
        if hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest().startswith('17'):
            subprocess.run(['git', '-C', str(main), 'hash-object', '-w', '--stdin'],
                           input=data, capture_output=True, check=True, env=env)
            sampled += 1
            if sampled == 3:
                return
    raise AssertionError('could not seed auto-gc loose-object sample')


def run(tmp, gwt, git, env, private_git, expect):
    main = gwt(tmp / 'maintenance')
    lane = main.parent / 'worker'
    git(main, 'worktree', 'add', '-q', '-b', 'lane/worker', str(lane))
    # Exercise command-scope overrides even when the repository enables maintenance.
    git(main, 'config', 'maintenance.auto', 'true')

    def loose_refs():
        head = git(lane, 'rev-parse', 'HEAD')
        for n in range(100):
            git(main, 'update-ref', 'refs/heads/lane/fixture-%d' % n, head)

    def commit(extra):
        return subprocess.run(['git', '-C', str(lane), '-c', 'core.hooksPath=/dev/null',
                               'commit', '--allow-empty', '-qm', 'maintenance fixture'],
                              env={**env, **extra, 'GIT_TRACE': '1'}, capture_output=True, text=True)

    seed(main, git, env)
    before = commit({})
    expect(before.returncode == 0 and 'pack-refs --all --prune' in before.stderr,
           'real lane commit reproduces automatic packing of shared refs', before.stderr)
    loose_refs()
    packed = main.parent / '.bare/packed-refs'
    snapshot = packed.read_bytes()
    after = commit(private_git)
    expect(after.returncode == 0 and 'maintenance run' not in after.stderr
           and 'pack-refs' not in after.stderr and packed.read_bytes() == snapshot,
           'agent Git commit skips automatic shared maintenance despite repo settings', after.stderr)
    expect(git(lane, 'log', '-1', '--format=%s') == 'maintenance fixture',
           'maintenance suppression still creates the lane commit')
