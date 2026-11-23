"""Drive the actual executor and OS boundary with credential-free binaries."""
import hashlib,json,os,shutil,sqlite3,subprocess,sys,tempfile,time
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_lib"))
from scratch import scratch_dir
from hermetic_env import hermetic_env
from cli_execution import command as cli_command
ROOT=Path(__file__).resolve().parents[4]
PI=[{'type':'agent_start'}, {'type':'message_end','message':{'role':'assistant','stopReason':'stop','usage':{'input':1}}}, {'type':'agent_end','messages':[{'role':'assistant','stopReason':'stop','usage':{'input':1}}]}, {'type':'agent_settled'}]
OP=[{'type':'step_start','sessionID':'s','part':{'type':'step-start'}},{'type':'step_finish','sessionID':'s','part':{'type':'step-finish','reason':'stop'}}]
OP2=[OP[0],{'type':'retry','sessionID':'s','attempt':1},{'type':'text','sessionID':'s','part':{'type':'text','text':'ok'}},{'type':'background_wait','sessionID':'s','jobs':[]},OP[1]]
KEYS={'grok':'XAI_API_KEY','opencode-go':'OPENCODE_API_KEY'}
GROK={'stopReason':'end_turn','text':'done','sessionId':'s','requestId':'r','num_turns':1}

