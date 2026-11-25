"""Muse process boundary."""
import json
import os
import signal
import subprocess
import sys
from pathlib import Path


def check_environment(env):
    if "META_API_KEY" in env:
        raise ValueError("META_API_KEY present; subscription launch refused")


def subscription_metadata(path):
    try:
        data = json.loads(Path(path).read_text())
        meta = data["providers"]["meta"]
        expected = {"mechanism": "oauth", "storage": "keychain",
                    "obtained_via": "device_code", "api_base_url": "https://api.meta.ai/v1"}
        if data["schema_version"] != 2 or any(meta.get(k) != v for k, v in expected.items()):
            raise ValueError()
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        raise ValueError("Muse requires standard OAuth/device_code/Keychain metadata") from None
    return {"schema_version": 2, "providers": {"meta": expected}}


def sandbox_command(command, workspace, real_home, readonly_paths=(), extra=(), read_roots=(), *,
                    adapter=None, check_scratch=None):
    if sys.platform != "darwin" or not Path("/usr/bin/sandbox-exec").is_file():
        raise ValueError("Muse requires the macOS OS sandbox; no fallback")
    ws, home = Path(workspace).resolve(), Path(real_home).resolve()
    if home.is_relative_to(ws):
        raise ValueError("workspace cannot contain the personal home")
    if any(not Path(p).resolve().is_relative_to(ws) for p in readonly_paths):
        raise ValueError("protected configuration must be inside workspace")
    from _estate_io import native_sandbox_request, render_native_sandbox, sandbox_runtime_code_root
    code_root = sandbox_runtime_code_root()
    request = native_sandbox_request(command[0], workspace, workspace, real_home,
                                     read_links=(Path(real_home) / "Library/Keychains",),
                                     read_roots=read_roots, env=os.environ, caller=dict(extra or {}))
    request.update(variant="adapter" if adapter is not None else "muse",
                   readonly_paths=[str(Path(p).resolve()) for p in readonly_paths],
                   adapter=adapter)
    if check_scratch is not None:
        request["check_scratch"] = str(Path(check_scratch).resolve())
    profile = render_native_sandbox(code_root, code_root / "policy/sandbox.json", request)
    return ["/usr/bin/sandbox-exec", "-p", profile.decode("utf-8"), *map(str, command)]


class ForcedStop(RuntimeError):
    def __init__(self, result):
        super().__init__("executor resource limit")
        self.result = result


def require_resources(result):
    if getattr(result, "forced_meta", None):
        raise ForcedStop(result)
    return result


class Cancelled(RuntimeError):
    def __init__(self, stdout, stderr):
        super().__init__("executor cancelled")
        self.stdout, self.stderr = stdout, stderr


class CleanupFailed(RuntimeError):
    """Job-process cleanup or tracking failed after the CLI ran; carries what the CLI wrote."""
    def __init__(self, error, stdout="", stderr="", returncode=None):
        super().__init__(str(error))
        self.stdout, self.stderr, self.returncode = stdout, stderr, returncode


def close_processes(processes, captured, step=None):
    """processes.close() (or `step`), re-raising a cleanup/tracking RuntimeError as CleanupFailed
    with the output captured so far (captured["output"], or captured["snapshot"]() when given)."""
    try:
        (step or processes.close)()
    except CleanupFailed:
        raise
    except RuntimeError as error:
        output = captured.get("output")
        if output is None and captured.get("snapshot") is not None:
            output = captured["snapshot"]()
        stdout, stderr = output or ("", "")
        raise CleanupFailed(error, stdout, stderr, captured.get("returncode")) from error


def run_process(command, workspace, env, timeout, input_text=None, on_started=None, memory_cap_mb=None, on_evidence=None):
    check_environment(env)
    from _job_memory import prepare_job
    command, env, marker = prepare_job(command, env)
    cancelled = False

    def defer_cancel(_signum, _frame):
        nonlocal cancelled
        cancelled = True

    previous = signal.signal(signal.SIGTERM, defer_cancel)
    try:
        proc = subprocess.Popen(command, cwd=workspace, env=env, start_new_session=True,
                                stdin=subprocess.PIPE if input_text is not None else subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        from _job_memory import JobProcesses
        processes = JobProcesses(proc.pid, workspace, marker=marker)
        if not hasattr(processes, "evidence"):
            processes.evidence = []
        monitor = None
        if memory_cap_mb is not None and memory_cap_mb > 0:
            from _job_memory import MemoryMonitor

            monitor = MemoryMonitor(memory_cap_mb, proc.pid, processes)
    except BaseException:
        signal.signal(signal.SIGTERM, previous)
        raise
    killed = False
    captured = {}
    def kill_group():
        nonlocal killed
        if killed:
            return
        close_processes(processes, captured, processes.kill)
        killed = True
    def cancel(_signum, _frame):
        raise KeyboardInterrupt()
    try:
        if monitor is not None:
            monitor.start()
        if on_started is not None:
            on_started()
        try:
            signal.signal(signal.SIGTERM, cancel)
            if cancelled:
                raise KeyboardInterrupt()
            stdout, stderr = proc.communicate(input_text, timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
            signal.signal(signal.SIGTERM, defer_cancel)
            kill_group()
            # communicate retains output read before the timeout. Reading pipes
            # directly loses it and can hang on a surviving child's open pipe.
            stdout, stderr = proc.communicate(timeout=5)
            captured.update(output=(stdout, stderr), returncode=proc.returncode)
            if isinstance(error, subprocess.TimeoutExpired):
                raise subprocess.TimeoutExpired(command, timeout, output=stdout, stderr=stderr) from None
            raise Cancelled(stdout, stderr) from None
        captured.update(output=(stdout, stderr), returncode=proc.returncode)
        done = subprocess.CompletedProcess(command, proc.returncode, stdout, stderr)
        done.job_process_evidence = processes.evidence
        if monitor is not None:
            monitor.close()
            meta = monitor.forced_meta()
            if meta is not None:
                setattr(done, "forced_meta", meta)
                setattr(done, "forced_stop", meta["stop"])
        return done
    finally:
        signal.signal(signal.SIGTERM, defer_cancel)
        try:
            if monitor is not None:
                monitor.close()
        finally:
            try:
                kill_group()
                proc.wait()
            finally:
                try:
                    try:
                        close_processes(processes, captured)
                    finally:
                        if on_evidence is not None:
                            on_evidence(processes.evidence)
                finally:
                    signal.signal(signal.SIGTERM, previous)
