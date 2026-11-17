"""Error vocabulary `error-v1` and its classifier (Plan.CS.v2.1 §4). Not an op.

Classification is a reading of evidence the runners already write: terminal.json's `stop` (the run)
and messages.jsonl `tool` events (each call). The runners' strings are pinned byte for byte by the
v2 fixtures under both runners, so this module's input is a contract; a reworded message becomes
`unclassified`, never a finalization failure. finalize-run assigns (row_version 2 rows); every other
reader calls row_error(row), which classifies a version 1 row's stop the same way on read.

  class                  scope          source           retryable  assigned from
  gateway_unreachable    run            runner           true       stop `error: gateway unreachable: ...`
  provider_fault         run            gateway          true       `gateway runtime_fault|retryable_gateway [code]`
  gateway_config         run            gateway          false      `gateway fatal_session [code]`, code in CONFIG_CODES
  gateway_protocol       run            gateway|runner   false      any other fatal_session code; `bad gateway response`,
                                                                    `gateway status`, `gateway omitted cost numbers`
  node_config            run            runner|launcher  false      `no gateway endpoint`, op and load errors; dispatch
                                                                    roots / env_policy / sandbox_policy refusals
  node_restart           run            launcher         true       dispatch `orphaned_running` after a restart
  interrupted            run            runner           true       stop `interrupted`
  hook_rejected          run or call    hook             false      stop `aborted` (pre_tool exit 2); a call `pre_tool <h> refused:`
  tool_timeout           call           tool             true       call ending `exit: signal: killed` ("killed (timeout or
                                                                    external)"), unless the run's own deadline stopped that turn
  sandbox_unenforceable  call           sandbox          false      call with a `cs-sandbox: ` line ending `exit: exit status 101`
  sandbox_denied         call           sandbox          false      a failed call of a tool the wrapper reported on, whose error
                                                                    reads as the sandbox's denial (sandbox_denied below)
  spool_full             finalization   finalize-run     true       ENOSPC inside finalize-run; tick's low-disk admission stop
  unclassified           run            -                false      any other `error:` stop

`budget:*`, `done` and the landing/operator stops are stop reasons, not errors: no object.
`tool_exit` is not a class: call_errors counts a plain non-zero tool exit under it as feedback.

sandbox_denied is a heuristic over the wrapper's reports (cs-sandbox --report-dir, grouped per tool by finalize-run),
approximate by the 3a decision (R3 narrowed): the wrapper sees the tool's exit, never the syscall that failed. A failed
call is denied when its observation carries a denial text (EPERM/EACCES/EROFS/ENOENT, or their strerror) and a path
named in its args or output, resolved against the workspace (symlinks followed), is one the effective policy withholds:
outside every readable root with EPERM/EACCES/EROFS, or with ENOENT when the host has the path (the sandbox hid it);
outside every writable root with EPERM/EROFS. Or when that tool's reports show a narrowing the failure matches: a
narrowed path named in the call, or a narrowed net with a network error. Limits: a path the tool builds without naming
it, or a denial reworded by the tool, stays tool_exit; a host-missing path outside the policy is not a denial; EACCES
inside the readable roots (ordinary file modes) is not a denial; reports are per tool, not per call (the policy is the
same for every call of one tool in one run, tmp aside), so attribution is by tool.
"""
import os
import re

VOCABULARY = "error-v1"
CLASSES = {  # class: (scope, retryable)
    "gateway_unreachable": ("run", True), "provider_fault": ("run", True),
    "gateway_config": ("run", False), "gateway_protocol": ("run", False),
    "node_config": ("run", False), "node_restart": ("run", True), "interrupted": ("run", True),
    "hook_rejected": ("run", False), "tool_timeout": ("call", True),
    "sandbox_unenforceable": ("call", False), "sandbox_denied": ("call", False),
    "spool_full": ("finalization", True), "unclassified": ("run", False),
}
CONFIG_CODES = {"gateway.no_provider_credential", "gateway.provider_unsupported", "gateway.unknown_model"}
LAUNCHER_KINDS = ("roots", "env_policy", "sandbox_policy")
DETAIL_BYTES = 512
GATEWAY_CODED = re.compile(r"gateway (\w+) \[([^\]]*)\]: ?")
# Runner-side node failures: the gateway endpoint, a refused op, a palette or hook that did not load.
NODE = re.compile(r"(no gateway endpoint|(?:capture-stimulus|setup-workspace|publish-evidence)(?=: )|"
                  r"tool \"[^\"]*\": |(?:post_turn|pre_tool) \"[^\"]*\": not in hooks/bin|frozen stimulus unreadable)")
PROTOCOL = ("bad gateway response", "gateway status ", "gateway omitted cost numbers")
PERM = re.compile(r"Operation not permitted|\bEPERM\b")
ACCES = re.compile(r"Permission denied|\bEACCES\b")
ROFS = re.compile(r"Read-only file system|\bEROFS\b")
NOENT = re.compile(r"No such file or directory|\bENOENT\b")
NET = re.compile(r"Operation not permitted|Network is unreachable|Could not resolve host|Name or service not known|"
                 r"nodename nor servname|Temporary failure in name resolution")
ABS_PATH = re.compile(r"(?<![\w.~:/-])/[^\s'\"`:,;()<>\[\]{}]+")
SANDBOX_CLASSES = ("sandbox_denied", "sandbox_unenforceable")


def clip(text):
    raw = str(text or "").encode("utf-8")
    return raw[:DETAIL_BYTES].decode("utf-8", "ignore")


def error_object(cls, code=None, source=None, detail=""):
    """{class, code, source, retryable, detail}: the one shape rows, dispatch rows and alerts carry."""
    return {"class": cls, "code": code or None, "source": source or None,
            "retryable": CLASSES[cls][1], "detail": clip(detail)}


