"""The native renderer refuses every transport failure before returning a command."""
from pathlib import Path
import subprocess
import os
import sys
import tempfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(ROOT / 'cs/ops/bin'))
import _estate_io as io
import _executor as executor
import _muse as muse
import _cli_adapter as adapter
import _lead_host as lead


def check():
    with tempfile.TemporaryDirectory(dir=ROOT / 'target', prefix='native-switch-') as raw:
        root = Path(raw)
        code_root = root / 'cs'
        code_root.mkdir()
        binary = root / 'bin/cs-sandbox'
        binary.parent.mkdir()
        binary.write_bytes(b'fixture')
        assert io.native_sandbox_binary(code_root, env={}) == binary
        assert io.native_sandbox_binary(code_root, env={'CS_SANDBOX_BIN': '/missing'}) == Path('/missing')
        binary.unlink()
        assert io.native_sandbox_binary(code_root, env={}) == root / 'target/release/cs-sandbox'
        import _toolchains
        with patch.object(_toolchains, 'discover', return_value=()), \
                patch.object(_toolchains, 'discover_sandbox_links', return_value=()), \
                patch.object(_toolchains, 'has_cargo_toolchain', return_value=False), \
                patch.dict(os.environ, {'PATH': '/ambient', 'SSH_AUTH_SOCK': '/ambient.sock'}):
            explicit = io.native_sandbox_request('/usr/bin/true', root, root, root,
                                                  env={'PATH': '/run/bin', 'SSH_AUTH_SOCK': '/run.sock'})
            assert explicit['lookup_path'] == ['/run/bin']
            assert explicit['ssh_auth_sock'] == '/run.sock'
        policy = root / 'policy.json'
        request = {'version': 1}
        failures = [FileNotFoundError('missing'), subprocess.TimeoutExpired('render', 10),
                    subprocess.CompletedProcess([], 101, b'', b'refused'),
                    subprocess.CompletedProcess([], 0, b'wrong header', b'')]
        for failure in failures:
            options = {'side_effect': failure} if isinstance(failure, Exception) else {'return_value': failure}
            with patch.object(io.subprocess, 'run', **options), patch.object(sys, 'platform', 'darwin'), \
                    patch.object(io, 'native_sandbox_request', return_value=request):
                profile = root / 'profile.sb'
                try:
                    executor.sandbox_cmd('/usr/bin/true', [], root, root, profile, policy=policy, env={})
                except RuntimeError:
                    pass
                else:
                    raise AssertionError('renderer failure returned an executor command')
                assert not profile.exists()
        ws, home = root / 'workspace', root / 'owner'
        ws.mkdir()
        home.mkdir()
        for failure in failures:
            options = {'side_effect': failure} if isinstance(failure, Exception) else {'return_value': failure}
            with patch.object(io.subprocess, 'run', **options), patch.object(sys, 'platform', 'darwin'), \
                    patch.object(Path, 'is_file', return_value=True), \
                    patch.object(io, 'native_sandbox_request', return_value=dict(request)), \
                    patch.object(lead, '_load_sandbox_cmd', return_value=executor.sandbox_cmd):
                routes = [
                    lambda: muse.sandbox_command(['/usr/bin/true'], ws, home),
                    lambda: adapter.command({'auth': 'none'}, '/usr/bin/true', ws, root, home, [], []),
                    lambda: lead.sandbox_builder(ws, root, root / 'lead.sb', write_roots=[root / 'runtime'])('/usr/bin/true', []),
                ]
                for route in routes:
                    try:
                        route()
                    except RuntimeError:
                        pass
                    else:
                        raise AssertionError('switched route returned a command after renderer failure')
                assert not (root / 'lead.sb').exists()
        # The executor renders against the HOST env, like the Python renderer: the child env strips
        # SSH_AUTH_SOCK, and the host's real agent socket must keep its explicit deny.
        seen = {}
        with patch.object(sys, 'platform', 'darwin'), \
                patch.object(io, 'native_sandbox_request', side_effect=lambda *a, **k: seen.update(env=dict(k['env'])) or request), \
                patch.object(io, 'render_native_sandbox', return_value=b'(version 1)\n'), \
                patch.dict(os.environ, {'PATH': '/host/bin', 'SSH_AUTH_SOCK': '/host-agent.sock'}):
            executor.sandbox_cmd('/usr/bin/true', [], root, root, root / 'host.sb', policy=policy)
        assert seen['env'].get('SSH_AUTH_SOCK') == '/host-agent.sock', 'host ssh-agent socket lost'
        assert seen['env'].get('PATH') == '/host/bin', 'interpreter lookup must use the host PATH'
        # A symlinked launcher (~/.local/bin/claude -> versions/X) must reach Rust unresolved, so its
        # hops get read grants; resolving it here broke every real CLI (sandbox-exec execvp ENOENT).
        real = root / 'share/versions/1/tool'
        real.parent.mkdir(parents=True)
        real.write_text('#!/bin/sh\n')
        link = root / 'local-bin/tool'
        link.parent.mkdir()
        link.symlink_to(real)
        built = io.sandbox_request(workspace=root, tmpdir=root, home=root, executable=link,
                                   lookup_path=[link.parent], ssh_auth_sock=link)
        assert built['executable'] == str(link), 'launcher symlink resolved before Rust saw it'
        assert built['lookup_path'] == [str(link.parent)]
        assert built['ssh_auth_sock'] == str(link)
        # Every switched caller sends the host inputs and unresolved symlink paths.
        captured = []
        labs = []
        def capture(code_root, policy, body):
            captured.append(dict(body))
            labs.append(code_root)
            return b'(version 1)\n'
        original_is_file = Path.is_file
        def fixture_is_file(path):
            return str(path) == '/usr/bin/sandbox-exec' or original_is_file(path)
        with patch.object(sys, 'platform', 'darwin'), \
                patch.object(Path, 'is_file', fixture_is_file), \
                patch.object(executor, 'HOME', home), \
                patch.object(lead, '_load_sandbox_cmd', return_value=executor.sandbox_cmd), \
                patch.object(_toolchains, 'discover', return_value=()), \
                patch.object(_toolchains, 'discover_sandbox_links', return_value=()), \
                patch.object(_toolchains, 'has_cargo_toolchain', return_value=False), \
                patch.object(io, 'render_native_sandbox', side_effect=capture), \
                patch.dict(os.environ, {'PATH': str(link.parent), 'SSH_AUTH_SOCK': str(link)}):
            muse.sandbox_command([str(link)], ws, home)
            adapter.command({'auth': 'none'}, link, ws, root, home, [], [])
            lead.sandbox_builder(ws, root, root / 'lead-inputs.sb', write_roots=[root / 'runtime'])(str(link), [])
            import _muse_adapter
            _muse_adapter.check_command(link, ws, home)
            import importlib.util
            frozen_file = root / 'stimulus/executor/ops/bin/_muse.py'
            frozen_file.parent.mkdir(parents=True)
            frozen_file.write_bytes((ROOT / 'cs/ops/bin/_muse.py').read_bytes())
            spec = importlib.util.spec_from_file_location('frozen_muse', frozen_file)
            frozen = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(frozen)
            frozen.sandbox_command([str(link)], ws, home)
        assert len(captured) == 5
        assert all(code_root == io.sandbox_runtime_code_root() for code_root in labs), 'frozen Muse located a different runtime'
        for body in captured:
            assert body['executable'] == str(link)
            assert body['lookup_path'] == [str(link.parent)]
            assert body['ssh_auth_sock'] == str(link)
        result = subprocess.CompletedProcess([], 0, b'(version 1)\nbytes\n', b'')
        with patch.object(io.subprocess, 'run', return_value=result) as run:
            assert io.render_sandbox_request(binary, policy, request) == result.stdout
            assert run.call_args.args[0] == [str(binary), '--policy', str(policy), '--profile', 'cli-executor', '--request', '-']
            assert run.call_args.kwargs['timeout'] == 10
    print('e2-native-switch: fail-closed transport and release resolution PASS')


check()
