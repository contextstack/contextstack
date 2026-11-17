from scratch import scratch_dir
from hermetic_env import hermetic_env
"""Independent behavioral acceptance through the real executor; no network."""
import hashlib
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

SOURCE = Path.cwd()
VERSION = "Muse Code 1.0.3 (1.0.3-R2198.1)"
NATIVE_SHA = "4c0f960028b603174af7df7bd5051d8c35d6c1aa372a37d18bc770926a0577a7"
FILES = ("_executor.py", "_toolchains.py", "_harvest_git.py", "_lanes.py", "_lane_policy.py", "_job_memory.py", "_vitals.py", "executor-templates.json", "_muse.py", "_muse_adapter.py",
         "muse-profile.json", "capture-stimulus", "capture-stimulus.json",
         "_est.py", "_grant.py", "_corpus_contract.py", "_workspace_setup.py", "_frozen_inputs.py",
         "publish-evidence", "harvest", "finalize-run")
AUTH = {"schema_version": 2, "providers": {"meta": {
    "mechanism": "oauth", "storage": "keychain", "obtained_via": "device_code",
    "api_base_url": "https://api.meta.ai/v1"}}}


def patch_profile(value, binary, sha):
    if isinstance(value, dict):
        return {k: patch_profile(v, binary, sha) for k,v in value.items()}
    if isinstance(value, list):
        return [patch_profile(v, binary, sha) for v in value]
    if isinstance(value, str):
        if "muse-bin-1.0.3-R2198.1" in value:
            return str(binary)
        if value == NATIVE_SHA:
            return sha
    return value


