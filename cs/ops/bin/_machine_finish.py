"""Machine-side finishing for the native CLI adapter."""
from __future__ import annotations

import fcntl
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from functools import partial
from pathlib import Path

import _lanes
from _check_grader import GRADER_REPORT_KEYS, lane_grader
import _run_record
from _estate_io import remaining_check_time, run_op as estate_run_op, verify_frozen_stimulus, write_atomic
from _grant import covers_path
from _roots import runs_dir, transcript_id, workspace_dir
from _job_reason import redact
from _muse import Cancelled, run_process


def emit(CODE_ROOT, run_dir, ws, row, spec_id, task_ref, started, check_window,
         verdict, stop, extra=None, *, observed_trace=None, lane=None):
    row["verdict"] = verdict
    row["stop"] = stop
    row.update(extra or {})
    _run_record.check_json(run_dir, row.get("stimulus_sha"), check_window, verdict=verdict)
    _run_record.terminal(run_dir, ws, row, run_dir.name, spec_id, task_ref,
                         started, check_window,
                         cli_row=True, observed_trace=observed_trace, cli_lane=lane)
    return result(CODE_ROOT, run_dir, row)


def result(CODE_ROOT, run_dir, row):
    from _executor import CRED_ENV

    verdict, stop = row["verdict"], row["stop"]
    finalized = _run_record.finalize(CODE_ROOT, run_dir, {k: v for k, v in os.environ.items() if not CRED_ENV.search(k)})
    out = {
        "ok": verdict == "pass",
        "run_id": run_dir.name, "verdict": verdict, "stop": stop,
        "model": row["model"], "cost_usd": row["cost_usd"], "cost_class": row["cost_class"],
        "est_usd": row.get("est_usd", 0), "est_basis": row.get("est_basis", ""),
        "auth": row["auth"], "auth_verified": row["auth_verified"],
        "transcript": row["transcript"],
        "cli_invoked": row.get("cli_invoked", False),
    }
    if row.get("preflight_reason"):
        out["preflight_reason"] = row["preflight_reason"]
    if "cli_exit" in row:
        out["cli_exit"] = row["cli_exit"]
    for key in GRADER_REPORT_KEYS:
        if key in row:
            out[key] = row[key]
    if row.get("branch"):
        out["branch"] = row["branch"]
    if row.get("lane"):
        out["lane"] = row["lane"]
    out["finalize"] = finalized.get("state") or "pending"
    if row.get("muse_identity") or row.get("cli_identity") or row.get("native_probe_count") is not None:
        out = {**row, **out}
    return out


def print_result(out):
    print(json.dumps(out), flush=True)
    raise SystemExit(3 if out["finalize"] != "finalized" else 0 if out["ok"] else 1)


def record_process_evidence(run_dir, rows):
    from _agent_stream import append

    for evidence in rows:
        append(run_dir / "process-evidence.jsonl", dict(evidence, stage="check"))


@contextmanager
def check_environment(env, scratch):
    git_env = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
    for key in ("GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL"):
        if key in os.environ:
            git_env[key] = os.environ[key]
    with tempfile.TemporaryDirectory(prefix="cs-", dir=scratch) as target:
        canonical = str(Path(target).resolve())
        if len(os.fsencode(canonical)) < 32:
            yield dict(env, **git_env, TMPDIR=canonical, TMP=canonical, TEMP=canonical)
            return
        # Unix sockets count bytes in the supplied path before resolving symlinks.
        # The link lives in a private directory and resolves inside the existing grant.
        with tempfile.TemporaryDirectory(prefix="cs-", dir="/tmp") as short:
            alias = Path(short) / "t"
            alias.symlink_to(Path(target).absolute(), target_is_directory=True)
            yield dict(env, **git_env, TMPDIR=str(alias), TMP=str(alias), TEMP=str(alias))


