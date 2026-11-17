"""Repository registration uses hermetic state and real disposable Git repositories."""
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import MagicMock, Mock, patch

HERE = Path(__file__).resolve().parent
CS = HERE.parents[2]
sys.path[:0] = [str(HERE.parent / '_lib'), str(CS / 'ops/bin'), str(CS / 'scripts')]
from hermetic_env import hermetic_env
from scratch import scratch_dir
import _repos


class ReposTests(unittest.TestCase):
    def setUp(self):
        temp = scratch_dir(prefix='repos-')
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        env = hermetic_env()
        env.update(HOME=str(self.root), CS_REPOS_FILE=str(self.root / 'config/repos.json'),
                   GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1')
        patched = patch.dict(os.environ, env, clear=True)
        patched.start()
        self.addCleanup(patched.stop)
        self.repo = self.root / 'project'
        self.repo.mkdir()
        self.git('init', '-q', '-b', 'main')
        self.git('-c', 'user.name=Test', '-c', 'user.email=test@example.test',
                 'commit', '-q', '--allow-empty', '-m', 'seed')

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.repo), *args], check=True, capture_output=True)

    def cli(self, *args, ok=True):
        result = subprocess.run([sys.executable, str(CS / 'scripts/cs'), 'repo', *args],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0 if ok else 2, result.stdout + result.stderr)
        return result

    def test_round_trip(self):
        self.assertIn('no extra gate command', self.cli('list').stdout)
        self.cli('add', str(self.repo))
        path = _repos.repos_file()
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(path.read_text()),
                         {'project': {'path': str(self.repo), 'branch': 'main'}})
        self.assertEqual(_repos.inventory(), [{'name': 'project', 'path': str(self.repo), 'branch': 'main'}])
        self.assertIn('project\t' + str(self.repo) + '\tmain', self.cli('list').stdout)
        self.cli('remove', 'project')
        self.assertEqual(json.loads(path.read_text()), {})
        self.git('branch', 'dev')
        self.cli('add', str(self.repo), '--name', 'custom', '--branch', 'dev', '--gate', 'test -f ready')
        self.assertEqual(json.loads(path.read_text())['custom']['gate'], 'test -f ready')
        self.assertEqual(_repos.inventory()[0]['branch'], 'dev')

    def test_refusals_preserve_config(self):
        self.cli('add', str(self.root), ok=False)
        self.cli('add', str(self.repo), '--branch', 'missing', ok=False)
        self.cli('add', str(self.repo), '--name', 'Bad Name', ok=False)
        self.cli('add', str(self.repo))
        before = _repos.repos_file().read_bytes()
        self.cli('add', str(self.repo), ok=False)
        self.cli('add', str(self.repo), '--name', 'alias', ok=False)
        other = self.root / 'other'
        subprocess.run(['git', 'clone', '-q', str(self.repo), str(other)], check=True)
        self.cli('add', str(other), '--name', 'project', ok=False)
        self.cli('remove', 'missing', ok=False)
        self.assertEqual(_repos.repos_file().read_bytes(), before)

    def test_mcp_list_repos_dispatches_to_director(self):
        import cs_mcp
        repos = {'repos': [{'node': 'mac', 'name': 'project', 'path': str(self.repo), 'branch': 'main'}]}
        director = Mock()
        director.get.return_value = repos
        tools = cs_mcp.Tools(director, 'mac', can_land=False)
        schema = cs_mcp.handle(tools, {'method': 'tools/list'})
        self.assertIn('list_repos', [row['name'] for row in schema['tools']])
        response = cs_mcp.handle(tools, {'method': 'tools/call',
                                      'params': {'name': 'list_repos', 'arguments': {}}})
        self.assertEqual(json.loads(response['content'][0]['text']), repos)
        self.assertFalse(response.get('isError', False))
        director.get.assert_called_once_with('repos')

    def test_init_hints_only_when_unregistered(self):
        import cs_init
        cs = MagicMock(CONFIG=self.root / 'config', ROOT=self.root, CURRENT=self.root / 'current')
        init = MagicMock()
        init.step_first_job.return_value = 0
        with patch.object(cs_init._install_setup, 'init_setup_refusal', return_value=None), \
             patch.object(cs_init, 'Init', return_value=init), \
             patch.object(cs_init.os, 'readlink', return_value='releases/test'), \
             patch('builtins.print') as output:
            self.assertEqual(cs_init.main(cs, MagicMock()), 0)
            output.assert_called_once_with('Add a repository: cs repo add ~/path/to/project')
            output.reset_mock()
            self.cli('add', str(self.repo))
            self.assertEqual(cs_init.main(cs, MagicMock()), 0)
            output.assert_not_called()

    def test_short_form_inventory_and_preserved_fields(self):
        path = _repos.repos_file()
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({'old': str(self.repo), 'custom': {'path': '/other', 'hooks_path': 'hooks'}}))
        self.assertEqual(_repos.inventory()[1], {'name': 'old', 'path': str(self.repo), 'branch': 'main'})
        self.cli('remove', 'old')
        self.assertEqual(json.loads(path.read_text())['custom']['hooks_path'], 'hooks')


if __name__ == '__main__':
    unittest.main()
