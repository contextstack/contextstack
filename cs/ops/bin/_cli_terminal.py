"""Native CLI terminal receipts."""
import json


def terminal(name, text):
    if name == 'grok':
        data = json.loads(text)
        if (not isinstance(data, dict) or data.get('stopReason') != 'end_turn'
                or not isinstance(data.get('text'), str) or type(data.get('num_turns')) is not int
                or data['num_turns'] < 1 or any(not isinstance(data.get(k), str) or not data[k]
                                              for k in ('sessionId', 'requestId'))):
            raise ValueError('Grok did not report a complete end_turn')
        return {'turns': data['num_turns'], 'tool_calls': 0, 'tool_calls_known': False,
                'native_usage': {k: data[k] for k in ('usage', 'modelUsage', 'total_cost_usd',
                                                    'total_cost_usd_ticks') if k in data}}
    if name not in ('pi', 'opencode', 'opencode-go'):
        raise ValueError('unknown CLI terminal format')
    events = [json.loads(line) for line in text.splitlines() if line.strip()]
    if not events or any(not isinstance(e, dict) for e in events):
        raise ValueError('CLI requires JSONL objects')
    return pi(events) if name == 'pi' else opencode(events)


# opencode 1.x and v2 `run --format json`; v2 adds retry and background_wait, which carry no step.
OPENCODE_STEPLESS = ('text', 'reasoning', 'retry', 'background_wait')


def pi(events):
    starts = [i for i, e in enumerate(events) if e.get('type') == 'agent_start']
    ends = [i for i, e in enumerate(events) if e.get('type') == 'agent_end']
    settled = [i for i, e in enumerate(events) if e.get('type') == 'agent_settled']
    if (len(starts) != 1 or len(ends) != 1 or starts[0] >= ends[0]
            or settled not in ([], [ends[0] + 1]) or ends[0] + len(settled) != len(events) - 1):
        raise ValueError('pi requires one ordered, complete agent lifecycle')
    assistants = [(i, e['message']) for i, e in enumerate(events) if e.get('type') == 'message_end'
                  and isinstance(e.get('message'), dict) and e['message'].get('role') == 'assistant']
    if (not assistants or any(not starts[0] < i < ends[0] for i, _ in assistants)
            or any(m.get('stopReason') not in ('toolUse', 'stop') for _, m in assistants)
            or assistants[-1][1].get('stopReason') != 'stop'):
        raise ValueError('pi assistant completion missing or failed')
    end = events[ends[0]]
    messages = end.get('messages')
    if not isinstance(messages, list) or any(not isinstance(m, dict) for m in messages):
        raise ValueError('pi final messages malformed')
    final = [m for m in messages if m.get('role') == 'assistant']
    if end.get('willRetry') is True or not final or final[-1] != assistants[-1][1]:
        raise ValueError('pi terminal message disagrees with completed assistant')
    if any(e.get('type') == 'tool_execution_end' and e.get('isError') is True for e in events):
        raise ValueError('pi reported a failed tool')
    return {'turns': len(assistants),
            'tool_calls': sum(e.get('type') == 'tool_execution_start' for e in events),
            'native_usage': {'messages': [m['usage'] for _, m in assistants if isinstance(m.get('usage'), dict)]}}


def opencode(events):
    sid = events[0].get('sessionID')
    if not isinstance(sid, str) or not sid or any(e.get('sessionID') != sid for e in events):
        raise ValueError('OpenCode requires one session identity')
    finishes, active, last = [], False, None
    for event in events:
        kind, part = event.get('type'), event.get('part', {})
        if not isinstance(part, dict) or kind == 'error':
            raise ValueError('OpenCode reported an error or malformed part')
        if kind == 'tool_use' and part.get('state', {}).get('status') == 'error':
            raise ValueError('OpenCode reported a failed tool')
        if kind == 'step_start':
            if active or part.get('type') != 'step-start':
                raise ValueError('OpenCode start is missing or out of order')
            active = True
        elif kind == 'step_finish':
            if not active or part.get('type') != 'step-finish':
                raise ValueError('OpenCode finish is missing or out of order')
            finishes.append(part)
            active = False
        elif kind != 'tool_use' and kind not in OPENCODE_STEPLESS:
            continue  # an event type this parser does not know carries no step
        if kind in ('step_start', 'step_finish', 'tool_use'):
            last = kind
    if (active or not finishes or last != 'step_finish'
            or finishes[-1].get('reason') != 'stop'
            or any(p.get('reason') != 'tool-calls' for p in finishes[:-1])):
        raise ValueError('OpenCode terminal is incomplete or failed')
    return {'turns': len(finishes), 'tool_calls': sum(e.get('type') == 'tool_use' for e in events),
            'native_usage': {'steps': finishes}}