def sandboxed_check(run_dir, ws, child_env, window, memory_cap_mb, policy, caller, read_roots, protected,
                    *, grader=None, step=None):
    from _executor import sandbox_cmd

    profile = run_dir / "check-sandbox.sb"
    stimulus = run_dir / "stimulus"
    if grader:
        read_roots = (*read_roots, grader.root)
        child_env = grader.environment(child_env)
        caller = dict(caller, deny_write_roots=[*caller.get("deny_write_roots", []), str(grader.root)])
    try:
        # A physical directory under /tmp stays short after canonicalization.
        with check_environment(child_env, "/tmp") as env:
            binary, args = str(stimulus / "check"), []
            workdir = grader.root if grader else ws
            if step:
                binary, args = "/bin/sh", ["-c", 'exec "$@"', "gate", *step["command"]]
                if not step.get("frozen"):
                    workdir = ws
                workdir /= step.get("cwd", ".")
            command = sandbox_cmd(binary, args, ws, run_dir / "tmp", profile, protected,
                                  read_roots=(stimulus, *read_roots), policy=policy, caller=caller,
                                  check_scratch=env["TMPDIR"])
            return run_process(command, str(workdir), env, window, memory_cap_mb=memory_cap_mb,
                               on_evidence=lambda rows: record_process_evidence(run_dir, rows))
    finally:
        profile.unlink(missing_ok=True)


def gate_check(run_dir, ws, lane, env, window, deadline, run):
    from _gate_job import run_steps, select_steps, step
    from _lanes import assert_lane_git_pointer, repo_config, workspace_changed_paths

    try:
        config = repo_config(lane["repo"])
        if config["path"] != Path(lane["main"]) or ws == config["path"]:
            raise ValueError("gate workspace does not match the configured lane")
        assert_lane_git_pointer(ws, config["path"], lane["lane"])
        with lane_grader(lane, ws, run_dir) as grader:
            paths = workspace_changed_paths(ws, lane["grader_base_sha"], lane_ctx=lane, env=env)
            steps = select_steps(lane["repo"], config, paths, ws, frozen_root=grader.root if grader else None)
            limit = min(window, int(config["gate_timeout_s"]))
            return run_steps(steps, partial(run, grader=grader), remaining_check_time(limit, deadline), grader)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        return {"steps": [step("gate setup", [], exit=1, seconds=0, tail=redact(str(error)))], "verdict": "fail"}


@dataclass
class Check:
    returncode: int
    stdout: str
    stderr: str
    interruption: str = ""
    interruption_note: str = ""
    resource: dict | None = None
    grader: dict | None = None


def interrupted_check(error):
    def output(value):
        return value.decode("utf-8", errors="replace") if isinstance(value, bytes) else str(value or "")

    stop = "check_timeout" if isinstance(error, subprocess.TimeoutExpired) else "cancelled"
    return Check(124, output(error.stdout), output(error.stderr), stop, redact(str(error)))


def record_check(run_dir, row, window, run, *, recover=False, hosted=False):
    # Persist the raw exit distinction and separately redacted streams before check.json.
    # Publication and failure notes use these even when a retry follows a completed check.
    path = run_dir / "execution-check.json"
    if recover and path.is_file():
        check = Check(**json.loads(path.read_text()))
    else:
        check = collect_check(run)
        if recover:
            check.stdout, check.stderr = redact(check.stdout), redact(check.stderr)
            write_atomic(path, json.dumps(asdict(check)).encode())
    if check.grader:
        row.update(check.grader)
    if check.resource:
        row.update(check.resource)
    if not (hosted and check.interruption):
        _run_record.check_json(run_dir, row["stimulus_sha"], window,
                               -1 if check.interruption else check.returncode, check.stdout + check.stderr,
                               grader=check.grader)
    return check


def collect_check(run):
    try:
        done = run()
        check = done if isinstance(done, Check) else Check(done.returncode, done.stdout or "", done.stderr or "")
    except (subprocess.TimeoutExpired, Cancelled) as error:
        check = interrupted_check(error)
    return check


def footer(run_dir, stop, verdict, row):
    path = run_dir / "transcript.md"
    if path.is_file() and re.search(r"\n---\nstop [^\n]+ · verdict \*\*(pass|fail|void)\*\* · turns \d+ · tools \d+ · cost [^\n]+\n\Z".encode(),
                                    path.read_bytes()):
        return
    _run_record.footer(run_dir, stop, verdict, row)


def semantic(row, check, emit, paths, grant_paths, started):
    chk = check()
    if chk.interruption:
        row["seconds"] = time.time() - started
        emit("fail", "preflight_check_timeout" if chk.interruption == "check_timeout" else "cancelled")
    if chk.returncode != 0:
        row["note"] = redact((chk.stderr or chk.stdout or "preflight check failed").strip())
        emit("fail", "preflight_check")
    if grant_paths:
        bad = [rel for rel in paths()
               if not covers_path(rel, grant_paths)]
        if bad:
            emit("fail", "grant_paths", extra={
                "note": "path not in grant.paths: " + ", ".join(bad)})
    emit("pass", "preflight_skip")


