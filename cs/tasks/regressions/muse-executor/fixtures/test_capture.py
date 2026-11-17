from scratch import scratch_dir
from hermetic_env import hermetic_env
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

src = Path.cwd()
with scratch_dir(dir=src, prefix='.capture-test-') as raw:
    root = Path(raw).resolve(); ops = root/'ops/bin'; ops.mkdir(parents=True)
    shutil.copy2(src/'ops/bin/capture-stimulus', ops/'capture-stimulus')
    for helper in (src/'ops/bin').glob('_*.py'):
        shutil.copy2(helper, ops/helper.name)
    task = root/'tasks/case'; task.mkdir(parents=True)
    (task/'goal.md').write_text('A frozen goal.\n')
    (task/'check').write_text('#!/bin/sh\nexit 0\n'); (task/'check').chmod(0o755)
    profile = ops/'muse-profile.json'; profile.write_text('{"version":"1.0.3"}\n')
    (ops/'helper').write_text('pinned helper\n'); (ops/'helper').chmod(0o755)
    seq = 0
    def capture(extra=None, run=None):
        global seq
        seq += 1
        run = run or root/'ledger/runs'/str(seq)
        args = {'run_dir':str(run), 'task':str(task), 'adapter':'executor'}
        args.update(extra or {})
        result = subprocess.run(['python3',str(ops/'capture-stimulus')], input=json.dumps(args),
            text=True,capture_output=True,cwd=root,env={k:v for k,v in hermetic_env().items() if k!='LAB_JOB_ID'})
        return result,run
    plain,pr = capture()
    assert plain.returncode == 0, plain.stderr
    pm = json.loads((pr/'stimulus/manifest.json').read_text())
    assert pm['env_sha'] == hashlib.sha256(b'').hexdigest()
    assert 'executor_identity' not in pm
    extra = {'executor_files':['ops/bin/muse-profile.json','ops/bin/helper'],
             'executor_identity':{'version':'1.0.3','binary_sha256':'a'*64}}
    a,ar = capture(extra); assert a.returncode == 0,a.stderr
    am = json.loads((ar/'stimulus/manifest.json').read_text())
    frozen = ar/'stimulus/executor/ops/bin/muse-profile.json'
    assert frozen.read_bytes() == profile.read_bytes()
    assert os.access(ar/'stimulus/executor/ops/bin/helper',os.X_OK)
    for item in am['inputs']:
        if item['role'] == 'executor_file':
            q = ar/'stimulus'/item['path']
            assert q.is_file(), 'manifest path does not resolve to its frozen bytes'
            assert hashlib.sha256(q.read_bytes()).hexdigest() == item['sha256']
    assert am['executor_identity'] == extra['executor_identity']
    assert len([i for i in am['inputs'] if i['role']=='executor_file'])==2
    profile.write_text('{"version":"1.0.4"}\n')
    b,br = capture(extra); assert b.returncode == 0,b.stderr
    bm=json.loads((br/'stimulus/manifest.json').read_text())
    assert bm['env_sha'] != am['env_sha'] and frozen.read_text()=='{"version":"1.0.3"}\n'
    c,cr=capture({**extra,'executor_identity':{'version':'1.0.3','binary_sha256':'b'*64}})
    assert c.returncode == 0,c.stderr
    assert json.loads((cr/'stimulus/manifest.json').read_text())['env_sha'] != bm['env_sha']
    again,_=capture(extra,ar);assert again.returncode==0
    assert json.loads((ar/'stimulus/manifest.json').read_text())==am
    (ops/'link').symlink_to(profile)
    for invalid in (['/etc/passwd'],['ops/bin/../secret'],['ops/bin/link'],['missing'],
                    ['ops/bin/helper']*2,['tasks/case/goal.md']):
        r,run=capture({**extra,'executor_files':invalid})
        assert r.returncode!=0,invalid
        assert not (run/'stimulus/manifest.json').exists()
    r,_=capture({**extra,'adapter':'lab'});assert r.returncode!=0
    many = []
    for n in range(10):
        q=ops/('helper%d'%n); q.write_text('x'); many.append('ops/bin/'+q.name)
    r,_=capture({**extra,'executor_files':many});assert r.returncode!=0, 'ten files accepted'
    (ops/'large').write_bytes(b'x'*(1024*1024+1))
    r,_=capture({**extra,'executor_files':['ops/bin/large']});assert r.returncode!=0
    r,_=capture({**extra,'executor_identity':{'large':'x'*17000}});assert r.returncode!=0
sidecar=json.loads((src/'ops/bin/capture-stimulus.json').read_text())
assert sidecar['schema']['properties']['executor_files']['type']=='array'
assert sidecar['schema']['properties']['executor_identity']['type']=='object'
assert sidecar['schema']['additionalProperties'] is False
print('executor profile and binary identity are frozen and bound to env_sha')
