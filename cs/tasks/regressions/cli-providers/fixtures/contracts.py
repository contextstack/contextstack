#!/usr/bin/env python3
import sys
from pathlib import Path
sys.path.insert(0, str(next(p / "tasks/regressions/_lib" for p in Path(__file__).resolve().parents if (p / "tasks/regressions/_lib").is_dir())))
from scratch import scratch_dir

import json,sys
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[4]/"ops/bin"))
from _cli_terminal import terminal
m={"role":"assistant","stopReason":"stop","usage":{"input":10,"output":2}}
pi=[{"type":"agent_start"},{"type":"message_end","message":m},{"type":"agent_end","messages":[m]}]
op=[{"type":"step_start","sessionID":"s","part":{"type":"step-start"}},{"type":"step_finish","sessionID":"s","part":{"type":"step-finish","reason":"stop","tokens":{"input":10}}}]
g={"stopReason":"end_turn","sessionId":"s","requestId":"r","text":"done","num_turns":1}
def wire(x): return json.dumps(x) if isinstance(x,dict) else "\n".join(map(json.dumps,x))
def bad(name,x):
 try:terminal(name,wire(x))
 except (ValueError,TypeError,KeyError):return
 raise AssertionError((name,x))
for name,x in [('pi',pi),('opencode',op),('grok',g)]:
 assert terminal(name,wire(x))['turns']==1
 bad(name,{})
bad('pi',pi[:-1]);bad('pi',pi+pi);bad('pi',[pi[0],pi[2],pi[1]])
bad('pi',[pi[0],pi[1],{"type":"agent_end","messages":[]}])
bad('pi',[pi[0],{"type":"message_end","message":{**m,"stopReason":"error"}},pi[2]])
bad('opencode',op[:-1]);bad('opencode',list(reversed(op)))
bad('opencode',[op[0],{**op[1],'sessionID':'foreign'}])
bad('opencode',[op[0],{**op[1],'part':{'type':'step-finish','reason':'length'}}])
bad('grok',{**g,'num_turns':True});bad('grok',{**g,'stopReason':'max_tokens'})
bad('unknown',g)
assert 'total_cost_usd' not in terminal('grok',wire(g))['native_usage']
print('check: strict native terminal receipts')

for name in ['pi','opencode']:
 assert terminal(name,(Path(__file__).parent/(name+'.jsonl')).read_text())['turns']==1
bad('grok',{k:v for k,v in g.items() if k!='sessionId'})
bad('grok',{k:v for k,v in g.items() if k!='requestId'})
assert terminal('grok',wire(g))['tool_calls_known'] is False
bad('pi',pi+[{'type':'agent_settled'},{'type':'agent_settled'}])
bad('pi',[pi[0],pi[1],{**pi[2],'willRetry':True}])

from _cli_profile import stage
import tempfile
with scratch_dir() as raw:
 ws=Path(raw)/'workspace';ws.mkdir()
 profile={'files':{'pinned.json':{'hooks':[]}},'environment':{'GROK_HOME':'{{home}}'}}
 env,protected=stage(ws,profile)
 assert env['GROK_HOME']==env['HOME'] and protected[0].stat().st_mode&0o777==0o400
 try:stage(ws,profile)
 except ValueError:pass
 else:raise AssertionError('HOME reused')
 sibling=Path(raw)/'workspace-escape';sibling.mkdir()
 outside=Path(raw)/'scope';outside.mkdir();(outside/'.lab-tmp').symlink_to(sibling)
 try:stage(outside,profile)
 except ValueError:pass
 else:raise AssertionError('prefix escape accepted')
print('check: frozen native output contracts and isolated configuration')

# A cap hit during the version probe must retain its resource stop and RSS,
# rather than being misreported as a pin/version mismatch.
import subprocess
from unittest.mock import patch
import _cli_adapter as adapter
import _muse
with scratch_dir() as raw:
 root=Path(raw);ws=root/'workspace';ws.mkdir();home=root/'personal';home.mkdir()
 profile={'auth':'none','cost_class':'free_local','version':'fixture','model':'fixture','files':{}}
 result=subprocess.CompletedProcess([], -9, '', '')
 result.forced_meta={'stop':'memory_cap','peak_rss_bytes':2048}
 context={'name':'opencode','ws':ws,'HOME':home,'HERE':root,'cost_class':'free_local',
          'auth':'none','run_id':'fixture','allowlist_env':lambda *args:{},
          'on_probe':lambda:None,'timeout':5,'memory_cap_mb':1}
 with patch.object(adapter,'pinned',return_value=(profile,Path(sys.executable))), \
      patch.object(adapter,'command',return_value=['fixture']), \
      patch.object(_muse,'run_process',return_value=result) as launch:
  try:adapter.prepare(context)
  except _muse.ForcedStop as error:assert error.result.forced_meta==result.forced_meta
  else:raise AssertionError('probe memory stop lost')
  assert launch.call_args.kwargs['memory_cap_mb']==1
print('check: profiled probe memory limit and stop evidence preserved')