def run_case(mode="ok", key=None):
    with scratch_dir(prefix="muse-independent-") as raw:
        root = Path(raw).resolve()
        estate = root/"estate"; ops = estate/"ops/bin"; ops.mkdir(parents=True)
        (estate / "policy").mkdir()
        shutil.copy2(SOURCE / "policy/sandbox.json", estate / "policy/sandbox.json")
        (estate/"ESTATE").write_text("dev\n")  # finalize-run's node identity
        for helper in (SOURCE/"ops/bin").glob("_*.py"):
            shutil.copy2(helper, ops/helper.name)
        for name in FILES:
            shutil.copy2(SOURCE/"ops/bin"/name, ops/name)
        home = root/"personal-home"; (home/".config/muse").mkdir(parents=True)
        (home/"hidden-personal-context").write_text("private-context-canary")
        (home/"Library/Keychains").mkdir(parents=True)
        auth = json.loads(json.dumps(AUTH))
        auth["providers"]["meta"]["user_email"] = "identity-canary"
        if mode == "auth": auth["providers"]["meta"]["mechanism"] = "api_key"
        (home/".config/muse/auth.json").write_text(json.dumps(auth))
        fixture = json.loads(json.dumps([json.loads(s) for s in (SOURCE/"echo.jsonl").read_text().splitlines()]))
        binary = root/"fake-muse"
        script = "#!"+sys.executable+"\n" + "MODE="+repr(mode)+"\n" + "REAL_HOME="+repr(str(home))+"\n" + "FIXTURE="+repr(fixture)+"\n" + "VERSION="+repr(VERSION)+"\n" + r'''import json,os,signal,subprocess,sys,time
from pathlib import Path
ws=Path.cwd()
marker=ws/'.lab-muse/native-calls'
marker.parent.mkdir(exist_ok=True)
with marker.open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
if '--version' in sys.argv:
    print(VERSION if MODE!='version' else 'Muse Code wrong'); sys.exit(0)
if sys.argv[1:]==['config','status']:
    state='present' if MODE=='config' else 'absent'
    print('Enterprise configuration status\nGeneration: sha256:'+'a'*64+'\nSources:')
    for plane,source in [('defaults','system_file'),('policy','system_file'),('defaults','macos_managed_preferences'),('policy','macos_managed_preferences')]:
        print('  plane='+plane+' source_class='+source+' state='+state)
    sys.exit(0)
assert sys.argv[1]=='exec', sys.argv
assert '--json' in sys.argv and '--no-foreign-personal-context' in sys.argv
assert sys.argv[sys.argv.index('--provider')+1]=='meta'
assert sys.argv[sys.argv.index('--model')+1]=='muse-spark-1.3-contributor'
assert sys.argv[sys.argv.index('--worktree')+1]=='off'
assert '--no-session-log' in sys.argv and '--disable-web-tools' in sys.argv
assert not any(k.startswith(('META_','MUSE_','TBH_')) for k in os.environ)
actual_home=Path(os.environ['HOME']).resolve()
assert actual_home == ws/'.lab-tmp/muse',actual_home
library=actual_home/'Library'; keychain=library/'Keychains'
assert keychain.is_symlink() and keychain.resolve()==Path(REAL_HOME)/'Library/Keychains'
try: keychain.unlink()
except OSError: pass
else: raise AssertionError('Keychain link can be replaced')
try: library.rename(actual_home/'moved-library')
except OSError: pass
else: raise AssertionError('Keychain parent can be renamed')
for name in ('XDG_CONFIG_HOME','XDG_DATA_HOME','XDG_CACHE_HOME','TMPDIR','TMP','TEMP'):
    assert Path(os.environ[name]).resolve().is_relative_to(ws),name
config=Path(os.environ['XDG_CONFIG_HOME'])/'muse'
assert json.loads((config/'settings.json').read_text())=={'schema_version':1}
assert 'identity-canary' not in (config/'auth.json').read_text()
for path in (Path(REAL_HOME)/'escaped', ws.parent/'escaped',config/'settings.json',config/'auth.json'):
    try: path.write_text('forbidden')
    except OSError: pass
    else: raise AssertionError('unconfined write: '+str(path))
try: (Path(REAL_HOME)/'hidden-personal-context').read_text()
except OSError: pass
else: raise AssertionError('personal context readable')
if MODE in ('runtime','runtime-after-start'):
    if MODE=='runtime-after-start': print(json.dumps(FIXTURE[0]))
    print('runtime host failed to start: failed to read skill file',file=sys.stderr);sys.exit(1)
if MODE=='missing-auth':
    print('missing meta credentials: run muse login',file=sys.stderr);sys.exit(1)
if MODE=='timeout':
    subprocess.Popen([sys.executable,'-c',"import signal,time;from pathlib import Path;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(5);Path('survivor').write_text('bad')"])
    print('child-started',flush=True); time.sleep(20)
(ws/'answer.txt').write_text('done\n')
records=FIXTURE
if MODE=='truncated': records=records[:-1]
if MODE=='mismatch': records[-1]['payload']['command_id']='alien'
if MODE=='child-terminal': records[-1]['payload_type']='task.lifecycle.completed'
if MODE=='failed':
    records[-1]['payload_type']='run.terminal.failed';records[-1]['payload']['terminal']='failed'
if MODE=='unknown-terminal':
    records[-1]['payload_type']='run.terminal.surprise';records[-1]['payload']['terminal']='surprise'
for r in records: print(json.dumps(r))
if MODE=='malformed': print('{broken')
if MODE=='exit': sys.exit(7)
'''
        binary.write_text(script); binary.chmod(0o755)
        sha=hashlib.sha256(binary.read_bytes()).hexdigest()
        profile=patch_profile(json.loads((ops/'muse-profile.json').read_text()),binary,sha)
        assert str(binary) in json.dumps(profile), 'profile native path not patched'
        profile['min_version'] = VERSION
        (ops/'muse-profile.json').write_text(json.dumps(profile))
        cfg = root/'config'; cfg.mkdir(); (cfg/'ESTATE').write_text('dev\n')
        if mode=='binary':
            import sys as _sys
            _sys.path.insert(0, str(ops))
            from _cli_pins import ensure_pin
            ensure_pin('muse-code', binary, VERSION, env={'LAB_CONFIG_ROOT': str(cfg)})
            recorded = json.loads((cfg/'cli-pins.json').read_text())['muse-code']['sha256']
            assert recorded == sha, 'machine pin did not record the fixture binary'
            binary.write_text(script+'\n# changed after pin\n')
        task=estate/'tasks/work/case';(task/'setup').mkdir(parents=True)
        (task/'goal.md').write_text('Write answer.txt containing done.\n')
        (task/'check').write_text('#!/bin/sh\n[ "$(cat answer.txt)" = done ]\n');(task/'check').chmod(0o755)
        env={k:v for k,v in hermetic_env().items() if not k.startswith(('GIT_','LAB_','META_','MUSE_','TBH_'))}
        env['HOME']=str(home)
        env['LAB_CONFIG_ROOT']=str(cfg)
        if key is not None: env['META_API_KEY']=key
        result=subprocess.run([sys.executable,str(ops/'_executor.py')],cwd=estate,env=env,
            input=json.dumps({'executor':'muse-code','task':str(task),'exp':'task-999','timeout_s':2 if mode=='timeout' else 10}),
            text=True,capture_output=True,timeout=45)
        rows=[json.loads(s) for s in (estate/'ledger/ledger.jsonl').read_text().splitlines()]
        assert len(rows)==1,(mode,rows,result.stderr)
        row=rows[0];run=estate/row['transcript']
        assert (run/'cli.log').is_file() and (run/'transcript.md').is_file(), (mode,row)
        assert not any(p.is_symlink() for p in run.rglob('Keychains')), ('Keychain link left in run evidence', mode)
        assert 'identity-canary' not in (run/'cli.log').read_text()
        assert row['cost_usd']==0 and row['cost_class']=='subscription'
        assert row['est_basis']=='chars' and row['est_rate_known'] is False, (mode,row)
        if mode=='ok' and key is None:
            assert result.returncode==0 and row['verdict']=='pass', (row,result.stderr)
            recorded = json.loads((cfg/'cli-pins.json').read_text())['muse-code']
            assert recorded['sha256']==sha and not (estate/'cli-pins.json').exists(), recorded
            assert row['auth_source']=='oauth_keychain' and row['auth_verified'] is True
            assert row['branch'] and row['evidence_sha'] and row['capture_state']=='complete'
            manifest=json.loads((run/'stimulus/manifest.json').read_text())
            assert sha in json.dumps(manifest['executor_identity'])
            assert VERSION in json.dumps(manifest['executor_identity'])
            assert manifest['env_sha']!=hashlib.sha256(b'').hexdigest()
            for item in manifest['inputs']:
                if item['role']=='executor_file':
                    assert hashlib.sha256((run/'stimulus'/item['path']).read_bytes()).hexdigest()==item['sha256']
        else:
            assert result.returncode!=0 and row['verdict']!='pass', (mode,row)
            assert not row.get('branch'),(mode,row)
            if key is not None:
                assert row['cli_invoked'] is False and row['verdict']=='void',row
                assert not list((run/'workspace').rglob('native-calls')),'META allowed a native probe'
            if mode=='missing-auth':
                assert row['verdict']=='void' and row['stop']=='muse_auth_unavailable'
                assert row['auth_verified'] is False
            if mode=='runtime':
                assert row['verdict']=='void' and row['stop']=='muse_runtime_unavailable'
                assert row['auth_verified'] is False
            if mode=='runtime-after-start':
                assert row['verdict']=='fail' and row['stop']=='executor_exit_1'
            if mode=='timeout':
                import time
                time.sleep(5.2)
                assert not (run/'workspace/survivor').exists(),'child outlived timeout'
        assert not (home/'escaped').exists() and not (run/'escaped').exists()

run_case()
for key in ('', 'secret-key-canary'): run_case(key=key)
# Cases own disjoint temporary estates; bound their subprocess concurrency.
with ThreadPoolExecutor(max_workers=3) as pool:
    list(pool.map(run_case, ('auth','binary','version','config','truncated','mismatch',
                           'child-terminal','failed','unknown-terminal','malformed','exit','missing-auth','runtime','runtime-after-start')))
run_case('timeout')
print('independent Muse executor acceptance passes')