def publish(run_dir, ws, row, chk, verdict, stop, run_op, code_root, emit, *, lane=None, recover=False):
    # Evidence must bind the transcript and candidate before harvest changes the workspace.
    if recover:
        footer(run_dir, stop, verdict, row)
    else:
        _run_record.footer(run_dir, stop, verdict, row)
    payload = {
        "run_dir": str(run_dir), "adapter": "executor", "workspace": str(ws),
        "verdict": verdict, "stop": stop,
        "check_output": redact(chk.stdout + chk.stderr)[:8192],
        "check_exit": chk.returncode, "check_skipped": False,
    }
    if lane:
        payload.update(archive="changed_files", lane_git={key: lane[key] for key in ("main", "lane")})
    published, error = run_op(code_root, "publish-evidence", payload)
    if error:
        emit("fail", "publish_evidence", extra={"note": "publish-evidence: " + error})
    evidence = published or {}
    row.update(evidence_sha=evidence.get("evidence_sha", ""), capture_state=evidence.get("capture_state", ""))
    return evidence


def require_complete(evidence, emit):
    if evidence.get("capture_state") != "complete":
        emit("fail", "evidence_incomplete", extra={"note": "; ".join(evidence.get("incomplete_reasons") or [])})


def hosted(run_dir, ws, row, check, emit, run_op, code_root, *, recover=False):
    try:
        chk = check()
    except Exception as error:
        emit("fail", "check_timeout", extra={"note": redact(str(error))})

    if chk.interruption:
        emit("fail", "check_timeout", extra={"note": chk.interruption_note})
    if chk.returncode != 0:
        emit("fail", "check", extra={"note": redact(chk.stderr or chk.stdout)[:500]})
    evidence = publish(run_dir, ws, row, chk, "pass", "done", run_op, code_root, emit, recover=recover)
    require_complete(evidence, emit)
    emit("pass", "done")


def committed_effect(run_dir, head, message, recover):
    """Recognize a trusted commit that preceded a launcher crash."""
    if not recover:
        return None
    marker = run_dir / "finish-commit-base.json"
    prior = head("rev-parse", "--verify", "HEAD")
    current = prior.stdout.strip() if prior.returncode == 0 else None
    if not marker.is_file():
        write_atomic(marker, json.dumps(current).encode())
        return None
    if not current or current == json.loads(marker.read_text()):
        return None
    log = head("log", "-1", "--format=%B")
    if log.returncode == 0 and log.stdout.strip() == message:
        return current
    raise ValueError("workspace HEAD changed while finishing the receipt")


def task(run_dir, ws, row, check, emit, run_op, code_root, native_stop,
         lane, repo_view, paths, grant_paths, task_name, cleanup, *, recover=False):
    run_id = run_dir.name
    chk = check()
    if chk.interruption and native_stop == "done":
        native_stop = chk.interruption
    cleanup()
    publish_verdict = "pass" if chk.returncode == 0 and native_stop == "done" else "fail"
    evidence = publish(run_dir, ws, row, chk, publish_verdict, native_stop, run_op, code_root,
                       emit, lane=lane, recover=recover)
    if native_stop != "done":
        emit("fail", native_stop)
    elif chk.returncode != 0:
        emit("fail", "check")
    require_complete(evidence, emit)

    if grant_paths:
        bad = [rel for rel in paths() if not covers_path(rel, grant_paths)]
        if bad:
            emit("fail", "grant_paths", extra={"note": "path not in grant.paths: " + ", ".join(bad)})

    if lane:  # the lane branch keeps the work; the repo's gate runs when it lands (_lanes.land)
        message = "lane %s: %s (%s)" % (lane["lane"], task_name, run_id)
        head = partial(_lanes.lane_git, lane["main"], lane["lane"], ws)
        try:
            lane["commit"] = committed_effect(run_dir, head, message, recover) or _lanes.commit_lane(
                ws, message, main=lane["main"], lane=lane["lane"])
        except _lanes.LaneError as error:
            emit("fail", "harvest", extra={"note": str(error)})
        row["lane"]["commit"] = lane["commit"]
        row["branch"] = lane["branch"]
        emit("pass", "done")
    if repo_view:
        emit("pass", "done")
    from _harvest_git import git as harvest_git, repository
    slug = (re.sub(r"[^a-z0-9-]", "", task_name.lower()) or "task")
    if recover and repository(ws).is_symlink():
        emit("fail", "harvest", extra={"note": "harvest: host harvest repository must not be a symlink"})
    message = "cs: %s (%s)" % (slug, run_id)
    if committed_effect(run_dir, partial(harvest_git, ws), message, recover):
        row["branch"] = "cs/%s-%s" % (slug, run_id)
    else:
        harvested, error = run_op(code_root, "harvest", {"workspace": str(ws), "slug": slug, "run_id": run_id})
        if error:
            emit("fail", "harvest", extra={"note": "harvest: " + error})
        row["branch"] = harvested.get("branch")
    emit("pass", "done")


