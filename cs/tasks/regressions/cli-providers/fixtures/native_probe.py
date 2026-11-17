"""Optional installed-binary characterization; fake localhost model, no credentials."""
import sys
from pathlib import Path
sys.path.insert(0, str(next(p / "tasks/regressions/_lib" for p in Path(__file__).resolve().parents if (p / "tasks/regressions/_lib").is_dir())))
from scratch import scratch_dir

import json,os,sys,tempfile,threading
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
ROOT=Path(__file__).resolve().parents[4]
sys.path.insert(0,str(ROOT/'ops/bin'))
import _cli_adapter
from _cli_terminal import terminal

with scratch_dir(prefix='cli-native-proof-') as raw:
 root=Path(raw).resolve();calls=[]
 class Handler(BaseHTTPRequestHandler):
  def log_message(self,*a):pass
  def do_POST(self):
   data=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
   calls.append({'path':self.path,'model':data['model'],'has_tools':bool(data.get('tools'))})
   write=next((t['function'] for t in data.get('tools',[]) if t['function']['name']=='write'),None)
   used=any(m.get('role')=='tool' for m in data['messages'])
   delta={'role':'assistant','content':'fixture complete'};finish='stop'
   if write and not used:
    field='filePath' if 'filePath' in write['parameters']['properties'] else 'path'
    delta={'role':'assistant','content':'','tool_calls':[{'index':0,'id':'fixture-write','type':'function','function':{'name':'write','arguments':json.dumps({field:str(self.server.ws/'answer.txt'),'content':'done\n'})}}]};finish='tool_calls'
   chunk={'id':'fixture','object':'chat.completion.chunk','created':0,'model':'local-coder','choices':[{'index':0,'delta':delta,'finish_reason':None}]}
   out='data: '+json.dumps(chunk)+'\n\n';chunk['choices']=[{'index':0,'delta':{},'finish_reason':finish}];chunk['usage']={'prompt_tokens':10,'completion_tokens':2,'total_tokens':12}
   out+='data: '+json.dumps(chunk)+'\n\ndata: [DONE]\n\n'
   self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(out)));self.end_headers();self.wfile.write(out.encode())
 server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
 threading.Thread(target=server.serve_forever,daemon=True).start()
 profiles=json.loads((ROOT/'ops/bin/cli-profiles.json').read_text());folder=root/'ops';folder.mkdir()
 for name in ['opencode']:
  ws=root/name;ws.mkdir();server.ws=ws;profile=profiles[name]
  profile['network_port']=server.server_port
  profile['files']=json.loads(json.dumps(profile['files']).replace('127.0.0.1:8000','127.0.0.1:'+str(server.server_port)))
  (folder/'cli-profiles.json').write_text(json.dumps(profiles))
  c={'name':name,'ws':ws,'HOME':Path.home(),'HERE':folder,'timeout':45,'cost_class':'free_local','auth':'none','on_probe':lambda:None,'allowlist_env':lambda *_:{'PATH':'/opt/homebrew/bin:/usr/bin:/bin'}}
  binary,env,protected,identity=_cli_adapter.prepare(c)
  result=_cli_adapter.execute(folder,ws,Path.home(),binary,env,protected,identity,'Write answer.txt containing done.',45)
  assert result.returncode==0,(name,result.stdout,result.stderr)
  parsed=terminal(name,result.stdout)
  assert (ws/'answer.txt').read_text()=='done\n' and parsed['tool_calls']==1,(name,result.stdout,result.stderr)
  print(json.dumps({'provider':name,'version':identity['version'],'turns':parsed['turns'],'tool_calls':parsed['tool_calls'],'check':'pass','network':'localhost fake only'}))
 server.shutdown();print('requests',json.dumps(calls))
