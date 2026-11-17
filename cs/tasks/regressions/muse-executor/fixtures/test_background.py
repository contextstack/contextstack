import copy,json,sys
sys.path.insert(0,'ops/bin')
from _muse_adapter import validate_root_terminal as parse
base=[json.loads(x) for x in open('background.jsonl')]
def invoke(rs): return parse('\n'.join(json.dumps(r) for r in rs))
def bad(rs):
    try: invoke(rs)
    except ValueError: return
    raise AssertionError('accepted invalid background lifecycle')
accepted=[r for r in base if r['payload_type']=='runtime.command.accepted']
assert len(accepted)==2
root,bg=[r['payload']['command_id'] for r in accepted]
r=invoke(base)
assert r['root_command_id']==root
assert r['resolved_model']=='muse-spark-1.3-contributor',r
kinds=[r['payload']['event']['task_kind'] for r in base if r['payload_type']=='task.lifecycle.proposed' and r['payload']['command_id'] in (root,bg)]
assert r['turns']==sum(k.startswith('model.') for k in kinds),r
assert r['tool_calls']==sum(k.startswith('tool.') for k in kinds),r
for i,record in enumerate(base):
    if record['payload'].get('command_id')!=bg: continue
    typ=record['payload_type']
    if typ in ('runtime.command.accepted','session.run.linked','run.lifecycle.started','run.terminal.completed'):
        if typ!='runtime.command.accepted': bad(base[:i]+base[i+1:])
        bad(base+[record])
        mutated=copy.deepcopy(base);mutated[i]['payload']['kind']='wrong';bad(mutated)
    if typ=='runtime.command.accepted':
        for client in (None,'unknown',''):
            mutated=copy.deepcopy(base);mutated[i]['payload']['client_id']=client;bad(mutated)
    if typ=='run.terminal.completed':
        mutated=copy.deepcopy(base);mutated[i]['payload']['terminal']='failed';bad(mutated)
        mutated=copy.deepcopy(base);mutated[i]['payload']['run_stream']['id']='alien';bad(mutated)
        bad([record]+base[:i]+base[i+1:])
print('native root plus background lifecycle validates, malformed backgrounds refuse')

# Background acceptance itself must follow the initial terminal, not just its completion.
b=copy.deepcopy(base)
i=next(i for i,r in enumerate(b) if r['payload_type']=='runtime.command.accepted' and r['payload']['command_id']==bg)
item=b.pop(i)
j=next(i for i,r in enumerate(b) if r['payload_type']=='run.terminal.completed' and r['payload']['command_id']==root)
b.insert(j,item);bad(b)
assert invoke(base)['resolved_model_source']=='run.model.configured'
