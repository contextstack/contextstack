"""Shared host-side action approval flow for the harness and CLI lanes."""
import hashlib
import json
import os
import stat
import subprocess
import tempfile
import time
from pathlib import Path

from _director_client import Client, DirectorUnreachable, receiver_config
from _roots import roots

# Must fit inside request_action's 120 s tool budget after polling (about 22 s) and diffs.
ACTION_TIMEOUT_S = 60


SCRIPT_LIMIT = 64_000
ACTION_PATH = "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin"


def script_source(path, workspace):
    relative = path.relative_to(workspace)
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    try:
        directory_fd = os.open(workspace, directory_flags)
        try:
            for part in relative.parts[:-1]:
                child_fd = os.open(part, directory_flags, dir_fd=directory_fd)
                os.close(directory_fd)
                directory_fd = child_fd
            fd = os.open(relative.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                         dir_fd=directory_fd)
            with os.fdopen(fd, "rb") as source:
                if stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                    data = source.read(SCRIPT_LIMIT + 1)
                    if len(data) <= SCRIPT_LIMIT and b"\0" not in data:
                        return {"path": str(relative), "contents": data.decode("utf-8")}
        finally:
            os.close(directory_fd)
    except (OSError, UnicodeError):
        pass
    return {"path": str(relative), "unavailable": True}


def command_context(argv, workspace):
    executable = (workspace / argv[0]).resolve() if "/" in argv[0] else next(
        (candidate.resolve() for directory in ACTION_PATH.split(":")
         if (candidate := Path(directory) / argv[0]).is_file()), None)
    operands = set()
    for word in argv[1:]:
        for token in (word, word.partition("=")[2]):
            path = workspace / token
            if token and "://" not in token and ("/" in token or path.is_symlink()):
                operands.add(str(path.resolve()))
    script = executable
    if executable and not executable.is_relative_to(workspace):
        interpreter = Path(argv[0]).name.rstrip("0123456789.")
        if interpreter in {"sh", "bash", "zsh", "dash", "python", "ruby", "perl", "php", "node"}:
            if any(arg in {"-c", "-e", "-m", "-r"} for arg in argv[1:]):
                script = None
            else:
                operand = next((arg for arg in argv[1:] if not arg.startswith("-")), None)
                script = (workspace / operand).resolve() if operand else None
    scripts = []
    if script and script.is_relative_to(workspace):
        scripts.append(script_source(script, workspace))
    return dict(resolved_operands=sorted(operands),
                resolved_executable=str(executable) if executable else None, scripts=scripts)


def script_digest(command):
    return hashlib.sha256(json.dumps(command["scripts"], sort_keys=True).encode()).hexdigest()


def lane_context(run_dir, argv, env, workspace):
    workspace = workspace.resolve()
    home = Path.home().resolve()
    command = command_context(argv, workspace)
    goal = (run_dir / "stimulus/goal.md").read_text(encoding="utf-8")[:24_000]
    diff_env = dict(env, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                    GIT_OPTIONAL_LOCKS="0")
    diff = ""
    with tempfile.TemporaryFile() as output:
        for base in ("main", "HEAD"):
            result = subprocess.run(
                ["/usr/bin/git", "-c", "core.fsmonitor=false", "--no-pager", "diff",
                 "--no-ext-diff", "--no-textconv", base, "--"],
                cwd=workspace, env=diff_env, stdout=output, stderr=subprocess.DEVNULL, timeout=3, check=False,
            )
            if result.returncode == 0:
                output.seek(0)
                diff = output.read(48_000).decode(errors="replace")
                break
            output.seek(0)
            output.truncate()
    return dict(workspace=str(workspace), home=str(home), job_tmp=env["TMPDIR"],
                goal=goal, diff=diff, **command)


def observation(record):
    return {key: record[key] for key in
            ("id", "status", "reviewer_model", "verdict", "review_reason", "outcome")
            if key in record}


def sandbox_prefix():
    # Match the ordinary runner prefix, but require a standalone cs-sandbox call.
    words = os.environ.get("CS_TOOL_PREFIX", "").split()
    if (len(words) < 4 or Path(words[0]).name != "cs-sandbox" or
            words[1] != "--policy" or words[-1] != "--" or len(words[3:-1]) not in (0, 2) or
            (len(words) == 6 and words[3] != "--report-dir")):
        raise ValueError("Action approval requires the configured cs-sandbox tool prefix.")
    return words


