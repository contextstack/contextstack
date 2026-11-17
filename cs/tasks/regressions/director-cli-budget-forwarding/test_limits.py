import json, subprocess, tempfile, unittest, urllib.request, urllib.error
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import os, sys
LAB=Path(__file__).resolve().parents[3]
sys.path.insert(0,str(LAB/"ops/bin"))
os.environ.setdefault('CS_TOOLS_BIN',str(LAB.parent/'target/release/cs-tools'))
from _cli_budget import Admission, Broker, declared, claude_args, native_result
from _yaml import load

def parsed(text):
    with tempfile.TemporaryDirectory() as d:
        path=Path(d)/'spec.yaml';path.write_text(text);return load(path)

class Limits(unittest.TestCase):
    def setUp(self):
        self.budget={'turns':3,'tool_calls':7,'usd':0.25,'seconds':60}
    def test_preserves_limits(self):
        self.assertEqual(declared(parsed('budget: {turns: 3, tool_calls: 7, usd: 0.25, seconds: 60}\ncheck_window: 5\n')),(self.budget,5))
    def test_reads_either_yaml_style(self):
        # The Director edits budget.usd in place; a client may write the mapping as block lines.
        block='id: x\nbudget:\n  turns: 3   # limits\n  tool_calls: 7\n  usd: 0.25\n  seconds: 60\ntasks:\n- tasks/t\n'
        self.assertEqual(declared(parsed(block)),(self.budget,60))
        self.assertEqual(declared(parsed('budget: {turns: 3, tool_calls: 7, usd: 0, seconds: 60}\n'))[0]['usd'],0.0)
    def test_refuses_invalid_declarations(self):
        for raw in ['{}','{turns: 3}','~','{turns: 3, tool_calls: 7, usd: nan, seconds: 60}','{turns: 3, tool_calls: 7, usd: 1, seconds: "60"}',
                    '{turns: 3, tool_calls: 7, usd: 1, seconds: 1e3}','{turns: 0, tool_calls: 7, usd: 1, seconds: 60}',
                    '{turns: 3, tool_calls: -1, usd: 1, seconds: 60}','{turns: 3, tool_calls: 7, usd: 1, seconds: 60, extra: 1}']:
            with self.assertRaises(ValueError,msg=raw):declared(parsed('budget: '+raw+'\n'))
        for window in ['0','x','']:
            with self.assertRaises(ValueError,msg=window):declared(parsed('budget: {turns: 3, tool_calls: 7, usd: 1, seconds: 60}\ncheck_window: '+window+'\n'))
        with self.assertRaises(ValueError):declared(parsed('- a list\n'))
    def test_concurrent_calls_cannot_exceed_cap(self):
        broker=Broker(7)
        try:
            def ask(_):
                return json.load(urllib.request.urlopen(urllib.request.Request(f'http://127.0.0.1:{broker.server.server_address[1]}/consume',data=b''),timeout=5))
            with ThreadPoolExecutor(max_workers=8) as pool:answers=list(pool.map(ask,range(30)))
            self.assertEqual(sum(a=={} for a in answers),7)
            self.assertEqual(broker.admission.count,7)
            self.assertTrue(broker.admission.refused)
            with self.assertRaises(urllib.error.HTTPError):
                urllib.request.urlopen(urllib.request.Request(f'http://127.0.0.1:{broker.server.server_address[1]}/reset',data=b''))
            self.assertEqual(broker.admission.count,7)
        finally:broker.close()
    def test_hook_fails_closed_when_broker_stops(self):
        with tempfile.TemporaryDirectory() as d:
            broker=Broker(1);broker.hook(d)
            hook=Path(d)/'cli-budget-hook.py'
            first=subprocess.run(['python3',str(hook)],capture_output=True,text=True)
            self.assertEqual(json.loads(first.stdout),{})
            second=subprocess.run(['python3',str(hook)],capture_output=True,text=True)
            self.assertFalse(json.loads(second.stdout)['continue'])
            broker.close()
            unavailable=subprocess.run(['python3',str(hook)],capture_output=True,text=True)
            self.assertEqual(unavailable.returncode,2)
            self.assertFalse(json.loads(unavailable.stdout)['continue'])
    def test_replaces_stale_native_caps_without_widening_tools(self):
        args=claude_args(['--print','--tools','Read,Write','--settings','{"disableAllHooks":true}','--max-turns','99','--max-budget-usd','99','task'],self.budget,'python3 hook.py')
        self.assertEqual(args[args.index('--tools')+1],'Read,Write')
        self.assertEqual(args[args.index('--max-turns')+1],'3')
        self.assertEqual(args[args.index('--max-budget-usd')+1],'0.25')
        settings=json.loads(args[args.index('--settings')+1]);self.assertFalse(settings['disableAllHooks'])
        self.assertEqual(len(settings['hooks']['PreToolUse']),1)
    def test_terminal_errors_cannot_be_success(self):
        valid={'type':'result','subtype':'success','is_error':False,'num_turns':2,'result':'done','total_cost_usd':0.01}
        self.assertEqual(native_result(json.dumps(valid),self.budget,Admission(7))['stop'],'done')
        for patch,expected in [({'subtype':'error_max_turns'},'budget:turns'),({'subtype':'error_max_budget_usd'},'budget:usd'),({'is_error':True},'native_terminal_error'),({'result':''},'native_terminal_error')]:
            self.assertEqual(native_result(json.dumps(valid|patch),self.budget,Admission(7))['stop'],expected)
        with self.assertRaises(ValueError):native_result(json.dumps(valid|{'num_turns':None}),self.budget,Admission(7))

unittest.main()