def profiled(run_dir, ws, row, check, emit, run_op, code_root, stop, verdict, lane, read_only,
             paths, grant_paths, task_name):
    if verdict is None:
        # The profiled check uses its original sandbox and shares recovery and
        # the candidate publication/commit sequence with the other CLI templates.
        task(run_dir, ws, row, check, emit, run_op, code_root, stop, lane, read_only,
             paths, grant_paths, task_name, lambda: None, recover=True)
    footer(run_dir, stop, verdict, row)
    payload = {"run_dir": str(run_dir), "adapter": "executor", "workspace": str(ws),
               "verdict": verdict, "stop": stop, "check_skipped": True, "check_exit": None,
               "check_output": row.pop("profiled_diagnostic", "")}
    if lane:
        payload.update(archive="changed_files", lane_git={key: lane[key] for key in ("main", "lane")})
    evidence, error = run_op(code_root, "publish-evidence", payload)
    if error:
        emit("fail", "publish_evidence", extra={"note": row["note"] + "; " + error})
    row.update({key: evidence[key] for key in ("evidence_sha", "capture_state")})
    emit(verdict, stop)


WORK_ORDER_ENV = (
    "CS_JOB_ID", "CS_TRIAL", "CS_SCHEDULE_DATE", "CS_SCHEDULE_SLOT", "CS_SCHEDULE_TIMEZONE", "CS_VIDEO_MUTATION",
)


