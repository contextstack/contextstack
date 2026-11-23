"""Machine-side launcher and receipt handoff for harness cells."""
import fcntl
import json
import os
import signal
import subprocess
import sys
import tempfile
from contextlib import ExitStack, contextmanager
from datetime import datetime, timezone
from pathlib import Path

from _env_policy import DEFAULT_ENV
from _estate_io import verify_frozen_stimulus, write_atomic
from _roots import runs_dir, transcript_id, workspace_dir


RECEIPT = "execution-receipt.json"


def finish_harness(code_root, run_dir, receipt, *, foreground=False):
    run_dir = run_dir.resolve()
    if run_dir.parent != runs_dir(code_root).resolve() or not run_dir.is_dir():
        raise ValueError("run_dir must be a directory under the runs root")
    row = receipt["row"]
    lane = row.get("harness_lane")
    workspace = Path(lane["path"]) if lane else workspace_dir(run_dir, code_root)
    if (type(receipt["version"]) is not int or receipt["version"] != 1
            or receipt["run_id"] != run_dir.name or receipt["finish"]["mode"] != "harness"
            or receipt != json.loads((run_dir / RECEIPT).read_text())
            or row["run_id"] != run_dir.name or Path(row["run_dir"]).resolve() != run_dir
            or row["transcript"] != transcript_id(run_dir.name)
            or Path(row["workspace"]).resolve() != workspace.resolve()
            or Path(row["stimulus_dir"]).resolve() != run_dir / "stimulus"
            or (row["trial"], row["spec"], row["task"], row["stop"], row["stimulus_sha"]) != (
                receipt["trial"], receipt["spec_id"], receipt["task_ref"],
                receipt["native"]["stop"], receipt["stimulus"]["sha256"])):
        raise ValueError("harness receipt differs from its run identity")

    # The harness holds this lock through process exit. Serialize promotion of
    # the receipt as well, so every caller uses the same terminal record.
    with (run_dir / "execution.lock").open("r+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        terminal = run_dir / "terminal.json"
        if not terminal.is_file():
            if row["stimulus_sha"]:
                verify_frozen_stimulus(run_dir, receipt["stimulus"])
            elif row["verdict"] != "void":
                raise ValueError("harness receipt has no frozen stimulus")
            trace = dict(receipt["trace"]["harness_trace"],
                         finalization_started=datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"))
            write_atomic(terminal, json.dumps(dict(row, harness_trace=trace)))
    pending = {"ok": False, "state": "pending", "verdict": row["verdict"], "stop": row["stop"]}
    if foreground and receipt["finish"]["defer"]:
        return pending

    # Preserve the environment boundary of the trusted op the harness used to call.
    # Sweeps still use the node environment, as they do for existing terminals.
    names = os.environ.get("CS_CHILD_ENV", " ".join(DEFAULT_ENV)).split()
    if lane:
        # Only the finalizer needs the node's toolchain config location. Child checks
        # receive the resolved environment through child_env and harness_lane_env.
        names.extend(("CS_REPOS_FILE", "CS_TOOLCHAINS_FILE"))
    env = {key: os.environ[key] for key in names if key in os.environ} if foreground else None
    with subprocess.Popen([str(code_root / "ops/bin/finalize-run")], cwd=code_root, env=env,
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                          text=True, start_new_session=True) as child:
        try:
            out, _ = child.communicate(json.dumps({"run_dir": str(run_dir)}), timeout=120)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.communicate()
            return dict(pending, diagnostic="finalize-run: watchdog expired; inspect the run and retry finalization")
    lines = out.splitlines()
    if lines:
        try:
            result = json.loads(lines[-1])
        except ValueError:
            result = None
        if isinstance(result, dict):
            pending.update(result)
    if child.returncode:
        status = (f"signal: {signal.Signals(-child.returncode).name}"
                  if child.returncode < 0 else f"exit status {child.returncode}")
        pending["diagnostic"] = (
            f"finalize-run: {status}; inspect the run and retry finalization")
    return pending


def finish_request(code_root, run_dir, *, foreground=False, run=subprocess.run, lane_binding=None):
    receipt = json.loads((run_dir / RECEIPT).read_text())
    result = run([str(code_root / "ops/bin/machine-finish")], cwd=str(code_root),
                 input=json.dumps({"run_dir": str(run_dir), "receipt": receipt, "foreground": foreground,
                                   **({"lane_binding": lane_binding} if lane_binding else {})}),
                 text=True, capture_output=True)
    return receipt, result


def execute_harness(code_root, runner, args, *, order=None, env=None, cancel_marker=None):
    # Keep stderr and the controlling terminal attached while collecting the handoff.
    child = None
    pending = None

    def forward(signum, _frame):
        nonlocal pending
        pending = signum
        if child is not None and child.poll() is None:
            child.send_signal(signum)

    signals = (signal.SIGINT, signal.SIGTERM)
    previous = {sig: signal.signal(sig, forward) for sig in signals}
    try:
        if cancel_marker is not None and cancel_marker.is_file():
            return 1, "", signal.SIGTERM
        with subprocess.Popen([runner, *args], cwd=code_root, env=env,
                              stdin=subprocess.PIPE if order is not None else None,
                              stdout=subprocess.PIPE, text=True) as child:
            if pending is not None:
                forward(pending, None)
            out, _ = child.communicate(order)
        return child.returncode, out, pending
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def reserve_cli_lane(code_root, order, locks):
    spec_ref = order.get("spec")
    binding = None
    order.pop("_cs_lane_binding", None)
    if spec_ref:
        from _yaml import load
        spec = load(code_root / str(spec_ref))
        if isinstance(spec, dict) and spec.get("repo_read_only") is not True and spec.get("repo") and spec.get("lane"):
            from _lanes import lane_lock, repo_config
            binding = locks.enter_context(lane_lock(repo_config(str(spec["repo"])), str(spec["lane"])))
            order["_cs_lane_binding"] = binding
    return binding


def launch_cli(code_root, runner, order):
    try:
        with ExitStack() as locks:
            payload = json.loads(order)
            if not isinstance(payload, dict):
                raise ValueError("CLI work order must be an object")
            return _launch_cli(code_root, runner, payload, locks)
    except (OSError, ValueError) as error:
        print(f"executor: {error}", file=sys.stderr)
        return 1


def _launch_cli(code_root, runner, order, locks):
    from uuid import uuid4

    from _job_reason import redact
    from _node_env import node_env

    env = os.environ.copy()
    settings = node_env()
    env.pop("CSGATEWAY_AUTH_TOKEN", None)
    endpoint = env.pop("CSGATEWAY_CLI_ENDPOINT", "") or settings.get("CSGATEWAY_CLI_ENDPOINT")
    if endpoint:
        env["CSGATEWAY_CLI_ENDPOINT"] = endpoint
    env.setdefault("CS_JOB_ID", "local-" + uuid4().hex)
    job_id = env["CS_JOB_ID"]
    cancel_dir = runs_dir(code_root).parent / "node/cancelled"
    marker = cancel_dir / job_id if Path(job_id).name == job_id else None
    binding = reserve_cli_lane(code_root, order, locks)
    request = json.dumps({"order": order, "token": settings.get("CSGATEWAY_AUTH_TOKEN", "")})
    # Leave a completed execution available for finishing even if a cancel races
    # the gateway's terminal reply. The dispatcher owns the job terminal state.
    previous = {sig: signal.signal(sig, lambda *_: None) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        rc, out, cancelled = execute_harness(code_root, runner, ["cli-run", "--machine"],
                                             order=request, env=env, cancel_marker=marker)
        if rc:
            return 1 if cancelled else (rc if rc > 0 else 128 - rc)
        execution = json.loads(out)
        detail = execution["detail"]
        if isinstance(detail, dict):
            sys.stderr.write(redact(detail["stderr"]))
            receipt = detail.get("receipt")
            if receipt is not None:
                run_dir = runs_dir(code_root) / execution["run_id"]
                finished = subprocess.run([str(code_root / "ops/bin/machine-finish")], cwd=code_root,
                                          input=json.dumps({"run_dir": str(run_dir), "receipt": receipt, "lane_binding": binding}),
                                          text=True)
                return finished.returncode if finished.returncode in (0, 1, 3) else 3
        elif isinstance(detail, str):
            print(f"executor: {redact(detail)}", file=sys.stderr)
        if execution["outcome"] == "timed_out":
            print("executor: gateway work order timed out", file=sys.stderr)
        return 1
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


@contextmanager
def repo_view_binding(code_root, env, repo, lane):
    from _lanes import prepare_repo_view, repo_config
    from _sandbox_policy import prefix_value, tool_prefix

    if not isinstance(repo, str) or not repo or lane is not None:
        raise ValueError("a read-only repo spec requires repo: and omits lane:")
    cfg = repo_config(repo)
    words = tool_prefix(code_root, env)
    if not words:
        raise ValueError("read-only repo views require cs-sandbox")
    prefix = prefix_value(words)
    existing = env.get("CS_TOOL_PREFIX", "").split()
    if existing[:3] != words[:3] or existing[3:4] != ["--report-dir"] or existing[5:] != ["--"]:
        env["CS_TOOL_PREFIX"] = prefix
    state = runs_dir(code_root).parent
    state.mkdir(parents=True, exist_ok=True)
    owned = env.pop("CS_REPO_VIEW_STAGING", None)

    def interrupt(signum, _frame):
        raise SystemExit(128 + signum)

    # Cover archive setup and removal as well as execution_harness's signal forwarding.
    previous = {sig: signal.signal(sig, interrupt) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        # Tick owns this directory for dispatched jobs so it can reap even after SIGKILL.
        with tempfile.TemporaryDirectory(prefix="read-only-", dir=owned or state) as staging:
            path = Path(staging).resolve() / "repo"
            env["CS_HARNESS_LANE"] = json.dumps(dict(
                repo=repo, path=str(path), read_only=True, deny_write=prepare_repo_view(cfg, path)))
            yield
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def launch(code_root, runner, args):
    try:
        with ExitStack() as locks:
            return _launch(code_root, runner, args, locks)
    except (OSError, ValueError, TypeError, AttributeError, subprocess.CalledProcessError) as error:
        print(f"cs: {error}", file=sys.stderr)
        return 1


def _launch(code_root, runner, args, locks):
    binding = None
    env = os.environ.copy()
    for key in ("CS_HARNESS_LANE", "CS_HARNESS_SANDBOX", "CS_HARNESS_TOOLCHAIN_ENV"):
        env.pop(key, None)
    if not env.get("CSGATEWAY_ENDPOINT"):
        from _node_env import node_env
        endpoint = node_env().get("CSGATEWAY_ENDPOINT")
        if endpoint:
            env["CSGATEWAY_ENDPOINT"] = endpoint
    repo = lane = None
    spec = {}
    if args and not args[-1].startswith("-"):
        spec_path = code_root / args[-1]
        if any(line.startswith(("repo:", "lane:", "repo_read_only:"))
               for line in spec_path.read_text().splitlines()):
            from _yaml import load
            spec = load(spec_path)
        repo, lane = spec.get("repo"), spec.get("lane")
    if spec.get("repo_read_only") is True:
        with repo_view_binding(code_root, env, repo, lane):
            rc, out, _ = execute_harness(code_root, runner, ["run", *args], env=env)
    else:
        if repo is not None or lane is not None:
            from _lanes import assert_lane_git_pointer, ensure_lane, git_common_dir, lane_lock, repo_config
            from _toolchains import discover, discover_sandbox_links, harness_lane_env
            if not isinstance(repo, str) or not isinstance(lane, str):
                raise ValueError("a lane spec names both repo: and lane:")
            task = args[args.index("-task") + 1] if "-task" in args[:-1] else None
            tasks = [task] if task else spec.get("tasks") or []
            if any((code_root / path / "setup").exists() for path in tasks):
                raise ValueError("a lane task has no setup/ directory")
            cfg = repo_config(repo)
            binding = locks.enter_context(lane_lock(cfg, lane))
            workspace_binding = ensure_lane(cfg, lane)
            env["CS_HARNESS_LANE"] = json.dumps(workspace_binding)
            workspace, main = workspace_binding["path"], workspace_binding["main"]
            gitdir = assert_lane_git_pointer(workspace, main, lane)
            common = git_common_dir(main)
            git_reads = [common / name for name in ("config", "HEAD", "objects", "refs", "packed-refs", "shallow")]
            env["CS_HARNESS_SANDBOX"] = json.dumps(dict(
                path=str(gitdir), deny_write=[],
                read_roots=[str(path) for path in git_reads if path.exists()] + list(discover()),
                read_links=[str(path) for path in discover_sandbox_links()]))
            with harness_lane_env(workspace, env.get("PATH", ""), sandboxed=True) as toolchain:
                env["CS_HARNESS_TOOLCHAIN_ENV"] = json.dumps(toolchain)
                env["PATH"] = toolchain["PATH"]
                rc, out, _ = execute_harness(code_root, runner, ["run", *args], env=env)
        else:
            rc, out, _ = execute_harness(code_root, runner, ["run", *args], env=env)
    if rc != 0:
        return rc if rc > 0 else 128 - rc

    run_dir = None
    try:
        run_dir = Path(json.loads(out)["run_dir"])
        receipt, result = finish_request(code_root, run_dir, foreground=True, lane_binding=binding)
        row = receipt["row"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"cs: finalization pending: {run_dir or error}", file=sys.stderr)
        return 3
    try:
        finished = json.loads(result.stdout.splitlines()[-1])
    except (IndexError, ValueError):
        finished = {}
    verdict, stop = finished.get("verdict") or row["verdict"], finished.get("stop") or row["stop"]
    pending = finished.get("state") != "finalized"
    if pending:
        print(f"cs: finalization pending: {run_dir}", file=sys.stderr)
        diagnostic = finished.get("diagnostic") or finished.get("error") or result.stderr.strip()
        if diagnostic:
            print(f"cs: {row['task']} — {diagnostic}", file=sys.stderr)
    print(f"  {row['task']:<30} t{row['trial']:<2} {verdict:<4} stop={stop}"
          f" turns={row['turns']} tools={row['tool_calls']} ${row['cost_usd']:.4f}", flush=True)
    return 3 if pending else int(verdict != "pass")
