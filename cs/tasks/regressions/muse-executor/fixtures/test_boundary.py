import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from scratch import scratch_dir
from hermetic_env import hermetic_env

sys.path.insert(0, 'ops/bin')  # _muse imports its ops/bin siblings (_job_memory)
spec = importlib.util.spec_from_file_location('muse', 'ops/bin/_muse.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

def refuses(fn, *args):
    try:
        fn(*args)
    except ValueError as e:
        assert 'secret-canary' not in str(e)
        return
    raise AssertionError('required refusal did not occur')

for value in ('secret-canary', ''):
    refuses(m.check_environment, {'META_API_KEY': value})
m.check_environment({'PATH': '/usr/bin'})
base = {'schema_version': 2, 'providers': {'meta': {'mechanism': 'oauth',
        'storage': 'keychain', 'obtained_via': 'device_code',
        'api_base_url': 'https://api.meta.ai/v1'}}}
with scratch_dir(dir=Path.cwd(), prefix='.boundary-') as raw:
    root = Path(raw).resolve()
    home = root / 'real-home'
    ws = root / 'work space "quoted"'
    home.mkdir(); ws.mkdir()
    auth = root / 'auth.json'
    rich = json.loads(json.dumps(base))
    rich['providers']['meta'].update(user_email='secret-canary', access_token='secret-canary')
    rich['providers']['other'] = {'api_key': 'secret-canary'}
    auth.write_text(json.dumps(rich))
    assert m.subscription_metadata(auth) == base
    for field, value in [('mechanism','api_key'), ('storage','file'),
                         ('api_base_url','https://other.invalid'), ('obtained_via','api_key')]:
        bad = json.loads(json.dumps(base)); bad['providers']['meta'][field] = value
        auth.write_text(json.dumps(bad)); refuses(m.subscription_metadata, auth)
    auth.write_text('{bad'); refuses(m.subscription_metadata, auth)
    refuses(m.subscription_metadata, root / 'absent')
    refuses(m.sandbox_command, [sys.executable], Path('/'), home)
    refuses(m.sandbox_command, [sys.executable], root, home)
    pinned = ws / 'config'; pinned.mkdir()
    (pinned / 'settings.json').write_text('{}')
    (home / 'hidden.txt').write_text('secret-canary')
    (ws / 'escape').symlink_to(home, target_is_directory=True)
    probe = ws / 'probe.py'
    probe.write_text('''from pathlib import Path
import sys
ws, home = map(Path, sys.argv[1:])
(ws/'allowed').write_text('yes')
for p in (home/'outside', ws/'escape'/'outside', ws/'config'/'settings.json'):
    try: p.write_text('forbidden')
    except OSError: pass
    else: raise AssertionError(str(p))
try: (home/'hidden.txt').read_text()
except OSError: pass
else: raise AssertionError('personal home readable')
print('bounded')
''')
    command = m.sandbox_command([sys.executable, str(probe), str(ws), str(home)], ws, home, [pinned])
    result = m.run_process(command, ws, {**hermetic_env()}, 8)
    assert result.returncode == 0, result.stderr
    assert 'bounded' in result.stdout
    assert not (home / 'outside').exists()
    assert (pinned/'settings.json').read_text() == '{}'
    nested = home/'project'
    nested.mkdir()
    nested_probe = nested/'probe.py'
    nested_probe.write_text("from pathlib import Path; Path('allowed').write_text('yes')\n")
    nested_command = m.sandbox_command([sys.executable, str(nested_probe)], nested, home)
    nested_result = m.run_process(nested_command, nested, {**hermetic_env()}, 8)
    assert nested_result.returncode == 0, nested_result.stderr
    assert (nested/'allowed').read_text() == 'yes', 'workspace under home must work'
    sleeper = ws/'sleep.py'
    sleeper.write_text('''import subprocess,sys,time
from pathlib import Path
p=subprocess.Popen([sys.executable,'-c',"import time; from pathlib import Path; time.sleep(4); Path('escaped-child').write_text('bad')"])
print('started',flush=True)
time.sleep(10)
''')
    try:
        m.run_process([sys.executable,str(sleeper)], ws, {**hermetic_env()}, 2)
    except subprocess.TimeoutExpired as e:
        assert 'started' in str(e.output)
    else: raise AssertionError('timeout missing')
    time.sleep(4.2)
    assert not (ws/'escaped-child').exists(), 'child survived cancellation'
    sleeper.write_text('''import subprocess,sys,time
p=subprocess.Popen([sys.executable,'-c',"import signal,time; from pathlib import Path; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(3); Path('escaped-stubborn-child').write_text('bad')"])
time.sleep(10)
''')
    try:
        m.run_process([sys.executable,str(sleeper)], ws, {**hermetic_env()}, 2)
    except subprocess.TimeoutExpired:
        pass
    else: raise AssertionError('timeout missing')
    time.sleep(3.2)
    assert not (ws/'escaped-stubborn-child').exists(), 'SIGTERM-resistant child survived timeout'
print('credential refusal, metadata filtering, OS boundary, and process-group cleanup pass')
