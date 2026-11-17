import copy,json
import sys
sys.path.insert(0,'ops/bin')
from _muse_adapter import validate_root_terminal as parse
base=[json.loads(x) for x in open('echo.jsonl') if x.strip()]
def invoke(records): return parse('\n'.join(json.dumps(x) for x in records))
def bad(records):
    try: invoke(records)
    except ValueError: return
    raise AssertionError('accepted invalid stream')
r=invoke(base); assert r['turns']==1 and r['tool_calls']==0 and r['resolved_model'] is None
start=next(x for x in base if x['payload_type']=='run.lifecycle.started')
terminal=base[-1]; cid=start['payload']['command_id']
child=[]
for item in (start,terminal):
    x=copy.deepcopy(item); x['payload']['command_id']='child'; x['payload']['run_stream']['id']='child';child.append(x)
child[-1]['payload_type']='run.terminal.failed';child[-1]['payload']['terminal']='failed'
assert invoke(base[:-1]+child+[terminal])['root_command_id']==cid
bad(base[:-1]+child)
for i in (0,1,4,len(base)-1):
    bad(base[:i]+base[i+1:]);bad(base+[base[i]])
bad([terminal]+base[:-1])
for field,value in [('terminal','failed'),('text',None),('run_stream',{'kind':'run','id':'wrong'}),('command_id','child')]:
    b=copy.deepcopy(base);b[-1]['payload'][field]=value;bad(b)
b=copy.deepcopy(base);b[-1]['payload_type']='run.terminal.unknown';bad(b)
for extra in ([],{'schema_version':2,'payload':{}},{'schema_version':1,'payload':[]}): bad(base+[extra])
for raw in ('', '{', '\n'.join(json.dumps(x) for x in base)+'\nno-json'):
    try: parse(raw)
    except ValueError: continue
    raise AssertionError('accepted invalid JSONL')
print('root terminal cases pass')

for i in (0,1,4):
    b=copy.deepcopy(base);b[i]['payload']['kind']='wrong';bad(b)
for original in (start,terminal):
    b=copy.deepcopy(base);x=copy.deepcopy(original);x['payload']['run_stream']['id']='wrong';bad(b+[x])
x=copy.deepcopy(start);x['payload']['command_id']='alien';bad(base+[x])
print('kind and conflicting root identity cases pass')
