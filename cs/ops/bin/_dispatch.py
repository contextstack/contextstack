"""Dispatch identity and first-terminal semantics shared by queue consumers."""
from _estate_io import iter_jsonl, utc_time

TERMINAL = {"done", "error", "operator_close", "cancelled"}


def qid(row):
    return str(row.get("id") or row.get("job") or "")


def first_terminal(path, jid):
    if not jid:
        return None
    for row in iter_jsonl(path, strict=True, errors="strict", objects_only=False):
        if qid(row) == jid and row.get("state") in TERMINAL:
            return row
    return None


def spec_value(text, key):
    """A top-level scalar; a quoted YAML scalar ("x" or 'x') reads as its value. The Director quotes `model:` in
    the chat bundles it builds, and a quoted CLI template name must still route to the executor (2026-11-12)."""
    for line in text.splitlines():
        if line.startswith(key + ":"):
            value = line.split(":", 1)[1].strip()
            if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                value = value[1:-1]
            return value
    return ""


def executor_template(templates, value):
    """Resolve an exact template or a declared CLI model selection."""
    if value in templates:
        return templates[value]
    name, separator, model = value.partition(":")
    template = templates.get(name, {})
    flag = template.get("model_flag")
    if not separator or not flag or model not in template.get("models", {}) or "args" not in template:
        return None
    args = list(template["args"])
    if flag in args:
        args[args.index(flag) + 1] = model
    else:
        # Preserve the CLI subcommand and add options before prompt arguments.
        index = next((i for i, arg in enumerate(args) if arg.startswith(("-", "{{"))), len(args))
        args[index:index] = [flag, model]
    return {**template, "args": args}


def first_terminal_states(rows, *, terminal=TERMINAL):
    states = {}
    for row in rows:
        job = qid(row)
        if job and row.get("state") in terminal and job not in states:
            states[job] = row.get("state")
    return states


def job_records(rows):
    jobs = {}
    for row in rows:
        if not qid(row):
            continue
        job = jobs.setdefault(qid(row), {'pending': None, 'running': None, 'terminal': None, 'meta': {}})
        if job['terminal'] is not None:  # first-terminal-wins, including timestamps
            continue
        job['meta'].update(row)
        state = row.get('state')
        if state in ('pending', 'running') and job[state] is None:
            job[state] = utc_time(row.get('ts'))
        if state in TERMINAL:
            job['terminal'] = row
    return jobs

