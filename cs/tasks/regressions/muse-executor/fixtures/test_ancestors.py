import json,os,sys,tempfile
from pathlib import Path
from scratch import scratch_dir
from hermetic_env import hermetic_env
sys.path.insert(0,'ops/bin')
from _muse import sandbox_command,run_process
with scratch_dir(dir=Path.cwd()) as raw:
 root=Path(raw).resolve();home=root/'home';ws=home/'nested'/'workspace';ws.mkdir(parents=True)
 (home/'secret').write_text('canary');(home/'nested/sibling').mkdir();(ws/'escape').symlink_to(home)
 code="""import os,sys
from pathlib import Path
ws,home=map(Path,sys.argv[1:])
for p in ws.parents:
 if p.is_relative_to(home): os.stat(p)
(ws/'allowed').write_text('yes')
for operation in [lambda:os.listdir(home),lambda:os.listdir(home/'nested'),lambda:(home/'secret').read_text(),lambda:(ws/'escape/secret').read_text(),lambda:(home/'outside').write_text('bad'),lambda:(home/'nested/sibling/bad').write_text('bad')]:
 try:operation()
 except OSError:pass
 else:raise AssertionError('personal data or outside write allowed')
"""
 command=sandbox_command([sys.executable,'-c',code,str(ws),str(home)],ws,home)
 result=run_process(command,ws,{**hermetic_env()},10)
 assert result.returncode==0,result.stderr
 assert not (home/'outside').exists()
print('literal ancestor metadata allowed; directory data, personal reads and outside writes denied')