def execution_receipt(run_dir, ws, row, spec_id, task_ref, task_name, started, check_window,
                      mode, stop, verdict, extra, trace, lane, read_only, grant_paths,
                      policy, caller, read_roots, protected, env_drop, memory_cap_mb, child_env, *, execution_id=None, deadline=None):
    row.update(extra or {})
    usage = {key: row[key] for key in (
        "input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens",
        "reasoning_tokens", "cost_usd", "est_usd", "est_basis", "turns", "tool_calls",
    ) if key in row}
    receipt = {
        "version": 1, "adapter": "executor", "run_id": run_dir.name,
        "job_id": os.environ.get("CS_JOB_ID") or None, "execution_id": execution_id,
        "trial": row["trial"], "spec_id": spec_id, "task_ref": task_ref, "task_name": task_name,
        "started": started, "native": {"stop": stop, "exit": row.get("cli_exit")},
        "stimulus": {"manifest": "stimulus/manifest.json", "sha256": row["stimulus_sha"]},
        "workspace": str(ws), "usage": usage, "row": row,
        "finish": {"mode": mode, "verdict": verdict, "lane": lane, "read_only": read_only,
                   "grant_paths": grant_paths, "check_window": check_window, "memory_cap_mb": memory_cap_mb,
                   **({"deadline": deadline} if deadline is not None else {})},
        "sandbox": {"policy": "policy/sandbox.json",
                    "policy_sha256": sha256(policy) if policy.is_file() else None,
                    "caller": caller, "read_roots": [str(path) for path in read_roots],
                    "protected": protected.name if protected else None, "env_drop": env_drop,
                    "work_order_env": {key: child_env[key] for key in WORK_ORDER_ENV if key in child_env}},
        "trace": trace.terminal_fields(),
    }
    write_atomic(run_dir / "execution-receipt.json", json.dumps(receipt, allow_nan=False).encode())
    return receipt


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def validate_receipt(code_root, run_dir, receipt):
    if (not isinstance(receipt, dict) or type(receipt.get("version")) is not int or receipt["version"] != 1
            or receipt["adapter"] != "executor" or receipt["run_id"] != run_dir.name):
        raise ValueError("receipt version, adapter or run identity differs")
    saved = run_dir / "execution-receipt.json"
    if receipt != json.loads(saved.read_text()):
        raise ValueError("receipt differs from the machine's execution record")
    row, stimulus, finish = (receipt[key] for key in ("row", "stimulus", "finish"))
    if (row["transcript"] != transcript_id(run_dir.name) or row["trial"] != receipt["trial"]
            or row.get("cli_exit") != receipt["native"]["exit"] or stimulus["sha256"] != row["stimulus_sha"]):
        raise ValueError("receipt metadata differs from its run identity")
    if finish["mode"] not in ("task", "semantic", "hosted", "terminal", "profiled", "gate"):
        raise ValueError("unknown receipt finish mode")
    verdicts = {"terminal": ("pass", "fail"), "profiled": (None, "void", "fail")}
    if finish["verdict"] not in verdicts.get(finish["mode"], (None,)):
        raise ValueError("receipt verdict differs from its finish mode")
    verify_frozen_stimulus(run_dir, stimulus)
    if not (run_dir / "stimulus/check").is_file():
        raise ValueError("missing frozen check")
    lane = finish["lane"]
    ws = Path(receipt["workspace"]).resolve()
    expected = Path(lane["path"]) if lane else workspace_dir(run_dir, code_root)
    if ws != expected.resolve() or not ws.is_dir():
        raise ValueError("receipt workspace differs from run workspace")
    sandbox = receipt["sandbox"]
    policy = code_root / "policy/sandbox.json"
    if sandbox["policy"] != "policy/sandbox.json" or (
            sha256(policy) if policy.is_file() else None) != sandbox["policy_sha256"]:
        raise ValueError("sandbox policy differs from execution")
    if sandbox["protected"] not in (None, "budget-controls"):
        raise ValueError("invalid sandbox protected directory")
    return ws


