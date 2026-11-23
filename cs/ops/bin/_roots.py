"""The four roots, one rule (Plan.CS.v2.1 §9). Not an op.

Authority sits here, as it does for the environment allowlist (_env_policy): the
launcher (tick, run-spec) calls roots() before launching anything and refuses on
RootsError (tick: a config-void row `stop: roots: ...`; run-spec: exit 2); every
op that touches state (finalize-run, capture-stimulus, tick)
validates again on its own entry. The Rust harness reads CS_STATE_ROOT and
CS_WORKSPACES_ROOT with the same defaults; a direct `cs-harness run`
with a bad root is refused by the first op that resolves it, and no row lands.

workspace_dir(run) -> the run's workspace under the workspaces root.
roots(code_root=None, env=None) -> {"code", "config", "state", "workspaces"}, each a Path:
  CS_CODE_ROOT        default: the checkout containing this file (`code_root` when given)
  CS_CONFIG_ROOT      default: code           (policy/, PINNED.md, ESTATE, prompts/)
  CS_STATE_ROOT       default: <code>/ledger  (runs/, ledger.jsonl, dispatch.jsonl, locks)
  CS_WORKSPACES_ROOT  default: <state>/runs   (a run's workspace stays inside its run dir)
A set root must be an absolute existing directory (CS_CODE_ROOT: one holding
INVARIANTS.md); state and workspaces may not lie inside the code root unless they
equal the defaults. Anything else is RootsError, which an op reports as a refused
input (exit 2) before it writes anything. A set root is taken by its real path
(symlinks resolved) and containment is lexical on real paths, with symlinks resolved. The defaults resolve a checkout’s local ledger.

A ledger row's `transcript` is "ledger/runs/<id>" in every layout: the run's
identifier under the state root, never a filesystem path; run_dir() resolves it.
"""
import os
from pathlib import Path

CODE_ROOT = Path(__file__).resolve().parent.parent.parent
NAMES = ("CS_CODE_ROOT", "CS_CONFIG_ROOT", "CS_STATE_ROOT", "CS_WORKSPACES_ROOT")
TRANSCRIPT_PREFIX = "ledger/runs/"


class RootsError(ValueError):
    """A CS_*_ROOT the rule refuses; the message names the variable and its value."""


def _root(env, name, default, outside=None):
    """One root: the variable when set (validated, unless it equals its default), else the default."""
    given = env.get(name) or ""
    path = os.path.normpath(given) if given else default
    if given and os.path.isabs(path) and os.path.exists(path):  # as Go's EvalSymlinks: a missing path stays as given
        path = os.path.realpath(path)
    if not given or (path == default and name != NAMES[0]):
        return path
    if name == NAMES[0]:
        ok, want = os.path.isfile(os.path.join(path, "INVARIANTS.md")), "ContextStack checkout (INVARIANTS.md)"
    else:
        ok, want = os.path.isdir(path), "directory" + (" outside the code root" if outside else "")
    if not os.path.isabs(path) or not ok or (outside and (path + "/").startswith(outside + "/")):
        raise RootsError('%s="%s": want an absolute existing %s' % (name, path, want))
    return path


def roots(code_root=None, env=None):
    """The four roots for this process; `env` defaults to os.environ, `code_root` to this checkout."""
    env = os.environ if env is None else env
    code = _root(env, NAMES[0], os.path.realpath(str(code_root or CODE_ROOT)))
    config = _root(env, NAMES[1], code)
    state = _root(env, NAMES[2], os.path.join(code, "ledger"), outside=code)
    workspaces = _root(env, NAMES[3], os.path.join(state, "runs"), outside=code)
    return {"code": Path(code), "config": Path(config), "state": Path(state), "workspaces": Path(workspaces)}


def runs_dir(code_root=None, env=None):
    """<state>/runs: the directory every run dir lives in."""
    return roots(code_root, env)["state"] / "runs"


def ledger_path(code_root=None, env=None):
    """<state>/ledger.jsonl."""
    return roots(code_root, env)["state"] / "ledger.jsonl"


def outbox_dir(code_root=None, env=None):
    """<state>/outbox: finalize-run's run envelopes awaiting a receiver (spool S1); acked ones under acked/."""
    return roots(code_root, env)["state"] / "outbox"


def jobs_dir(code_root=None, env=None):
    """<state>/jobs: one dir per director job a node accepted (job-pull): spec.yaml, tasks/, prompts/."""
    return roots(code_root, env)["state"] / "jobs"


def job_dir_of(spec, code_root=None, env=None):
    """The job dir holding `spec` when the spec lives directly in <state>/jobs/<job>/, else None."""
    try:
        spec, jobs = Path(os.path.realpath(str(spec))), Path(os.path.realpath(str(jobs_dir(code_root, env))))
    except (OSError, RootsError):
        return None
    return spec.parent if spec.is_absolute() and spec.parent.parent == jobs else None


def sandbox_reports_dir(code_root=None, env=None):
    """<state>/sandbox-reports: one dir per cell where cs-sandbox writes its per-call reports until finalize-run
    moves them into the run dir's sandbox/ (_sandbox_policy.cell_prefix)."""
    return roots(code_root, env)["state"] / "sandbox-reports"


def workspace_dir(run, code_root=None, env=None):
    """<workspaces>/<run id>/workspace: where the runner puts a run's workspace.
    In the default layout that is <run dir>/workspace, today's path byte for byte."""
    return roots(code_root, env)["workspaces"] / Path(run).name / "workspace"


def transcript_id(run_id):
    """A row's `transcript` for a run: its identifier in every layout, never a path."""
    return TRANSCRIPT_PREFIX + run_id


def task_label(task, code_root):
    """A row's `task`: relative to the code root when it lives there; a Director job's task
    (<state>/jobs/<id>/tasks/, outside the code root on an installed node) as its absolute path."""
    return str(task.relative_to(code_root)) if task.is_relative_to(code_root) else str(task)


def run_dir(run_id_or_transcript, code_root=None, env=None):
    """The run dir for a row's transcript ("ledger/runs/<id>") or a bare run id.
    Any other shape (a path, "..", empty) is ValueError: a run is named by its id, never by a path."""
    text = str(run_id_or_transcript)
    if text.startswith(TRANSCRIPT_PREFIX):
        text = text[len(TRANSCRIPT_PREFIX):]
    if not text or "/" in text or text in (".", ".."):
        raise ValueError("not a run id or transcript: %r" % (run_id_or_transcript,))
    return runs_dir(code_root, env) / text
