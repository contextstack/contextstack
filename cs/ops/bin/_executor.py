#!/usr/bin/env python3
# Private implementation of the gateway-owned CLI work order.
# Run a CLI against the frozen goal and return its execution receipt.
# Dest merge is parked (operator_close).
from _yaml import load as load_yaml
from _estate_io import state_dir
import atexit, hashlib, json, os, re, shutil, subprocess, sys, time, signal, threading, tempfile
from pathlib import Path


from _estate_io import run_op as estate_run_op
import _lanes
from functools import partial

from _agent_stream import CursorAvailability, CursorEditRefusals, Events, Mapper, append as append_event, dialect, run_streaming, signals as stream_signals
from _provider_limits import detect_limit, enrich_allowance, limit_signal, record_limit
from _cli_native import stage_copies, finish_copies, stage_claude_login, snapshot_login_files, stage as stage_native, terminal_stop, unstage as unstage_native, verify_login_files
from _est import parse_usage
import _run_record
from _job_reason import persist_output, redact
from _stuck import Monitor as StuckMonitor, parse_budget_stuck, write_report as write_stuck_report
from _roots import NAMES as ROOT_ENV, task_label, transcript_id
from _frozen_inputs import stage_frozen_inputs
from _grant import covers_path, cover
from _muse import Cancelled, run_process
from _setup_workspace import materialize

HERE = Path(__file__).resolve().parent
CODE_ROOT = HERE.parent.parent
TEMPLATES = HERE / "executor-templates.json"
HOME = Path.home()
KEEP_ENV = {
    "PATH", "HOME", "USER", "LOGNAME", "TERM", "LANG", "LC_ALL", "LC_CTYPE",
    "TMPDIR", "TMP", "TEMP", "SHELL", "HOMEBREW_PREFIX", "HOMEBREW_CELLAR",
    "CS_SCHEDULE_DATE", "CS_SCHEDULE_SLOT", "CS_SCHEDULE_TIMEZONE",
    "CS_JOB_ID", "CS_TRIAL", "CS_VIDEO_MUTATION",
}
CRED_ENV = re.compile(
    r"(API[_-]?KEY|SECRET|TOKEN|PASSWORD|CREDENTIAL|^AWS_|SSH_AUTH_SOCK)",
    re.I,
)
AUTH_EXPIRED = re.compile(
    r"Not signed in|Failed to authenticate.*401|OAuth (?:access token has been revoked|(?:access token has|session) expired)|api_error_status[\"']?\s*[:=]\s*401",
    re.I | re.S,
)
# Broader sign-in failures, used only for CLIs with a trusted login refresh (Pi, Codex, Grok):
# a generic "Authentication required" from another CLI must not pause it with no way to recover.
LOGIN_EXPIRED = re.compile(
    r"Not signed in|signed out|not logged in|please (?:log|sign) in|"
    r"refresh[_ ]token[_ ](?:expired|reused|invalid)|invalid_grant|Failed to authenticate.*401|"
    r"(?:token refresh|refresh token) (?:failed|error|has expired|has already been used)|"
    r"Authentication required|not authenticated|No (?:API key|credentials) found|"
    r"OAuth (?:access token has been revoked|(?:access token has|session) expired)|"
    r"api_error_status[\"']?\s*[:=]\s*401",
    re.I | re.S,
)
LOGIN_REFRESH_CLIS = ("pi", "codex", "grok")


def fail(msg):
    sys.exit("executor: " + msg)


def spec_sha(path):
    if not path or not path.is_file():
        return ""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def spec_trials(spec):
    try:
        return max(1, int(spec.get("trials") or 1))
    except (TypeError, ValueError):
        return 1


def spec_hypothesis(spec):
    return str(spec.get("hypothesis") or "")


def budget_turns(spec, budget, default=200):
    for source in (budget, (spec or {}).get("budget") if isinstance(spec, dict) else None):
        raw = source.get("turns") if isinstance(source, dict) else None
        if type(raw) is int and raw > 0:
            return raw
    return default


def names_executor(spec, name):
    """A spec chooses its executor deliberately: its `model:` is the executor's name, or its hypothesis names it. A spec where neither does is refused. client-web sends no hypothesis."""
    return spec.get("model") == name or name in spec_hypothesis(spec)


def allowlist_env(extra_drop, tmpdir, workspace=None):
    drop = {str(k) for k in (extra_drop or [])}
    from _toolchains import lane_path, toolchain_lane_env
    path = lane_path(os.environ.get("PATH", ""))
    out = {"GIT_TERMINAL_PROMPT": "0", "PATH": path, "TMPDIR": str(tmpdir), "TMP": str(tmpdir), "TEMP": str(tmpdir)}
    out.update(toolchain_lane_env(tmpdir, workspace))
    for k, v in os.environ.items():
        if k in drop or CRED_ENV.search(k) or k not in KEEP_ENV:
            continue
        if k in ("PATH", "TMPDIR", "TMP", "TEMP"):
            continue
        out[k] = v
    return out


def trusted_op_env(env):
    return dict(env, **{key: os.environ[key] for key in ROOT_ENV if key in os.environ})


def research_prompt(repo_view, goal):
    return f"Research the question using the read-only repository snapshot at {repo_view}. Do not commit.\n\n{goal}"


def require_executor_sandbox():
    # Host-side opt-in only; never inherited by the coding CLI.
    if sys.platform != "darwin" and os.environ.get("CS_EXECUTOR_ALLOW_UNSANDBOXED") != "1":
        fail("executor sandbox not available on %s (development opt-in: CS_EXECUTOR_ALLOW_UNSANDBOXED=1)" % sys.platform)


def sandbox_cmd(binary, args, ws, tmpdir, profile_path, protected=None, read_links=(), login_writes=None,
                read_roots=(), home_reads=(), *, env=None, policy=None, caller=None):
    from _estate_io import native_sandbox_request, render_native_sandbox
    require_executor_sandbox()
    if sys.platform != "darwin":
        return [binary, *args]
    # Render against the host environment, as the Python renderer did: the child env strips
    # SSH_AUTH_SOCK, and the host's real agent socket must still get its explicit deny.
    env = os.environ if env is None else env
    policy = CODE_ROOT / "policy/sandbox.json" if policy is None else policy
    request = native_sandbox_request(binary, ws, tmpdir, HOME, protected=protected,
                                     read_links=read_links, login=login_writes,
                                     read_roots=read_roots, home_reads=home_reads, env=env, caller=caller)
    profile_path.write_bytes(render_native_sandbox(CODE_ROOT, policy, request))
    return ["sandbox-exec", "-f", str(profile_path), binary, *args]


