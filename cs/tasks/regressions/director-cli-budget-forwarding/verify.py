from pathlib import Path
import json,os,subprocess,shutil,time,sys,tempfile
source=Path(__file__).resolve().parents[3]
if sys.platform!='darwin':
 print('native budget sandbox fixtures require macOS; adapter refuses bounded runs elsewhere')
 raise SystemExit(0)
os.environ.setdefault('CS_TOOLS_BIN',str(source.parent/'target/release/cs-tools'))  # the copied lab has no target/
temporary=tempfile.TemporaryDirectory(prefix='director-cli-budget-fixture-')
proto=Path(temporary.name);lab=proto/'lab';ops=lab/'ops/bin';ops.mkdir(parents=True)
for source_file in (source/'ops/bin').iterdir():
 if source_file.is_file():shutil.copy2(source_file,ops/source_file.name)
sys.path.insert(0, str(source / 'tasks/regressions/_lib'))
from native_runtime import stage_native_runtime
stage_native_runtime(ops)
fake=lab/'fake';fake.mkdir()

claude=fake/'claude';claude.write_text('''#!/usr/bin/env python3
import json,sys,subprocess,time,shlex
from pathlib import Path
args=sys.argv[1:];settings=json.loads(args[args.index('--settings')+1]);hook=settings['hooks']['PreToolUse'][0]['hooks'][0]['command']
case=Path('case').read_text().strip()
if case=='auth_expired':
 print('OAuth access token has expired',file=sys.stderr)
 raise SystemExit(1)
assert args[args.index('--max-turns')+1]=='3'
assert args[args.index('--max-budget-usd')+1]=='0.25'
Path('hello.txt').write_text('hello')
if case=='hook_protected':
 target=Path(shlex.split(hook)[1]); original=target.read_bytes(); protected=False
 try:target.write_bytes(original+b'\\n# changed\\n')
 except PermissionError:protected=True
 Path('probe.json').write_text(json.dumps({'protected':protected}))

for _ in range(8 if case=='tool_cap' else 2):
 call=subprocess.run(hook,shell=True,text=True,capture_output=True)
 if json.loads(call.stdout).get('continue') is False:break
if case=='wall_timeout':time.sleep(10)
value={'type':'result','subtype':'success','is_error':False,'num_turns':2,'result':'done','total_cost_usd':0.01}
if case=='native_error':value['is_error']=True
if case=='turn_cap':value['subtype']='error_max_turns';value['is_error']=True
if case=='usd_cap':value['subtype']='error_max_budget_usd';value['is_error']=True
if case=='malformed':value.pop('num_turns')
if case=='over_budget_success':value['total_cost_usd']=0.26
print(json.dumps(value))
''');claude.chmod(0o755)
fake_template={'binary':str(claude),'args':['--print','--output-format','json','--model','sonnet','{{prompt}}'],'cost_class':'subscription','cost_usd':0,'auth':'subscription'}
(ops/'executor-templates.json').write_text(json.dumps({'fake-claude':fake_template,'claude-code-sonnet':fake_template}))
(lab/'ESTATE').write_text('dev\n');(lab/'specs').mkdir(exist_ok=True)
results=[]
for case,expect in [('hook_protected','done'),('success','done'),('omitted_payload','done'),('auth_expired','auth_expired'),('tool_cap','budget:tool_calls'),('native_error','native_terminal_error'),('turn_cap','budget:turns'),('usd_cap','budget:usd'),('over_budget_success','budget:usd'),('malformed','native_terminal_error'),('check_timeout','check_timeout'),('wall_timeout','timeout')]:
 task=lab/'tasks'/case;setup=task/'setup';setup.mkdir(parents=True,exist_ok=True)
 (setup/'case').write_text(case);(task/'goal.md').write_text('Write hello.txt with hello.');(task/'notes.md').write_text('Fixture only.\n')
 check=task/'check';check.write_text('#!/bin/sh\n'+('sleep 10\n' if case=='check_timeout' else '')+'test "$(cat hello.txt)" = hello\n');check.chmod(0o755)
 if case=='hook_protected':check.write_text(check.read_text()+'python3 -c \'import json; assert json.load(open("probe.json"))["protected"]\'\n')
 limits={'turns':3,'tool_calls':7,'usd':0.25,'seconds':2 if case=='wall_timeout' else 12}
 window=1 if case=='check_timeout' else 5
 spec=lab/'specs'/(case+'.yaml');spec.write_text('id: fixture-'+case+'\nmodel: fake-claude\nbudget: {turns: 3, tool_calls: 7, usd: 0.25, seconds: '+str(limits['seconds'])+'}\ncheck_window: '+str(window)+'\ntrials: 1\nhypothesis: "fake-claude isolated fixture"\n')
 start=time.monotonic()
 payload={'executor':'fake-claude','task':str(task.relative_to(lab)),'spec':str(spec.relative_to(lab)),'exp':'FIXTURE-'+case,'trial':1,'timeout_s':limits['seconds'],'budget':limits,'check_window':window}
 if case=='omitted_payload':
  for key in ('budget','check_window','timeout_s'):payload.pop(key)
 r=subprocess.run([sys.executable,str(ops/'_executor.py')],input=json.dumps(payload),cwd=lab,text=True,capture_output=True,timeout=20)
 if not r.stdout.strip():raise AssertionError(case+': '+r.stderr)
 value=json.loads(r.stdout.splitlines()[-1]);assert value['stop']==expect,(case,value,r.stderr)
 assert (r.returncode==0)==(expect=='done'),(case,r.returncode)
 row=json.loads((lab/'ledger/ledger.jsonl').read_text().splitlines()[-1]);assert row['tool_calls']==(7 if case=='tool_cap' else 0 if case=='auth_expired' else 2)
 if case not in ['wall_timeout','malformed','auth_expired']:assert row['turns']==2
 if case in ['wall_timeout','malformed','auth_expired']:assert row['turns'] is None
 if case=='auth_expired':assert len(row['calls'])==1 and row['calls'][0]['stage']=='task'
 assert row['cost_usd']==0
 if case!='wall_timeout':
  evidence=json.loads((lab/row['transcript']/'evidence/manifest.json').read_text());assert evidence['check']['verdict']==('pass' if expect=='done' else 'fail')
 results.append({'case':case,'stop':value['stop'],'seconds':round(time.monotonic()-start,2),'tools':row['tool_calls']});print(results[-1],flush=True)
