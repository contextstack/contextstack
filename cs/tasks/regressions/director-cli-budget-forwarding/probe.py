"""Exercise the real dispatcher boundary without invoking a provider."""
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from unittest.mock import patch

code_root = Path(__file__).resolve().parents[3]
os.environ.setdefault('CS_TOOLS_BIN', str(code_root.parent / 'target/release/cs-tools'))
sys.path.insert(0, str(code_root / 'ops/bin'))
loader = importlib.machinery.SourceFileLoader('budget_probe_tick', str(code_root / 'ops/bin/tick'))
spec = importlib.util.spec_from_loader(loader.name, loader)
tick = importlib.util.module_from_spec(spec)
loader.exec_module(tick)
limits = {'turns': 3, 'tool_calls': 7, 'usd': 0.25, 'seconds': 60}
text = ('id: exp-fixture\nmodel: claude-code-sonnet\n'
        'budget: {turns: 3, tool_calls: 7, usd: 0.25, seconds: 60}\ncheck_window: 5\n')
calls = []
real_run = subprocess.run


def child(command, **kwargs):
    if Path(command[0]) != code_root / 'ops/bin/executor':
        return real_run(command, **kwargs)
    calls.append(json.loads(kwargs['input']))
    return subprocess.CompletedProcess(command, 0)


with tempfile.TemporaryDirectory(prefix='director-budget-probe-') as temp:
    tick.Q = Path(temp) / 'dispatch.jsonl'
    tick.Q.write_text('')
    (Path(temp) / 'fixture.yaml').write_text(text)  # the dispatcher reads the budget from the spec file as YAML
    with patch.object(tick, 'child_env', return_value={}), patch.object(tick.subprocess, 'run', side_effect=child), \
            patch.object(tick, 'CODE_ROOT', Path(temp)):
        tick.one_executor_trial('fixture.yaml', text, 'tasks/work/fixture', 1)
assert len(calls) == 1, 'Expected one executor launch'
payload = calls[0]
print(json.dumps(payload))
assert payload.get('budget') == limits, 'CLI work order dropped declared budget'
assert payload.get('check_window') == 5, 'CLI work order dropped check timeout'