def claude_auth_refresh_cmd(binary, args):
    if Path(binary).name != "claude" or "--model" not in args:
        return None
    model = args[args.index("--model") + 1]
    return [
        binary, "--print", "--output-format", "text", "--model", model,
        "--setting-sources", "", "--settings", '{"disableAllHooks":true}',
        "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
        "--tools", "", "--disable-slash-commands",
        "Reply with exactly: AUTH_OK",
    ]


def changed_paths(ws, base, lane=None, *, env=None):
    lane_ctx = {"main": lane["main"], "lane": lane["lane"]} if lane else None
    if lane_ctx:
        return _lanes.workspace_changed_paths(ws, base, lane_ctx=lane_ctx, env=env)
    names = []
    for args in (
        ("diff", "--name-only", "%s..HEAD" % base),
        ("diff", "--cached", "--name-only"),
        ("diff", "--name-only"),
        ("ls-files", "--others", "--exclude-standard"),
    ):
        done = _lanes.workspace_git(ws, *args, lane_ctx=None, env=env)
        if done.returncode != 0:
            fail("git %s: %s" % (args[0], (done.stderr or done.stdout or "").strip()))
        names.extend((done.stdout or "").splitlines())
    seen, out = set(), []
    for n in names:
        n = n.strip()
        if n and n not in seen:
            seen.add(n)
            out.append(n)
    return out


def changed_state(ws, base, paths=None):
    selected = list(paths) if paths is not None else changed_paths(ws, base)
    state = {}
    root = Path(ws).resolve()
    for rel in selected:
        path = root / rel
        try:
            path.relative_to(root)
            resolved = path.resolve()
            resolved.relative_to(root)
            if path.is_symlink():
                value = ("link:" + os.readlink(path)).encode()
            elif path.is_file():
                digest = hashlib.sha256()
                with path.open("rb") as stream:
                    for chunk in iter(lambda: stream.read(65536), b""):
                        digest.update(chunk)
                value = ("sha256:" + digest.hexdigest()).encode()
            elif path.exists():
                value = b"directory"
            else:
                value = b"missing"
        except (OSError, ValueError):
            value = b"outside-or-unreadable"
        state[str(rel)] = hashlib.sha256(value).hexdigest()
    return state