def finish_receipt(code_root, run_dir, receipt):
    from _executor import allowlist_env, changed_paths, trusted_op_env

    run_dir = run_dir.resolve()
    if run_dir.parent != runs_dir(code_root).resolve() or not run_dir.is_dir():
        raise ValueError("run_dir must be a directory under the runs root")
    execution_lock = run_dir / "execution.lock"
    if execution_lock.is_file():
        with execution_lock.open("r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    # Serialize check, evidence and harvest. finalize-run separately locks publication.
    with (run_dir / "machine-finish.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        ws = validate_receipt(code_root, run_dir, receipt)
        terminal = run_dir / "terminal.json"
        if terminal.is_file():
            row = json.loads(terminal.read_text()).get("cli_row")
            if row is None:
                raise ValueError("run already has a legacy terminal")
            return result(code_root, run_dir, row)

        row = dict(receipt["row"])
        if "lane" in row:
            row["lane"] = dict(row["lane"])
        settings, sandbox = receipt["finish"], receipt["sandbox"]
        protected = run_dir / sandbox["protected"] if sandbox["protected"] else None
        env = allowlist_env(sandbox["env_drop"], run_dir / "tmp", ws)
        for key in WORK_ORDER_ENV:
            env.pop(key, None)
        env.update(sandbox["work_order_env"])
        window, mode = settings["check_window"], settings["mode"]
        started = receipt["started"]
        observed = dict(receipt["trace"])
        if "observed_trace" in observed:
            observed["observed_trace"] = dict(observed["observed_trace"], finalization_started=_run_record.trace_clock())

        class Finished(Exception):
            pass

        def conclude(verdict, stop, extra=None):
            if mode in ("profiled", "gate"):
                row["seconds"] = time.time() - started
            raise Finished(verdict, stop, extra)

        def profiled_check(timeout, *, grader=None):
            from _muse_adapter import check_command

            try:
                with check_environment(grader.environment(env) if grader else env, "/tmp") as child_env:
                    command = check_command(run_dir / "stimulus/check", ws, Path.home(),
                                            sandbox["read_roots"], sandbox["caller"], env["TMPDIR"],
                                            grader=grader.root if grader else None,
                                            check_scratch=child_env["TMPDIR"])
                    done = run_process(command, str(grader.root if grader else ws), child_env, timeout, memory_cap_mb=settings["memory_cap_mb"],
                                       on_evidence=lambda rows: record_process_evidence(run_dir, rows))
            except Cancelled:
                raise
            except (OSError, RuntimeError) as error:
                return Check(1, "", redact(str(error)), "check_error")
            forced = getattr(done, "forced_meta", None)
            if forced:
                resource = {key: forced[key] for key in ("peak_rss_bytes", "reason") if forced.get(key) is not None}
                return Check(done.returncode, done.stdout or "", done.stderr or "",
                             str(forced.get("stop") or "memory_cap"), resource=resource)
            return done

        def check():
            limit = min(window, row.pop("profiled_timeout")) if mode == "profiled" else window

            def run_with_grader():
                try:
                    with lane_grader(lane, ws, run_dir) as grader:
                        timeout = remaining_check_time(limit, settings.get("deadline"))
                        if timeout <= 0:
                            chk = interrupted_check(subprocess.TimeoutExpired("check", 0))
                        else:
                            if mode == "profiled":
                                run = partial(profiled_check, timeout)
                            else:
                                run = partial(sandboxed_check, run_dir, ws, env, timeout, settings["memory_cap_mb"],
                                              code_root / sandbox["policy"], sandbox["caller"],
                                              sandbox["read_roots"], protected)
                            chk = collect_check(partial(run, grader=grader) if grader else run)
                        if grader:
                            chk.grader = grader.report(chk.stdout + "\n" + chk.stderr,
                                                       -1 if chk.interruption else chk.returncode)
                        return chk
                except (OSError, ValueError, subprocess.CalledProcessError) as error:
                    return Check(1, "", redact(str(error)), "check_error")

            return record_check(run_dir, row, window, run_with_grader, recover=True, hosted=mode == "hosted")

        lane = dict(settings["lane"]) if settings["lane"] else None
        paths = partial(changed_paths, ws, row["base_sha"], lane, env=env)
        run_op = partial(estate_run_op, env=trusted_op_env(env))
        stop = receipt["native"]["stop"]
        if mode == "profiled" and settings["verdict"] is not None:
            row.pop("profiled_timeout")
        try:
            if mode == "terminal":
                conclude(settings["verdict"], stop)
            elif mode == "gate":
                gate_path = run_dir / "gate.json"
                if gate_path.is_file():
                    gate = json.loads(gate_path.read_text())
                else:
                    def run_step(step, timeout, *, grader):
                        return sandboxed_check(
                            run_dir, ws, env, timeout, settings["memory_cap_mb"],
                            code_root / sandbox["policy"], sandbox["caller"], sandbox["read_roots"], protected,
                            grader=grader, step=step,
                        )

                    gate = gate_check(run_dir, ws, lane, env, window, settings.get("deadline"), run_step)
                    write_atomic(gate_path, (json.dumps(gate) + "\n").encode())
                verdict = gate["verdict"]
                _run_record.check_json(run_dir, row["stimulus_sha"], window, int(verdict != "pass"),
                                       "\n".join(item["tail"] for item in gate["steps"]))
                conclude(verdict, "done" if verdict == "pass" else "check")
            elif mode == "semantic":
                semantic(row, check, conclude, paths, settings["grant_paths"], started)
            elif mode == "hosted":
                hosted(run_dir, ws, row, check, conclude, run_op, code_root, recover=True)
            elif mode == "profiled":
                profiled(run_dir, ws, row, check, conclude, run_op, code_root, stop,
                         settings["verdict"], lane, settings["read_only"], paths,
                         settings["grant_paths"], receipt["task_name"])
            else:
                task(run_dir, ws, row, check, conclude, run_op, code_root, stop, lane,
                     settings["read_only"], paths, settings["grant_paths"], receipt["task_name"], lambda: None, recover=True)
        except Finished as done:
            if settings["read_only"]:
                for root in map(Path, sandbox["read_roots"]):
                    snapshot = root.parent
                    if (root.name == "repo" and snapshot.parent == run_dir
                            and snapshot.name.startswith("read-only-") and snapshot.exists()):
                        shutil.rmtree(snapshot)
            return emit(code_root, run_dir, ws, row, receipt["spec_id"], receipt["task_ref"],
                        started, window, *done.args, observed_trace=observed, lane=lane)
        raise RuntimeError("machine finishing did not produce a verdict")