def run_case(case):
 name,mode,key=case
 with scratch_dir(prefix='cli-profile-check-') as raw:
  root=Path(raw).resolve();estate=root/'estate';ops=estate/'ops/bin';ops.mkdir(parents=True)
  for path in (ROOT/'ops/bin').glob('_*.py'):shutil.copy2(path,ops/path.name)
  from native_runtime import stage_native_runtime
  stage_native_runtime(ops)
  for f in ['_executor.py', '_toolchains.py', '_lanes.py', '_lane_policy.py', '_job_memory.py', '_vitals.py','executor-templates.json','cli-profiles.json','capture-stimulus','publish-evidence','_harvest_git.py', 'harvest', 'finalize-run']:
   shutil.copy2(ROOT/'ops/bin'/f,ops/f)
  (estate/'ESTATE').write_text('dev\n')  # finalize-run's node identity
  home=root/'personal';home.mkdir();(home/'secret').write_text('private-canary')

  (home/'.grok').mkdir();auth={'credential':{'auth_mode':'oidc','principal_type':'User','expires_at':'2099-01-01T00:00:00Z','key':'private-canary'}}
  if mode=='expired':auth['credential']['expires_at']='2000-01-01T00:00:00Z'
  if mode=='api':auth['credential']['auth_mode']='api'
  (home/'.grok/auth.json').write_text(json.dumps(auth) if mode!='malformed-auth' else '{}')
  if name=='opencode-go' and mode!='nodb':
   (home/'.local/share/opencode').mkdir(parents=True)
   with sqlite3.connect(home/'.local/share/opencode/opencode.db') as conn:
    conn.execute('CREATE TABLE credential (provider_id TEXT, token TEXT)')
    conn.executemany('INSERT INTO credential VALUES (?, ?)', [('opencode-go','go-fixture'),('anthropic','foreign-fixture')])
  binary=root/'fake-cli';data={'pi':PI,'opencode':OP,'opencode-go':OP2,'grok':GROK}[name]
  script='#!'+sys.executable+'\nREAL_HOME='+repr(str(home))+'\nNAME='+repr(name)+'\nMODE='+repr(mode)+'\nDATA='+repr(data)+'\n'+r'''import os,sys,json,time,signal,sqlite3,subprocess
from pathlib import Path
ws=Path.cwd(); marker=ws/'.cs-tmp/calls';marker.parent.mkdir(exist_ok=True)
with marker.open('a') as f:f.write('call\n')
if '--version' in sys.argv:print('fixture' if MODE!='version' else 'wrong');sys.exit()
home=Path(os.environ['HOME'])
assert home==ws/'.cs-tmp/cli-home'
assert 'ANTHROPIC_API_KEY' not in os.environ
for path in [Path(REAL_HOME)/'escape',ws.parent/'escape',home/'pinned.json']:
 try:path.write_text('forbidden')
 except OSError:pass
 else:raise AssertionError('write escaped: '+str(path))
try:(Path(REAL_HOME)/'secret').read_text()
except OSError:pass
else:raise AssertionError('personal read escaped')
try:home.rename(home.with_name('moved'))
except OSError:pass
else:raise AssertionError('config parent renamed')
(ws/'.grok').mkdir(exist_ok=True);(ws/'.grok/config.toml').write_text('hidden=true')
try:(ws/'.grok/config.toml').read_text()
except OSError:pass
else:raise AssertionError('project config readable')
(ws/'.grok/config.toml').unlink();(ws/'.grok').rmdir()
if NAME=='grok':
 assert json.loads((home/'auth.json').read_text())['credential']['auth_mode']=='oidc'
 try:(home/'auth.json').unlink()
 except OSError:pass
 else:raise AssertionError('auth link writable')
if NAME=='opencode-go':
 db=Path(os.environ['OPENCODE_DB']);assert db==home/'data/opencode/opencode.db'
 with sqlite3.connect(db) as conn:
  assert conn.execute('SELECT * FROM credential').fetchall()==[('opencode-go','go-fixture')]
 db.with_name('opencode.db-wal').write_text('wal')
 real_db=Path(REAL_HOME)/'.local/share/opencode/opencode.db'
 for fn in (lambda:real_db.read_text(),lambda:real_db.write_text('tampered')):
  try:fn()
  except OSError:pass
  else:raise AssertionError('real opencode.db accessible in sandbox')
 assert 'OPENCODE_API_KEY' not in os.environ
if MODE=='timeout':
 subprocess.Popen([sys.executable,'-c',"import signal,time;from pathlib import Path;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(3);Path('survivor').write_text('bad')"])
 print('child-started',flush=True);time.sleep(10)
(ws/'answer.txt').write_text('done\n')
if MODE=='terminal':print('{}');sys.exit()
if NAME=='grok':print(json.dumps(DATA))
else:
 for event in DATA:print(json.dumps(event))
if MODE=='exit':sys.exit(3)
'''
  binary.write_text(script);binary.chmod(0o755)
  cfg=root/'config';cfg.mkdir();(cfg/'ESTATE').write_text('dev\n')
  env={k:v for k,v in hermetic_env().items() if not k.startswith(('CS_','GIT_','META_','MUSE_','XAI_','GROK_'))}
  env.update(CS_CONFIG_ROOT=str(cfg),HOME=str(home),ANTHROPIC_API_KEY='private-canary',OPENCODE_CONFIG='/foreign',
               GIT_CONFIG_GLOBAL='/dev/null',GIT_CONFIG_NOSYSTEM='1',GIT_AUTHOR_NAME='fixture',GIT_AUTHOR_EMAIL='f@l',
               GIT_COMMITTER_NAME='fixture',GIT_COMMITTER_EMAIL='f@l')
  profiles=json.loads((ops/'cli-profiles.json').read_text());p=profiles[name]
  p.update(binary=str(binary),min_version='fixture',files={'pinned.json':{'fixed':True}})
  for field in ['prefix','package','package_sha256','sha256','version']:p.pop(field,None)
  profiles[name]=p;(ops/'cli-profiles.json').write_text(json.dumps(profiles))
  if mode=='binary':
   import sys as _sys; _sys.path.insert(0,str(ops)); from _cli_pins import ensure_pin, resolve_binary
   ensure_pin(name, resolve_binary(str(binary), home), 'fixture', env=env)
   binary.write_text(script+'\n# changed\n')
  task=estate/'tasks/work/case';(task/'setup').mkdir(parents=True)
  (task/'goal.md').write_text('Write answer.txt containing done.\n')
  (task/'check').write_text('#!/bin/sh\n[ "$(cat answer.txt)" = done ]\n');(task/'check').chmod(0o755)
  if key is not None:env[KEYS[name]]=key
  r=subprocess.run(cli_command(sys.executable, ops/'_executor.py'),cwd=estate,env=env,input=json.dumps({'executor':name,'task':str(task),'timeout_s':1 if mode=='timeout' else 15}),text=True,capture_output=True,timeout=40)
  rows=[json.loads(s) for s in (estate/'ledger/ledger.jsonl').read_text().splitlines()]
  assert len(rows)==1,(case,r.stdout,r.stderr)
  row=rows[0];run=estate/row['transcript'];ws=run/'workspace'
  assert row.get('evidence_sha') and row.get('capture_state')=='complete',(case,row,r.stderr)
  assert 'private-canary' not in (run/'cli.log').read_text()
  assert row['cost_usd']==0 and row['cost_class']==p['cost_class']
  if mode=='ok' and key is None:
   assert r.returncode==0 and row['verdict']=='pass',(case,row,r.stderr,(run/'cli.log').read_text())
   assert row['auth_verified'] and row['turns']==1 and row['branch']
   manifest=json.loads((run/'stimulus/manifest.json').read_text())
   inputs=[i for i in manifest['inputs'] if i['role']=='executor_file']
   assert len(inputs)==9 and manifest['executor_identity']['version']=='fixture'
   for i in inputs:assert hashlib.sha256((run/'stimulus'/i['path']).read_bytes()).hexdigest()==i['sha256']
  else:
   assert r.returncode and row['verdict']!='pass' and not row.get('branch'),(case,row)
   if key is not None or mode in ('expired','api','malformed-auth','binary','nodb'):
    assert not row['cli_invoked'] and not (ws/'.cs-tmp/calls').exists(),(case,row)
   if mode=='timeout':
    time.sleep(3.2);assert not (ws/'survivor').exists()
  assert not (home/'escape').exists() and not (run/'escape').exists()

cases=[('opencode',m,None) for m in ['ok','binary','version','terminal','exit']]
cases += [('opencode-go',m,None) for m in ['ok','nodb','terminal','exit']]+[('opencode-go','ok','private-canary')]
with ThreadPoolExecutor(max_workers=3) as pool:list(pool.map(run_case,cases))
run_case(('opencode','timeout',None))
print('check: profiled executor auth, isolation, freeze, native terminals, failure receipts and timeout cleanup')