def classify_stop(stop, events=()):
    """A run's terminal stop (and, for `aborted`, its events) -> error object, or None for a stop reason."""
    stop = str(stop or "")
    if stop == "provider_unavailable":
        return error_object("provider_fault", code=stop, source="executor",
                            detail="Cursor could not reach its model; its quota may be used up — try another model")
    if stop == "interrupted":
        return error_object("interrupted", source="runner")
    if stop == "aborted":
        hook = [e.get("output", "") for e in events if e.get("event") == "pre_tool"]
        return error_object("hook_rejected", source="hook", detail=hook[-1] if hook else "")
    kind, sep, rest = stop.partition(": ")
    if sep and kind in LAUNCHER_KINDS:
        return error_object("node_config", code=kind, source="launcher", detail=rest)
    if kind == "error" and stop.startswith("error: scheduler orphaned_running"):
        return error_object("node_restart", code="orphaned_running", source="launcher", detail=rest)
    if not stop.startswith("error: "):
        return None
    msg = stop[len("error: "):]
    if msg.startswith("gateway unreachable: "):
        return error_object("gateway_unreachable", source="runner", detail=msg[len("gateway unreachable: "):])
    coded = GATEWAY_CODED.match(msg)
    if coded:
        gclass, code, detail = coded.group(1), coded.group(2), msg[coded.end():]
        if gclass in ("runtime_fault", "retryable_gateway"):
            return error_object("provider_fault", code, "gateway", detail)
        if gclass == "fatal_session" and code in CONFIG_CODES:
            return error_object("gateway_config", code, "gateway", detail)
        if gclass == "fatal_session":
            return error_object("gateway_protocol", code, "gateway", detail)
    if msg.startswith(PROTOCOL):
        return error_object("gateway_protocol", source="runner", detail=msg)
    node = NODE.match(msg)
    if node:
        return error_object("node_config", source="runner", detail=msg)
    return error_object("unclassified", detail=msg)


def _strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from _strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from _strings(item)


def _under(path, roots):
    return any(path == root or path.startswith(root.rstrip("/") + "/") for root in roots if root)


def sandbox_denied(event, reports, exists=os.path.lexists):
    """A failed call (its tool's wrapper reports) -> True when it reads as the sandbox's denial (module docstring)."""
    content = str(event.get("content") or "")
    perm, acces, rofs, noent = (bool(p.search(content)) for p in (PERM, ACCES, ROFS, NOENT))
    if not reports or not (perm or acces or rofs or noent or NET.search(content)):
        return False
    first = reports[0]
    eff, ws = first.get("effective_policy") or {}, str(first.get("cwd") or "")
    env_dirs = [str(p) for p in (eff.get("write_env") or {}).values()]
    tmps = [str((r.get("effective_policy") or {}).get("tmp") or "") for r in reports]
    write = [str(p) for p in eff.get("write") or []] + env_dirs + tmps
    read = [str(p) for p in eff.get("read") or []] + write
    named = [w for w in _strings(event.get("args")) if w and "\n" not in w and len(w) < 4096] + ABS_PATH.findall(content)
    for word in named:
        path = os.path.realpath(os.path.join(ws, word))
        if not _under(path, read) and (perm or acces or rofs or (noent and exists(path))):
            return True
        if _under(path, read) and not _under(path, write) and (perm or rofs):
            return True
    for cut in (n for r in reports for n in r.get("narrowed") or []):
        if cut.get("capability") == "net" and NET.search(content):
            return True
        wanted = str(cut.get("requested") or "")
        if cut.get("capability") != "net" and wanted and any(wanted in w for w in named + [content]):
            return True
    return False


def classify_event(event, stop="", last_turn=None, reports=None):
    """One messages.jsonl `tool` event with error true -> a class, `tool_exit`, or None (not an error).
    `reports`: {tool name: [cs-sandbox report]} for the run (finalize-run); none = sandbox_denied never assigned."""
    if event.get("event") != "tool" or not event.get("error"):
        return None
    content = str(event.get("content") or "")
    if content.startswith("pre_tool ") and " refused: " in content.split("\n", 1)[0]:
        return "hook_rejected"
    if content.endswith("exit: exit status 101") and re.search(r"(?m)^cs-sandbox: ", content):
        return "sandbox_unenforceable"
    if content.endswith("\nexit: signal: killed"):
        if stop in ("budget:seconds", "interrupted") and event.get("turn") == last_turn:
            return "tool_exit"  # the run's own deadline killed it; the run's stop already says so
        return "tool_timeout"
    if sandbox_denied(event, (reports or {}).get(event.get("name"))):
        return "sandbox_denied"
    return "tool_exit"


def call_errors(events, stop="", last_turn=None, reports=None):
    """{class: count} over a run's tool events; empty when no call failed."""
    counts = {}
    for event in events:
        cls = classify_event(event, stop, last_turn, reports)
        if cls:
            counts[cls] = counts.get(cls, 0) + 1
    return counts


def sandbox_tools(events, stop="", last_turn=None, reports=None):
    """{sandbox class: [tool names, first-seen order]}: which tools the sandbox denied or could not confine."""
    named = {}
    for event in events:
        cls = classify_event(event, stop, last_turn, reports)
        if cls in SANDBOX_CLASSES and event.get("name") not in named.setdefault(cls, []):
            named[cls].append(event.get("name"))
    return named


def row_error(row):
    """The row's error object: a version 2 row's own, a version 1 row's classified from its stop on read."""
    row = row or {}
    if row.get("row_version", 1) >= 2:
        return row.get("error")
    return classify_stop(row.get("stop"))