def main(argv=None) -> int:
    trace_started = _run_record.trace_clock()
    argv = sys.argv[1:] if argv is None else list(argv)
    if argv != ["--gateway-owned", "--execution-only"]:
        fail("expected gateway-owned execution")
    receipt_path = os.environ.pop("CS_CLI_RECEIPT_PATH", None)
    reserved_id = os.environ.pop("CS_RUN_ID", None)
    execution_id = os.environ.pop("CS_EXECUTION_ID", None)
    if not (receipt_path and execution_id and reserved_id
            and re.fullmatch(r"[0-9][A-Za-z0-9._-]{0,127}", reserved_id)):
        fail("invalid CLI receipt binding")
    receipt_channel = open(receipt_path, "w", encoding="utf-8")
    # Keep only the private, non-inheritable descriptor while running the agent.
    os.unlink(receipt_path)
    atexit.register(receipt_channel.close)
    a = json.loads(sys.stdin.readline())
    approval_token = a.pop("_cs_action_binding", None)
    lane_binding = a.pop("_cs_lane_binding", None)

    def owner_lifetime():
        sys.stdin.read()
        os.killpg(os.getpgrp(), signal.SIGTERM)

    threading.Thread(target=owner_lifetime, daemon=True).start()
    name = str(a.get("executor") or "").strip()
    templates = json.loads(TEMPLATES.read_text())
    tmpl = templates.get(name)
    if not tmpl:
        fail("unknown executor %s (templates: %s)" % (name, ", ".join(sorted(templates))))
    require_executor_sandbox()
    task = Path(str(a.get("task") or ""))
    if not task.is_absolute():
        task = CODE_ROOT / task
    if not (task / "goal.md").is_file() or not (task / "check").is_file():
        fail("task needs goal.md and check")
    dest = str(a.get("dest") or "").strip()
    raw_grant = a.get("grant")
    if isinstance(raw_grant, str):
        gid, extra_paths, flags = raw_grant.strip(), [], {}
    elif isinstance(raw_grant, dict):
        gid = str(raw_grant.get("id") or "").strip()
        extra_paths = [str(p) for p in (raw_grant.get("paths") or []) if str(p).strip()]
        flags = raw_grant
    else:
        gid, extra_paths, flags = "", [], {}
    if dest or (flags.get("merge") is True) or (flags.get("commit") is True):
        fail("executor dest merge is parked; operator_close dest")
    grant_paths = []
    if gid:
        ok, err, grant_file = cover(gid, repo=None, paths=[], commit=False)
        if not ok:
            fail(err)
        file_paths = grant_file.get("paths") or []
        if extra_paths:
            bad = [p for p in extra_paths if not covers_path(p, file_paths)]
            if bad:
                fail("grant.paths not a subset of grant file: " + ", ".join(bad))
            grant_paths = extra_paths
        else:
            grant_paths = list(file_paths)
    elif extra_paths:
        fail("inline grant.paths is not authority")

    binary = str(tmpl.get("binary") or name)
    if binary.startswith("~/"):  # grok's install lives at ~/.grok/bin/grok, off a launchd PATH
        binary = str(HOME / binary[2:])
    spec_rel = str(a.get("spec") or "")
    spec_path = (CODE_ROOT / spec_rel) if spec_rel else None
    if spec_rel:
        if not spec_path or not spec_path.is_file():
            fail("spec not found: %s" % spec_rel)
        try:  # the spec as YAML, whichever style the client or the Director's in-place edits left it in
            spec = load_yaml(spec_path)
        except (OSError, ValueError) as error:
            fail("spec is not YAML: %s" % error)
        if not isinstance(spec, dict):
            fail("spec is not a YAML mapping")
        if not names_executor(spec, name):
            fail("neither the spec's model nor its hypothesis names executor %s" % name)
        trial_limit = spec_trials(spec)
    else:
        spec = {}
        trial_limit = None
    # Refuse before setup, workspace creation, or any hosted session code runs.
    try:
        intent_fields = _run_record.spec_intent(spec)
    except ValueError as error:
        fail(str(error))

    # Only the hosted-lead spec takes the hosted path; ordinary conversation turns (tasks/turn with any other spec)
    # run as normal executor jobs (6th pass R1: keying on task.name refused every ordinary turn).
    hosted_turn = str(spec.get("id") or "") == "hosted-lead"
    if hosted_turn:
        from _lead_host import authorize_hosted_turn
        reason = authorize_hosted_turn(a, task, spec, CODE_ROOT)
        if reason:
            fail("hosted_turn_refused: " + reason)

    try:
        stuck_config = parse_budget_stuck(spec.get("budget") if isinstance(spec, dict) else None)
    except ValueError as error:
        fail(str(error))
    trial = int(a["trial"]) if a.get("trial") not in (None, "") else 1
    if trial_limit is not None and trial > trial_limit:
        fail("trial %d exceeds spec trials %d" % (trial, trial_limit))
    timeout = int(a.get("timeout_s") or 300)
    raw_budget = a.get("budget")
    spec_budget = spec.get("budget") if isinstance(spec, dict) else None
    job_budget = raw_budget if isinstance(raw_budget, dict) else (spec_budget if isinstance(spec_budget, dict) else None)
    limits = None
    check_window = int(a.get("check_window") or 60)
    budget_broker = None
    if spec_rel and Path(binary).name == "claude":
        from _cli_budget import declared, Broker, claude_args, native_result
        try:
            parsed, frozen_window = declared(spec)
        except ValueError as error:
            fail(str(error))
        if ((raw_budget is not None and parsed != raw_budget)
                or ('check_window' in a and check_window != frozen_window)
                or ('timeout_s' in a and timeout != parsed['seconds'])):
            fail("work order budgets differ from the spec")
        limits, check_window, timeout = parsed, frozen_window, parsed['seconds']
        job_budget = limits
        if sys.platform != "darwin":
            fail("budget hook isolation requires macOS")
    else:
        # Ordinary jobs may name only some fields; refuse non-finite or negative values without
        # importing _cli_budget (frozen executor copies include it only for declared limits).
        import math
        for key, value in (job_budget or {}).items() if isinstance(job_budget, dict) else ():
            if key in ("turns", "tool_calls", "usd", "seconds") and isinstance(value, (int, float)) \
                    and not isinstance(value, bool) and (not math.isfinite(value) or value < 0):
                fail("work order budget %s must be a finite number >= 0" % key)
        from _harness_usage import usd_budget_refusal
        refusal = usd_budget_refusal(tmpl, job_budget)
        if refusal:  # before any run dir or CLI: a missing dollar report is not $0
            fail(refusal)
        if raw_budget is not None and Path(binary).name == "claude":
            fail("declared CLI budgets require a Claude spec")

    from _harness_usage import resolve_harness_cost
    from _job_memory import job_memory_cap_bytes

    cap_bytes = job_memory_cap_bytes(spec=spec if spec_rel else None, limits=job_budget, config_root=CODE_ROOT)
    memory_cap_mb = cap_bytes // (1024 * 1024) if cap_bytes else None

    cost_class, unmetered = resolve_harness_cost(tmpl, tmpl.get("args"))
    if cost_class not in ("free_local", "subscription", "api"):
        fail("template %s missing cost_class" % name)
    auth = str(tmpl.get("auth") or "").strip()
    if "cost_usd" in tmpl:
        try:
            cost_usd = float(tmpl["cost_usd"])
        except (TypeError, ValueError):
            fail("template cost_usd is not a number")
    elif cost_class == "api" and unmetered:
        cost_usd = None  # not measured yet; settle_cost keeps a report and never invents 0
    elif cost_class == "api":
        fail("api cost_class has no measured amount; refuse rather than invent $0")
    else:
        cost_usd = 0.0
    if cost_class in ("free_local", "subscription") and cost_usd != 0:
        fail("cost_usd must be 0 for %s (wallet; never fabricate spend)" % cost_class)
    default_rate = 0.002 if cost_class == "free_local" else 0.015
    if unmetered and "cost_per_1k" not in tmpl:
        cost_per_1k = 0.0  # no published rate: do not price an unknown provider at the default
    else:
        try:
            cost_per_1k = float(tmpl["cost_per_1k"]) if "cost_per_1k" in tmpl else default_rate
        except (TypeError, ValueError):
            fail("template cost_per_1k is not a number")
    auth_verified = auth == "none"
    # A repo lane (_lanes): the spec names `repo:` and `lane:`, and the agent works in that lane's worktree beside the
    # repo's main checkout instead of a fresh workspace. Refused here, before any run effect, when it cannot be.
    lane = repo_config = repo_view = None
    repo_denies = []
    if spec.get("repo_read_only") is True:
        if not spec.get("repo") or spec.get("lane"):
            fail("a read-only repo spec requires repo: and omits lane:")
        if sys.platform != "darwin":
            fail("read-only repo workspaces require the macOS executor sandbox")
        try:
            repo_config = _lanes.repo_config(str(spec["repo"]))
        except _lanes.LaneError as error:
            fail("repo: %s" % error)
    elif spec.get("repo") is not None or spec.get("lane") is not None:
        if not (spec.get("repo") and spec.get("lane")):
            fail("a lane spec names both repo: and lane:")
        if name == "muse-code":
            fail("Muse repo lanes unsupported: private HOME/Keychain lifecycle requires a fresh workspace")
        if (task / "setup").exists() or a.get("inputs"):
            fail("a lane task has no setup/ or inputs: the agent works in the lane's worktree")
        try:
            cfg = _lanes.repo_config(str(spec["repo"]))
            name_of_lane = str(spec["lane"])
            _lanes.check_lane(cfg, name_of_lane)
            lease = _lanes.lane_execution(cfg["path"], name_of_lane, lane_binding)
            lease.__enter__()
            atexit.register(lease.__exit__, None, None, None)
            lane = _lanes.ensure_lane(cfg, name_of_lane)
        except _lanes.LaneError as error:
            fail("lane: %s" % error)
    run_id = reserved_id
    run_dir = state_dir(CODE_ROOT) / "runs" / run_id
    if run_dir.is_symlink() or not run_dir.is_dir() or os.environ.get("CS_TRIAL") != str(trial):
        fail("CLI run reservation missing or mismatched")
    approval_prefix = None
    approval_job = os.environ.get("CS_JOB_ID")
    cli_name = Path(binary).name
    if approval_token is not None and cli_name in ("codex", "cursor-agent", "claude") and not hosted_turn and not repo_config:
        import _lane_mcp
        if _lane_mcp.consume_binding(run_dir, approval_job, approval_token):
            approval_prefix = _lane_mcp.action_prefix()
    approval_bridge = None
    ws = run_dir / "workspace"
    if lane:
        ws = Path(lane["path"])
    elif os.environ.get("CS_WORKSPACES_ROOT"):  # a separate workspaces root: where finalize-run and the runner look
        from _roots import workspace_dir
        ws = workspace_dir(run_dir)
    cli_tmp = run_dir / "tmp"
    run_dir.mkdir(parents=True, exist_ok=True)
    cli_tmp.mkdir(parents=True)
    trace_task = "tasks/turn" if hosted_turn else str(a["task"])
    trace = _run_record.ObservedTrace(run_dir, CODE_ROOT, trace_task, trial, trace_started, execution_id)
    child_env = allowlist_env(tmpl.get("env_drop") or [], cli_tmp, ws)
    run_op = partial(estate_run_op, env=trusted_op_env(child_env))
    from _estate_io import link_target  # a ../../../ledger/... setup link resolves through the state root
    setup = link_target(CODE_ROOT, task / "setup")
    if lane:
        pass
    elif setup.is_dir():
        try:
            materialize(setup, ws, task=task, code_root=CODE_ROOT, env=child_env)
        except (OSError, ValueError) as error:
            fail("copy setup: " + str(error))
    else:
        ws.mkdir(parents=True)
    try:
        frozen_inputs = stage_frozen_inputs(a.get("inputs"), ws)
    except (OSError, ValueError) as error:
        fail("stage frozen inputs: " + str(error))
    if not (ws / ".git").exists():
        for args in (("init", "-q"), ("add", "-A")):
            done = _lanes.workspace_git(ws, *args, env=child_env)
            if done.returncode != 0:
                fail("git %s: %s" % (args[0], (done.stderr or done.stdout or "").strip()))
        subprocess.run(
            ["git", "-C", str(ws), *_lanes.LANE_GIT_HARDEN, "-c", "user.name=ContextStack", "-c", "user.email=cs@local",
             "commit", "--allow-empty", "-q", "-m", "executor base"],
            capture_output=True, env=child_env,
        )

    if lane:
        base = _lanes.lane_out(lane["main"], lane["lane"], ws, "rev-parse", "HEAD", env=child_env)
    else:
        head = _lanes.workspace_git(ws, "rev-parse", "HEAD", env=child_env)
        if head.returncode != 0:
            fail("git rev-parse HEAD: " + (head.stderr or head.stdout or "").strip())
        base = (head.stdout or "").strip()
    if not base:
        fail("workspace has no HEAD to pin as grant base")
    (run_dir / "base.sha").write_text(base + "\n", encoding="utf-8")

    if repo_config:
        def cancel_setup(_signum, _frame):
            raise SystemExit(128 + signal.SIGTERM)

        signal.signal(signal.SIGTERM, cancel_setup)
        snapshot = Path(tempfile.mkdtemp(prefix="read-only-", dir=run_dir))
        repo_view = snapshot / "repo"

        def discard_unfinished_snapshot():
            if not (run_dir / "execution-receipt.json").is_file():
                shutil.rmtree(snapshot, ignore_errors=True)

        atexit.register(discard_unfinished_snapshot)
        try:
            repo_denies = _lanes.prepare_repo_view(repo_config, repo_view)
        except (OSError, subprocess.CalledProcessError, _lanes.LaneError) as error:
            fail("repo: %s" % error)

    def send_receipt(receipt):
        json.dump(receipt, receipt_channel, allow_nan=False)
        receipt_channel.close()
        raise SystemExit(0)

    if name == "muse-code" or tmpl.get("profile") == "cli-profiles.json":
        from _muse_adapter import run
        run({**globals(), **locals()})
        raise SystemExit(0)

    stim, stim_err = run_op(CODE_ROOT, "capture-stimulus", {
        "run_dir": str(run_dir), "run_id": run_id, "task": str(task),
        "spec": spec_rel, "spec_sha": spec_sha(spec_path) if spec_path else "",
        "trial": trial, "adapter": "executor", "workspace": str(ws),
        "grant": gid or "", "grant_known": True, "authority": "work order",
        "executor_files": ["ops/bin/_executor.py", "ops/bin/_cli_budget.py"] if limits is not None else [],
    })
    if stim_err:
        fail("capture-stimulus: " + stim_err)
    stim_dir = run_dir / "stimulus"
    # The CLI is prompted from the frozen goal, never the live task.
    from _muse_adapter import goal_with_context
    goal = goal_with_context(stim_dir)
    prompt = (
        "Implement the task in the current working directory. "
        "Do not leave this directory. Do not commit unless asked.\n\n" + goal
    )
    if repo_view:
        prompt = research_prompt(repo_view, goal)
    if approval_prefix:
        prompt = _lane_mcp.DISCOVERY + "\n\n" + prompt
    turns = str(budget_turns(spec, job_budget))
    args = []
    for part in tmpl.get("args") or []:
        part = part.replace("{{workspace}}", str(ws)).replace("{{turns}}", turns)
        args.append(part.replace("{{prompt}}", prompt).replace("{{goal}}", goal))

    cost_class, unmetered = resolve_harness_cost(tmpl, args)

    row = {
        **intent_fields,
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "spec": spec_rel, "spec_sha": spec_sha(spec_path) if spec_path else "",
        "task_sha": stim.get("task_sha", ""), "env_sha": stim.get("env_sha", ""),
        "stimulus_sha": stim.get("stimulus_sha", ""),
        "model": name, "task": task_label(task, CODE_ROOT),
        "trial": trial,
        "cost_usd": cost_usd, "cost_class": cost_class, "auth": auth,
        "est_usd": 0, "est_basis": "not_invoked",
        "auth_verified": auth_verified,
        "turns": 0, "tool_calls": 0,
        "transcript": transcript_id(run_id),
        "base_sha": base, "note": cost_class,
    }
    if unmetered:
        row["unmetered"] = True
    if gid:
        row["grant"] = gid
    if lane:
        row["lane"] = {k: lane[k] for k in ("repo", "lane", "path", "branch", "base", "dirty_at_start")}
    if frozen_inputs:
        row["frozen_inputs"] = frozen_inputs


    login_refresh_status = None
    native_copies = []  # Host-only staging snapshots; only status enums enter evidence.
    native_links = []  # _cli_native links into personal files: gone before any evidence is kept


    def finalize_login_copies(expired=False):
        nonlocal login_refresh_status
        results = []
        if native_copies:
            results = finish_copies(native_copies)
            row["login_copy_refresh"] = results
            try:
                fd = os.open(run_dir / "login-copy-refresh.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                with os.fdopen(fd, "w") as stream:
                    stream.write(json.dumps(row["login_copy_refresh"]) + "\n")
            except OSError:
                pass  # The ledger row still records the result; never follow a planted evidence link.

        if (not expired and not results) or Path(binary).name not in ("pi", "codex", "grok") or login_refresh_status is not None:
            return
        from _login_refresh import maintain
        from _provider_limits import notice
        def refresh_sandbox(command, refresh_root):
            refresh_caller = {"deny_write_roots": [str(ws.resolve()), str(cli_tmp.resolve())],
                              "run_protection": {"run_root": str(run_dir.resolve()),
                              "workspace": str(refresh_root), "tmpdir": str(refresh_root),
                                                 "action_capabilities": True,
                              "refresh_write_paths": [str(refresh_root)]}}
            return sandbox_cmd(command[0], command[1:], refresh_root, refresh_root,
                               run_dir / "sandbox-login-refresh.sb", read_roots=(refresh_root,),
                               policy=sandbox_policy, caller=refresh_caller)
        login_refresh_status = maintain(binary, args, results, expired, home=HOME,
            root=run_dir / "trusted-login-refresh", env=cli_env, sandbox=refresh_sandbox)
        if login_refresh_status is not None:
            row["trusted_login_refresh"] = login_refresh_status
            # Notify and pause only when the job reported a sign-in failure the trusted refresh did not fix.
            # A failed maintenance refresh after a successful job, or a quota error, is not a dead login.
            if expired and login_refresh_status not in ("written_back", "unchanged"):
                notice(state_dir(CODE_ROOT), name, Path(binary).name, home=HOME)


    def cleanup():
        if approval_bridge is not None:
            atexit.unregister(approval_bridge.close)
            approval_bridge.close()
        finalize_login_copies()
        unstage_native(native_links)
        if budget_broker is not None:
            row["tool_calls"] = budget_broker.admission.count

    def emit(verdict, stop, extra=None):
        complete("terminal", stop, verdict=verdict, extra=extra)

    import _machine_finish as finish

    spec_id = str(spec.get("id") or "") or spec_rel
    started = time.time()
    if limits is not None:
        budget_broker = Broker(limits['tool_calls'])
        atexit.register(budget_broker.close)
        budget_controls = run_dir / 'budget-controls'
        budget_controls.mkdir()
        args = claude_args(args, limits, budget_broker.hook(budget_controls))
        row.update(budget=limits, check_window=check_window, budget_enforcement='claude-native-and-parent-tool-admission', tool_call_count_scope='admitted_native_tools', turns=None)
    if memory_cap_mb is not None:
        row["memory_cap_bytes"] = memory_cap_mb * 1024 * 1024

    row["cli_invoked"] = False

    from _estate_io import native_caller
    sandbox_caller, sandbox_read_roots = native_caller(tmpl, HOME, run_dir, ws, cli_tmp, lane)
    sandbox_caller["run_protection"]["action_capabilities"] = True
    if repo_view:
        sandbox_caller["run_protection"].pop("workspace")
        denied = [*sandbox_caller["deny_write_roots"], *repo_denies, str(ws.resolve())]
        sandbox_caller["deny_write_roots"] = denied
        sandbox_read_roots = (*sandbox_read_roots, repo_view)
    sandbox_policy = CODE_ROOT / "policy/sandbox.json"


    def record_process_evidence(stage, rows):
        for evidence in rows:
            append_event(run_dir / "process-evidence.jsonl", dict(evidence, stage=stage))


    protected = budget_controls if budget_broker else None

    def complete(mode, stop, verdict=None, extra=None):
        signal.signal(signal.SIGTERM, lambda *_: None)
        (cli_tmp / "claude-home/Library/Keychains").unlink(missing_ok=True)
        cleanup()
        receipt = finish.execution_receipt(
            run_dir, ws, row, spec_id, str(a.get("task") or ""), task.name, started,
            check_window, mode, stop, verdict, extra, trace, lane, bool(repo_view), grant_paths,
            sandbox_policy, sandbox_caller, sandbox_read_roots, protected, tmpl.get("env_drop") or [],
            memory_cap_mb, child_env, execution_id=execution_id,
        )
        send_receipt(receipt)

    def hosted_finish():
        complete("hosted", "done")

    def cancel_staged(_signum, _frame):
        row["seconds"] = time.time() - started
        emit("fail", "cancelled", extra={"note": "execution cancelled"})

    signal.signal(signal.SIGTERM, cancel_staged)

    if hosted_turn:
        from _lead_host import run_hosted_lead_turn
        run_hosted_lead_turn({**globals(), **locals()})
        raise SystemExit(0)


    # A deterministic task preflight may prove that a static scheduled rank has no
    # work before an expensive subscription CLI is launched. Exit 0 means run;
    # exit 3 plus {"run":false,"reason":"..."} is a checked no-work pass.
    preflight = task / "preflight"
    if preflight.is_file():
        try:
            probe = subprocess.run([str(preflight)], cwd=str(ws), env=child_env,
                                   capture_output=True, text=True,
                                   timeout=min(timeout, 30))
        except subprocess.TimeoutExpired:
            row["seconds"] = time.time() - started
            emit("fail", "preflight_timeout")
        probe_log = "stdout:\n%s\nstderr:\n%s\nexit=%s\n" % (
            probe.stdout or "", probe.stderr or "", probe.returncode)
        persist_output(run_dir / "preflight.log", probe_log)
        try:
            probe_value = json.loads((probe.stdout or "").splitlines()[-1])
        except (IndexError, json.JSONDecodeError):
            row["seconds"] = time.time() - started
            row["note"] = "preflight returned malformed JSON"
            emit("fail", "preflight")
        if not isinstance(probe_value, dict) or type(probe_value.get("run")) is not bool:
            row["seconds"] = time.time() - started
            row["note"] = "preflight response contract differs"
            emit("fail", "preflight")
        if probe.returncode == 3 and probe_value["run"] is False:
            reason = str(probe_value.get("reason") or "").strip()
            if not reason:
                row["seconds"] = time.time() - started
                row["note"] = "preflight skip omitted reason"
                emit("fail", "preflight")
            row["seconds"] = time.time() - started
            row["preflight_exit"] = 3
            row["preflight_reason"] = reason
            row["note"] = "deterministic preflight: " + redact(reason)
            row["est_usd"], row["est_basis"] = 0, "not_invoked"
            persist_output(run_dir / "cli.log", probe_log)
            persist_output(run_dir / "transcript.md", probe_log)
            complete("semantic", "preflight_skip")
        if probe.returncode != 0 or probe_value["run"] is not True:
            row["seconds"] = time.time() - started
            row["note"] = "preflight refused execution"
            emit("fail", "preflight")

    from _cli_native import stage_git
    cli_env = child_env.copy()
    cli_env.update(stage_git(cli_tmp, HOME))
    def stage_claude_home(subdir, root=cli_tmp):
        # HOME isolates runtime writes; CLAUDE_CONFIG_DIR would change the Keychain
        # service in Claude 2.1.15. Only the optional login file is copied; personal config stays private.
        runtime_home = Path(root) / subdir
        if runtime_home.is_symlink() or runtime_home.is_file():
            runtime_home.unlink()
        elif runtime_home.exists():
            shutil.rmtree(runtime_home)
        runtime_home.mkdir(mode=0o700, exist_ok=False)
        if sys.platform == "darwin":
            # Match the profiled executor's Keychain discovery without copying keys.
            (runtime_home / "Library").mkdir(mode=0o700, exist_ok=False)
            keychain_link = runtime_home / "Library/Keychains"
            keychain_link.symlink_to(HOME / "Library/Keychains", target_is_directory=True)
            # The link serves the CLI only; run evidence never keeps a path to the real Keychains.
            atexit.register(keychain_link.unlink, missing_ok=True)
            native_links.append(keychain_link)
        if subdir == "claude-refresh-home":
            refresh_copies.extend(stage_claude_login(runtime_home, HOME))
        return str(runtime_home)


    if Path(binary).name == "claude":
        cli_env["HOME"] = stage_claude_home("claude-home")
    try:  # the template's own runtime dirs, private HOME and read-only login links (_cli_native)
        native_env, made = stage_native(tmpl, cli_tmp, HOME, copy_records=native_copies)
        login_snapshot = snapshot_login_files(tmpl, HOME)
    except (OSError, ValueError) as error:
        fail("stage %s runtime: %s" % (name, error))
    native_links.extend(made)
    cli_env.update(native_env)
    if approval_prefix:
        approval_bridge = _lane_mcp.start(ws, cli_tmp, run_dir, name, approval_job, approval_prefix)
        atexit.register(approval_bridge.close)
        args = _lane_mcp.stage(cli_name, args, cli_env, approval_bridge.config)
    cmd = sandbox_cmd(binary, args, ws, cli_tmp, run_dir / "sandbox.sb", budget_controls if budget_broker else None,
                      read_links=native_links, login_writes=tmpl, read_roots=sandbox_read_roots,
                      policy=sandbox_policy, caller=sandbox_caller)


    def timeout_output(value):
        if isinstance(value, bytes):
            return value.decode("utf-8", errors="replace")
        return str(value or "")


    def record_usage(log):
        """The agent's own usage report onto the row (_est.parse_usage). Codex keeps its allowance in
        the thread's rollout under CODEX_HOME (the run's private home, else ~/.codex); only usage-shaped lines."""
        nonlocal cost_usd
        codex_home = cli_env.get("CODEX_HOME") or os.environ.get("CODEX_HOME") or str(HOME / ".codex")
        sessions = Path(codex_home) / "sessions" if Path(binary).name == "codex" else None
        usage = parse_usage(log, prompt, cost_per_1k, codex_sessions=sessions, since=started)
        row.update(usage)
        trace.report_usage({"claude": "claude_code", "agy": "antigravity"}.get(Path(binary).name, Path(binary).name)
                           + "_native", usage)
        from _harness_usage import settle_cost
        cost_usd = settle_cost(row, cost_usd, unmetered, cost_class)


    # A streaming agent's task attempts are turns, as the runner's model calls are (_agent_stream):
    # live text to events.jsonl (and, under the gateway, each stdout line on to stdout); the reply, or an error that
    # voids the partial text, to messages.jsonl.
    stream_kind = dialect(binary)
    events = Events(run_dir) if stream_kind else None
    edit_refusals = CursorEditRefusals() if Path(binary).name == "cursor-agent" else None
    turn = 0
    stuck_monitor = None
    if events is not None and stuck_config.get("enabled", True):
        stuck_monitor = StuckMonitor(stuck_config, lambda paths=None: changed_state(ws, base, paths))


    def close_turn(mapper, error=None):
        events.write(turn, discard=error is not None, at=_run_record.utc_now(), timestamp_source="producer")
        event = {"event": "turn", "turn": turn, "text": redact(mapper.reply())} if error is None else \
            {"event": "error", "turn": turn, "error": redact(str(error))}
        append_event(run_dir / "messages.jsonl", event)


    def run_cli(command, working, limit, stage, env=None):
        nonlocal turn
        run_env = cli_env if env is None else env
        def process_evidence(rows):
            record_process_evidence(stage, rows)
        task_call = stage.startswith("task")
        mapper = None
        availability = CursorAvailability() if Path(binary).name == "cursor-agent" else None
        if events is not None and task_call:
            turn, mapper = turn + 1, Mapper(stream_kind)

            def on_line(raw, at, timestamp_source):
                sys.stdout.buffer.write(raw if raw.endswith(b"\n") else raw + b"\n")
                sys.stdout.flush()
                delta = mapper.text(raw)
                if delta:
                    events.write(turn, delta, at=at, timestamp_source=timestamp_source)
                if availability is not None:
                    unavailable = availability.feed(raw, progress=bool(delta))
                    if unavailable:
                        return unavailable
                if edit_refusals is not None:
                    refused = edit_refusals.feed(raw)
                    if refused:
                        return refused
                hit = limit_signal(stream_kind, raw)
                if hit:
                    return hit
                if stuck_monitor is not None:
                    decision = stuck_monitor.feed(stream_signals(stream_kind, raw))
                    if decision:
                        return decision
        try:
            call_started = time.monotonic()
            try:
                if limits is not None:
                    limit = min(limit, timeout - (time.time() - started))
                    if limit <= 0:
                        raise subprocess.TimeoutExpired(command, timeout)
                if not task_call:
                    return run_process(command, str(working), run_env, limit, memory_cap_mb=memory_cap_mb, on_evidence=process_evidence)
                done = run_streaming(command, str(working), run_env, limit,
                                     on_line if mapper is not None else lambda *_: None,
                                     on_idle=availability.poll if availability and mapper is not None else None,
                                     memory_cap_mb=memory_cap_mb, on_evidence=process_evidence, trace=trace)
                if mapper is None:
                    return done
                forced = getattr(done, "forced_meta", None)
                if forced:
                    close_turn(mapper, str(forced.get("reason") or forced.get("stop") or ("%s exited %s" % (stage, done.returncode))))
                else:
                    close_turn(mapper, None if done.returncode == 0 else "%s exited %s" % (stage, done.returncode))
                return done
            finally:
                row.setdefault("calls", []).append({"stage": stage, "per_call_latency_ms":
                    (time.monotonic() - call_started) * 1000})
        except (subprocess.TimeoutExpired, Cancelled) as error:
            timed_out = isinstance(error, subprocess.TimeoutExpired)
            if mapper is not None:
                close_turn(mapper, stage + (" timed out after %ss" % limit if timed_out else " cancelled"))
            row["seconds"] = time.time() - started
            row["cli_exit"] = None
            row["note"] = stage + (" timed out after %ss" % limit if timed_out else " cancelled")
            log = (
                "stage=%s\ntimeout after %ss: %s\nstdout:\n%s\nstderr:\n%s\n"
                % (stage, limit, error, timeout_output(error.stdout),
                   timeout_output(error.stderr))
            )
            persist_output(run_dir / "cli.log", log)
            persist_output(run_dir / "transcript.md", log)
            record_usage(log)
            try:
                verify_login_files(tmpl, HOME, login_snapshot)
            except ValueError:
                pass
            emit("fail", "timeout" if timed_out else "cancelled")


    row["cli_invoked"] = True
    cli = run_cli(cmd, ws, timeout, "task")

    # Claude refreshes subscription OAuth via securityd. Run the fixed no-tools refresh
    # under the same sandbox profile and a fresh private HOME with hooks/settings disabled,
    # then retry the real task inside the unchanged sandbox. API credentials stay absent.
    login_tampered = None
    try:
        verify_login_files(tmpl, HOME, login_snapshot)
    except ValueError as error:
        login_tampered = str(error)
    auth_text = (cli.stdout or "") + "\n" + (cli.stderr or "")
    refresh_cmd = claude_auth_refresh_cmd(binary, args)
    if (
        cli.returncode != 0
        and login_tampered is None
        and cost_class == "subscription"
        and limits is None
        and AUTH_EXPIRED.search(auth_text)
        and refresh_cmd is not None
    ):
        shutil.rmtree(cli_tmp / "claude-refresh-home", ignore_errors=True)
        refresh_copies = []
        refresh_home = Path(stage_claude_home("claude-refresh-home", root=run_dir))
        refresh_sb = run_dir / "sandbox-refresh.sb"
        refresh_caller = {**sandbox_caller, "run_protection": {**sandbox_caller["run_protection"],
                          "refresh_write_paths": [str(refresh_home.resolve())]}}
        refresh_sandbox_cmd = sandbox_cmd(refresh_cmd[0], refresh_cmd[1:], ws, cli_tmp, refresh_sb,
                                          budget_controls if budget_broker else None, read_links=native_links, login_writes=tmpl,
                                          read_roots=(refresh_home, *sandbox_read_roots),
                                          policy=sandbox_policy, caller=refresh_caller)
        row["auth_refresh"] = "started"
        row["auth_refresh_writeback"] = "skipped:refresh_incomplete"
        refresh = None
        try:
            refresh = run_cli(
                refresh_sandbox_cmd,
                cli_tmp, min(timeout, 60), "auth_refresh", env={**cli_env, "HOME": str(refresh_home)},
            )
        finally:
            trusted_refresh = refresh is not None and refresh.returncode == 0 and refresh.stdout in ("AUTH_OK", "AUTH_OK\n")
            results = finish_copies(refresh_copies, claude_refresh=trusted_refresh)
            row["auth_refresh_login_copy"] = results
            if refresh is not None:
                reason = "refresh_failed" if refresh.returncode != 0 else "unexpected_output"
                if trusted_refresh:
                    reason = results[0]["status"] if results else "no_copy"
                row["auth_refresh_writeback"] = "written" if reason == "written_back" else "skipped:" + reason
            (refresh_home / "Library/Keychains").unlink(missing_ok=True)
            shutil.rmtree(refresh_home, ignore_errors=True)
            refresh_sb.unlink(missing_ok=True)
        row["auth_refresh"] = "pass" if refresh.returncode == 0 else "fail"
        if refresh.returncode == 0:
            row["pre_auth_refresh_login_copy"] = finish_copies(native_copies)
            native_copies.extend(stage_copies(tmpl, cli_tmp, HOME))
            cli = run_cli(cmd, ws, timeout, "task_after_auth_refresh")

    try:
        verify_login_files(tmpl, HOME, login_snapshot)
    except ValueError as error:
        login_tampered = str(error)

    if Path(binary).name in LOGIN_REFRESH_CLIS:
        finalize_login_copies(expired=bool(LOGIN_EXPIRED.search(cli.stderr or "")) or
                              (bool(LOGIN_EXPIRED.search(auth_text)) and
                               (cli.returncode != 0 or terminal_stop(tmpl.get("terminal"), cli.stdout) is not None)))

    seconds = time.time() - started
    row["seconds"] = seconds
    forced_meta = getattr(cli, "forced_meta", None)
    if forced_meta and forced_meta.get("peak_rss_bytes") is not None:
        row["peak_rss_bytes"] = int(forced_meta["peak_rss_bytes"])
    if forced_meta and str(forced_meta.get("stop") or "") == "provider_limit":
        pl = forced_meta.get("provider_limit") if isinstance(forced_meta.get("provider_limit"), dict) else {}
        try:
            record_limit(state_dir(CODE_ROOT), name, dict(pl, template=name))
        except OSError:
            pass
    if forced_meta and str(forced_meta.get("stop", "")).startswith("stuck:") and stuck_monitor is not None and not (run_dir / "stuck.md").is_file():
        write_stuck_report(run_dir / "stuck.md", stuck_monitor.report(goal, forced_meta.get("stop"), forced_meta.get("reason")))
    log = "stdout:\n%s\nstderr:\n%s\nexit=%s\n" % (cli.stdout or "", cli.stderr or "", cli.returncode)
    persist_output(run_dir / "cli.log", log)
    persist_output(run_dir / "transcript.md", log)
    row["cli_exit"] = cli.returncode
    record_usage(log)

    # Re-read after optional auth refresh so revoked/expired tokens classify once.
    auth_text = (cli.stdout or "") + "\n" + (cli.stderr or "")
    native_stop = "done" if cli.returncode == 0 else "executor_exit_%s" % cli.returncode
    if native_stop == "done":  # a zero exit without the CLI's own completion receipt is not done
        native_stop = terminal_stop(tmpl.get("terminal"), cli.stdout) or "done"
    if forced_meta:
        native_stop = str(forced_meta.get("stop") or native_stop)
        if forced_meta.get("reason"):
            row["reason"] = str(forced_meta.get("reason"))
        if isinstance(forced_meta.get("stuck"), dict):
            row["stuck"] = forced_meta["stuck"]
        if native_stop == "provider_edit_refused":
            row["refused_paths"] = forced_meta["refused_paths"]
        if native_stop == "provider_limit":
            pl = forced_meta.get("provider_limit") if isinstance(forced_meta.get("provider_limit"), dict) else {}
            if pl:
                row["allowance"] = enrich_allowance(row.get("allowance"), pl)
    elif cli.returncode != 0 and Path(binary).name == "cursor-agent" and re.search(r"RetriableError:\s*\[unavailable\]", cli.stderr or ""):
        native_stop = "provider_unavailable"
        row["reason"] = ("Cursor could not reach its model (unavailable); "
                         "the model's quota may be used up — try another model")
    elif (Path(binary).name in LOGIN_REFRESH_CLIS and (LOGIN_EXPIRED.search(cli.stderr or "") or
            (native_stop != "done" and LOGIN_EXPIRED.search(auth_text)))) or (
            cli.returncode != 0 and AUTH_EXPIRED.search(auth_text)):
        # Auth death is a cohort-wide class: do not leave it as a generic executor_exit_
        # that would be misclassified as a generic executor failure.
        native_stop = "auth_expired"
    elif budget_broker is not None:
        try:
            native = native_result(cli.stdout, limits, budget_broker.admission)
            row.update(turns=native['turns'], tool_calls=native['tool_calls'])
            native_stop = native['stop'] if native_stop == 'done' else native_stop
            if native['stop'].startswith('budget:'):
                native_stop = native['stop']
        except (ValueError, TypeError, KeyError):
            if native_stop == 'done' or str(native_stop).startswith('executor_exit_'):
                native_stop = 'auth_expired' if AUTH_EXPIRED.search(auth_text) else 'native_terminal_error'
    # A run that succeeded wasn't limited, whatever its output says (the agent's work can quote a limit message).
    if not forced_meta and native_stop not in ("auth_expired", "provider_unavailable", "done"):
        limit_hit = detect_limit(name, Path(binary).name, cli.stdout, cli.stderr, row.get("allowance"))
        if limit_hit:
            row["allowance"] = enrich_allowance(row.get("allowance"), limit_hit)
            row["reason"] = limit_hit["message"]
            try:
                record_limit(state_dir(CODE_ROOT), name, limit_hit)
            except OSError:
                pass
            if native_stop != "done" or row.get("allowance", {}).get("rejected"):
                native_stop = "provider_limit"
    if login_tampered:
        native_stop = "login_file_tampered"
        row["note"] = login_tampered
    if native_stop == "auth_expired" and Path(binary).name in (*LOGIN_REFRESH_CLIS, "claude"):
        # Only CLIs whose login file is known: a re-login (file change) can then lift the pause.
        from _provider_limits import notice
        if login_refresh_status not in ("written_back", "unchanged"):
            notice(state_dir(CODE_ROOT), name, Path(binary).name, home=HOME)
    elif native_stop == "done" and login_refresh_status in (None, "written_back", "unchanged"):
        from _provider_limits import clear_pause
        clear_pause(state_dir(CODE_ROOT), name)
    complete("task", native_stop)


if __name__ == "__main__":
    sys.exit(main())