def execute(record, request, prefix, env, repo_view, workspace):
    from _process_group import stop_group

    timed_out = False
    cleanup_failed = False
    widening = {key: key in request["needs"] for key in ("network", "delete")}
    if "network:loopback" in request["needs"]:
        widening["loopback"] = True
    widening["write_paths"] = request["write_paths"]
    flags = ["--action-widening", json.dumps(widening)]
    if repo_view:
        flags.extend(("--repo-view", repo_view))
    with tempfile.TemporaryFile() as output:
        try:
            process = subprocess.Popen(
                [*prefix[:-1], *flags, "--", *request["argv"]],
                cwd=workspace, env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError:
            code = 126
        else:
            try:
                code = process.wait(timeout=ACTION_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                timed_out = True
                code = 124
                try:
                    stop_group(process)
                    process.wait(timeout=5)
                except (OSError, subprocess.TimeoutExpired):
                    cleanup_failed = True
        output.seek(0)
        content = output.read(6000).decode(errors="replace")
        if output.read(1):
            content += "\n[output truncated]"
    if timed_out:
        content += "\n[approved action timed out]"
    if cleanup_failed:
        content += "\n[approved action process cleanup failed]"
    record["outcome"] = {"status": "succeeded" if code == 0 else "failed", "exit_code": code}
    return code, content


def request_action(root, run_dir, model, repo_view, request, *, job, workspace, prefix):
    if not isinstance(request, dict) or set(request) - {"argv", "needs", "reason", "write_paths"}:
        raise ValueError("Provide only argv, needs, reason, and optional write_paths.")
    argv, needs, reason = (request.get(key) for key in ("argv", "needs", "reason"))
    paths = request.get("write_paths", [])
    if (not isinstance(argv, list) or not argv or
            not isinstance(needs, list) or not needs or not isinstance(paths, list) or
            any(not isinstance(word, str) or "\0" in word for word in argv + needs + paths) or
            not isinstance(reason, str) or not reason.strip()):
        raise ValueError("Provide argv, needs, and reason. Provide absolute write_paths when requested.")
    if not argv[0] or any(not Path(path).is_absolute() for path in paths):
        raise ValueError("write_paths must be absolute.")
    env = {key: os.environ[key] for key in ("HOME", "LANG",
                                          "HOMEBREW_PREFIX", "HOMEBREW_CELLAR")
           if key in os.environ}
    env["PATH"] = ACTION_PATH
    job_tmp = run_dir.resolve() / "tmp"
    job_tmp.mkdir(mode=0o700, exist_ok=True)
    if job_tmp.resolve() != job_tmp:
        raise ValueError("The job tmp directory must not be a symlink.")
    env.update(TMPDIR=str(job_tmp), TMP=str(job_tmp), TEMP=str(job_tmp))
    # Freeze the exact canonical write targets before the owner sees them.
    paths = [str(Path(path).resolve()) for path in paths]
    client = director_client(root)
    if not job or not all(ch.isascii() and (ch.isalnum() or ch in "-_") for ch in job):
        raise ValueError("A Director job is required.")
    run = {"run_id": run_dir.name}
    action = dict(argv=argv, needs=sorted(set(needs)), reason=reason.strip(), write_paths=sorted(set(paths)),
                  requesting_model=model)
    path = f"jobs/{job}/action-approvals"
    context = lane_context(run_dir, argv, env, workspace)
    if context["scripts"]:
        action["script_sha256"] = script_digest(context)
    record = client.call("POST", path, body=dict(run, **action, context=context))
    deadline = time.monotonic() + 22
    path += "/" + record["id"]
    while record["status"] == "pending" and time.monotonic() < deadline:
        time.sleep(0.25)
        record = client.call("POST", path + "/poll", body=run)
    status = record["status"]
    if status != "approved":
        print(json.dumps(observation(record)), flush=True)
        if status == "needs_owner":
            print("This action is waiting on the owner. Continue without it if possible.")
        elif status == "denied":
            print("The owner denied this action. Continue without it.")
        else:
            print("The Director has not finished reviewing this action. Continue without it if possible.")
        return 0
    if record.get("claimed_at"):
        print(json.dumps(observation(record)), flush=True)
        print("The Director has already issued this action's single execution claim.")
        return 0
    current = command_context(argv, workspace.resolve())
    if any(context[key] != value for key, value in current.items()):
        raise ValueError("The command's resolved paths or script changed during review.")
    record = client.call("POST", path + "/claim", body=run)
    code = 126
    try:
        if any(record.get(key) != value for key, value in action.items()):
            raise ValueError("The execution claim differs from the requested action.")
        code, content = execute(record, action, prefix, env, repo_view, workspace)
        print(json.dumps(observation(record)), flush=True)
    finally:
        try:
            client.call("POST", path + "/outcome", body=dict(run, exit_code=code))
        except DirectorUnreachable:
            print("The Director did not confirm the outcome. The action claim is still consumed.")
    print(content)
    return 0 if code == 0 else 1


def director_client(root):
    config = roots(root)["config"]
    policy = json.loads((config / "policy/spool.json").read_text(encoding="utf-8"))["receiver"]
    if not isinstance(policy, dict) or policy.get("kind") != "director":
        raise ValueError("Action approvals require a Director node.")
    return Client(receiver_config(policy), timeout=3)
