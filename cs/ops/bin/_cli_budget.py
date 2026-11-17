"""Declared CLI limits and parent-owned admission."""
import json
import math
import shlex
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from _stuck import parse_budget_stuck


KEYS = {'turns', 'tool_calls', 'usd', 'seconds'}


def validate_budget_mapping(budget, *, label='budget', min_turns=0, min_seconds=0):
    """Refuse non-finite and negative budget fields; 0 keeps the runner's unlimited convention."""
    if budget is None:
        return
    if not isinstance(budget, dict):
        raise ValueError('%s must be a mapping of turns/tool_calls/usd/seconds' % label)
    extra = sorted(set(budget) - KEYS - {'stuck'})
    if extra:
        raise ValueError('%s has unknown keys: %s' % (label, ', '.join(extra)))
    number = lambda key, value: type(value) is int or (key == 'usd' and type(value) is float)
    for key in KEYS:
        if key not in budget:
            raise ValueError('Malformed CLI budget')
        if not number(key, budget[key]):
            raise ValueError('Malformed CLI budget')
    limits = {key: float(budget[key]) if key == 'usd' else budget[key] for key in KEYS}
    if any(not math.isfinite(value) or value < 0 for value in limits.values()):
        raise ValueError('Invalid CLI budget')
    if limits['turns'] < min_turns or limits['seconds'] < min_seconds:
        raise ValueError('Invalid CLI budget')


def declared(spec):
    """(limits, check_window) a parsed spec (`_yaml.load`) declares. The budget is a mapping in either YAML style,
    `{turns: 30, ...}` or block lines, as the client wrote it or the Director edited it in place."""
    budget = spec.get('budget') if isinstance(spec, dict) else None
    if set(budget or {}) - {'stuck'} != KEYS:
        raise ValueError('CLI budget requires exactly turns/tool_calls/usd/seconds')
    validate_budget_mapping(budget, label='CLI budget', min_turns=1, min_seconds=1)
    parse_budget_stuck(budget)  # validates budget.stuck when present
    limits = {key: float(budget[key]) if key == 'usd' else budget[key] for key in KEYS}
    check_window = spec.get('check_window', 60)
    if type(check_window) is not int:
        raise ValueError('Malformed check window')
    if check_window < 1:
        raise ValueError('Invalid check window')
    return limits, check_window


class Admission:
    def __init__(self, cap):
        self.cap, self.count, self.refused = cap, 0, False
        self.refusal_reason = "budget:tool_calls"
        self.lock = threading.Lock()

    def request(self):
        with self.lock:
            if self.refused or (self.cap > 0 and self.count >= self.cap):
                self.refused = True
                return {'continue': False, 'stopReason': self.refusal_reason,
                        'hookSpecificOutput': {'hookEventName': 'PreToolUse',
                         'permissionDecision': 'deny', 'permissionDecisionReason': self.refusal_reason}}
            self.count += 1
            # Count admission only. Do not turn a pending native permission denial into approval.
            return {}


class Broker:
    def __init__(self, cap):
        self.admission = Admission(cap)
        admission = self.admission

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                if self.path != '/consume' or self.headers.get('Content-Length') != '0':
                    self.send_error(400)
                    return
                body = json.dumps(admission.request()).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        class Server(ThreadingHTTPServer):
            # Concurrent native hooks must reach admission, including denials.
            request_queue_size = socket.SOMAXCONN

        self.server = Server(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def hook(self, directory):
        path = Path(directory) / 'cli-budget-hook.py'
        port = self.server.server_address[1]
        path.write_text('import json,sys,urllib.request\n'
                        'try:\n'
                        f' r=urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:{port}/consume",data=b""),timeout=2)\n'
                        ' value=json.load(r)\n'
                        ' print(json.dumps(value))\n'
                        'except Exception:\n'
                        ' print(json.dumps({"continue":False,"stopReason":"budget broker unavailable",'
                        '"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",'
                        '"permissionDecisionReason":"budget broker unavailable"}}))\n'
                        ' sys.exit(2)\n')
        return shlex.join([sys.executable, str(path)])


def claude_args(args, budget, hook):
    output = []
    settings = {}
    i = 0
    while i < len(args):
        arg = args[i]
        if arg in ('--max-turns', '--max-budget-usd', '--setting-sources'):
            i += 2
            continue
        if arg == '--settings':
            value = json.loads(args[i + 1])
            if not isinstance(value, dict):
                raise ValueError('CLI settings must be an operator JSON object')
            settings.update(value)
            i += 2
            continue
        output.append(arg)
        i += 1
    if settings.get('hooks'):
        raise ValueError('Budget launch refuses unreviewed template hooks')
    settings.update(disableAllHooks=False, hooks={'PreToolUse': [{'matcher': '*', 'hooks': [
        {'type': 'command', 'command': hook, 'timeout': 3}]}]})
    output += ['--setting-sources', '', '--settings', json.dumps(settings),
               '--max-turns', str(budget['turns'])]
    if budget['usd'] > 0:
        output += ['--max-budget-usd', str(budget['usd'])]
    return output


def native_terminal(stdout):
    """The CLI's result: the one JSON object of --output-format json, or the last line of stream-json."""
    try:
        return json.loads(stdout)
    except ValueError:
        lines = [line for line in (stdout or '').splitlines() if line.strip()]
        data = json.loads(lines[-1]) if lines else None
        if not isinstance(data, dict) or data.get('type') != 'result':
            raise ValueError('stream-json must end with its result')
        return data


def native_result(stdout, budget, admission):
    data = native_terminal(stdout)
    if not isinstance(data, dict):
        raise ValueError('Native CLI terminal must be one JSON result')
    turns = data.get('num_turns')
    if type(turns) is not int or turns < 1:
        raise ValueError('Native CLI omitted measured turns')
    if admission.refused:
        return {'stop': admission.refusal_reason, 'turns': turns, 'tool_calls': admission.count}
    measured = data.get('total_cost_usd')
    if type(measured) not in (int, float) or not math.isfinite(measured) or measured < 0:
        raise ValueError('Native CLI omitted valid usage cost')
    subtype = data.get('subtype')
    if subtype == 'error_max_turns' or turns > budget['turns']:
        stop = 'budget:turns'
    elif subtype == 'error_max_budget_usd' or (budget['usd'] > 0 and measured >= budget['usd']):
        stop = 'budget:usd'
    elif (data.get('type') != 'result' or subtype != 'success'
          or data.get('is_error') is not False or not isinstance(data.get('result'), str)
          or not data['result'].strip()):
        stop = 'native_terminal_error'
    else:
        stop = 'done'
    return {'stop': stop, 'turns': turns, 'tool_calls': admission.count}