(proto/'integration-results.json').write_text(json.dumps(results,indent=2))

# A client-web job as the Director offers it (2026-11-11): the client's spec stamped with its intent, budget as block
# lines, model naming the executor and a hypothesis that does not. The executor reads it as YAML and launches the CLI.
task=lab/'tasks/director_block';(task/'setup').mkdir(parents=True,exist_ok=True)
(task/'setup/case').write_text('success');(task/'goal.md').write_text('Write hello.txt with hello.');(task/'notes.md').write_text('Fixture only.\n')
(task/'check').write_text('#!/bin/sh\ntest "$(cat hello.txt)" = hello\n');(task/'check').chmod(0o755)
stamped=('id: client-web-job\nmodel: claude-code-sonnet\nbudget:\n  turns: 3\n  tool_calls: 7\n  usd: 0.25\n  seconds: 12\n'
 'tasks:\n- tasks/director_block\nhypothesis: \'Sonnet writes the greeting\'\nintent: 01M74BZ2Y1DQ9SF36Y33WQ53R1\n')
spec=lab/'specs/director_block.yaml';spec.write_text(stamped)
order={'executor':'claude-code-sonnet','task':'tasks/director_block','spec':'specs/director_block.yaml','exp':'FIXTURE-BLOCK','trial':1,'timeout_s':12}
r=subprocess.run([sys.executable,str(ops/'_executor.py')],input=json.dumps(order),cwd=lab,text=True,capture_output=True,timeout=20)
assert r.stdout.strip(),r.stderr
value=json.loads(r.stdout.splitlines()[-1]);assert value['stop']=='done' and r.returncode==0,(value,r.stderr)
row=json.loads((lab/'ledger/ledger.jsonl').read_text().splitlines()[-1]);assert row['tool_calls']==2 and row['model']=='claude-code-sonnet',row
print('director-stamped block spec: model names the executor; the CLI ran under its declared limits')
# The installed node (2026-11-11): the gateway hands the executor the node's roots, so the run, its ledger row and its
# workspace live outside the code root, and the evidence must be taken from that workspace (capture complete).
roots=proto/'node-roots';(roots/'state').mkdir(parents=True);(roots/'workspaces').mkdir();env=dict(os.environ,LAB_STATE_ROOT=str(roots/'state'),LAB_WORKSPACES_ROOT=str(roots/'workspaces'))
# Its own exp: run ids are per second, so the run above's id (and its directory under the code root) could be this one's.
r=subprocess.run([sys.executable,str(ops/'_executor.py')],input=json.dumps(dict(order,exp='FIXTURE-NODE')),cwd=lab,text=True,capture_output=True,timeout=20,env=env)
assert r.stdout.strip(),r.stderr
value=json.loads(r.stdout.splitlines()[-1]);assert value['stop']=='done' and r.returncode==0,(value,r.stderr)
row=json.loads((roots/'state/ledger.jsonl').read_text().splitlines()[-1])
evidence=json.loads((roots/'state/runs'/value['run_id']/'evidence/manifest.json').read_text())
assert evidence['capture_state']=='complete' and [f['path'] for f in evidence['files']]==['hello.txt'],evidence
assert row['transcript'].endswith(value['run_id']) and not (lab/'ledger/runs'/value['run_id']).exists(),'a node run never lands under the code root'
print('with separate state and workspaces roots: the run lands in the state root and its evidence is complete')
# Neither model nor hypothesis names the executor: still refused, before any run effect.
spec.write_text(stamped.replace('model: claude-code-sonnet','model: claude-code-opus'))
ledger=lab/'ledger/ledger.jsonl';before=ledger.read_bytes()
r=subprocess.run([sys.executable,str(ops/'_executor.py')],input=json.dumps(order),cwd=lab,text=True,capture_output=True,timeout=20)
assert r.returncode!=0 and "neither the spec's model nor its hypothesis names executor claude-code-sonnet" in r.stdout+r.stderr,(r.stdout,r.stderr)
assert ledger.read_bytes()==before,'Refusal must precede run effects'
print('a spec naming another model and a hypothesis without the executor is refused')

task=lab/'tasks/preflight_timeout';task.mkdir(parents=True,exist_ok=True)
(task/'goal.md').write_text('No work.');(task/'notes.md').write_text('Isolated fixture.')
(task/'preflight').write_text('#!/bin/sh\necho \'{"run":false,"reason":"no work"}\'\nexit 3\n');(task/'preflight').chmod(0o755)
(task/'check').write_text('#!/bin/sh\nsleep 10\n');(task/'check').chmod(0o755)
spec=lab/'specs/preflight_timeout.yaml';spec.write_text('id: preflight_timeout\nmodel: fake-claude\nbudget: {turns: 3, tool_calls: 7, usd: 0.25, seconds: 12}\ncheck_window: 1\ntrials: 1\nhypothesis: "fake-claude preflight check deadline"\n')
r=subprocess.run([sys.executable,str(ops/'_executor.py')],input=json.dumps({'executor':'fake-claude','task':str(task.relative_to(lab)),'spec':str(spec.relative_to(lab)),'exp':'FIXTURE-PREFLIGHT'}),cwd=lab,capture_output=True,text=True,timeout=15)
assert r.stdout.strip(),r.stderr
result=json.loads(r.stdout.splitlines()[-1]);assert result['stop']=='preflight_check_timeout',result
assert result['cli_invoked'] is False and r.returncode!=0
print('preflight timeout recorded; no CLI invocation')

task=lab/'tasks/success'
base={'executor':'fake-claude','task':'tasks/success','spec':'specs/success.yaml','exp':'FIXTURE-MISMATCH','budget':{'turns':3,'tool_calls':7,'usd':0.25,'seconds':12},'check_window':5,'timeout_s':12}
results=[]
for key,value in [('budget',dict(base['budget'],turns=30)),('check_window',500),('timeout_s',1200)]:
 payload=dict(base);payload[key]=value
 ledger=lab/'ledger/ledger.jsonl';before=ledger.read_bytes()
 r=subprocess.run([sys.executable,str(ops/'_executor.py')],input=json.dumps(payload),cwd=lab,capture_output=True,text=True,timeout=15)
 assert r.returncode!=0,(key,r.stdout,r.stderr)
 assert 'work order budgets differ from the spec' in r.stdout+r.stderr,(key,r.stdout,r.stderr)
 assert ledger.read_bytes()==before,'Refusal must precede run effects'
 results.append({'field':key,'refused':True})
print(json.dumps(results))

temporary.cleanup()
