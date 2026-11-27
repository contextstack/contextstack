"""Select and record model-free lane gate steps."""

import re
import subprocess
import time
from pathlib import Path

from _job_reason import redact
from _muse import Cancelled


REGRESSIONS = Path("cs/tasks/regressions")
NESTED_SKIP = re.compile(r"\bskip\b[^\n]*(?:seatbelt|nested sandbox|sandbox_apply|host probe)", re.I)


def step(name, command, **settings):
    return {"name": name, "command": command, **settings}


def configured_steps(steps):
    if not isinstance(steps, list) or not steps:
        raise ValueError("gate_steps must be a nonempty list")
    result = []
    for item in steps:
        if not isinstance(item, dict) or not isinstance(item.get("name"), str) or not item["name"]:
            raise ValueError("each gate step needs a name")
        command = item.get("command")
        if isinstance(command, str) and command.strip():
            command = ["/bin/sh", "-c", command]
        if not isinstance(command, list) or not command or not all(isinstance(arg, str) and arg for arg in command):
            raise ValueError("each gate step needs a shell command or argument list")
        if "host_only" in item and type(item["host_only"]) is not bool:
            raise ValueError("gate step host_only must be a boolean")
        result.append(step(item["name"], command, frozen=True, host_only=item.get("host_only", False)))
    return result


def select_steps(repo, config, paths, root, *, frozen_root=None):
    if config.get("gate_steps") is not None:
        return configured_steps(config["gate_steps"])
    if config.get("gate"):
        return configured_steps([step("repo gate", config["gate"])])
    if repo != "contextstack":
        raise ValueError("repo has no gate or gate_steps configured")

    steps = [
        step("rustfmt", ["cargo", "fmt", "--all", "--", "--check"]),
        step("clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
        step("rust tests", ["cargo", "test", "--workspace"]),
        step("size limits", ["python3", "scripts/cs-size.py"]),
    ]
    regressions = set()
    for path in map(Path, paths):
        if path.is_relative_to(REGRESSIONS) and len(path.parts) > len(REGRESSIONS.parts) + 1:
            regressions.add(path.parts[len(REGRESSIONS.parts)])
    for name in sorted(regressions):
        check = REGRESSIONS / name / "check"
        if (root / check).is_file() or (frozen_root and (frozen_root / check).is_file()):
            steps.append(step(name, ["./" + str(check.relative_to("cs"))], frozen=True, cwd="cs", regression=name))
    if any(path.startswith("director/") for path in paths):
        steps.append(step("Director contract", ["tests/director-spec/run", "--side", "both"]))
    return steps


def output_text(value):
    return value.decode("utf-8", "replace") if isinstance(value, bytes) else value or ""


def run_steps(steps, run, timeout, grader=None):
    """Run steps within one check window, preserving skips and interrupted output."""
    deadline = time.monotonic() + timeout
    results = []
    cancelled = False
    for item in steps:
        started = time.monotonic()
        result = step(item["name"], list(map(redact, item["command"])), exit=None, seconds=0, tail="")
        results.append(result)
        host = grader.host_only.get(item.get("regression")) if grader else None
        if item.get("host_only") or host:
            result.update(skipped="host_only", tail=host or "")
            continue
        if cancelled:
            result.update(exit=130, tail="gate cancelled")
            continue
        try:
            remaining = deadline - started
            if remaining <= 0:
                raise subprocess.TimeoutExpired(item["command"], 0)
            done = run(item, remaining)
            output = output_text(done.stdout) + output_text(done.stderr)
            result.update(exit=done.returncode, tail=redact(output)[-4000:])
            if item.get("regression") and done.returncode == 0 and NESTED_SKIP.search(output):
                result.update(exit=None, skipped="host_only")
            if grader:
                for skipped in grader.skips(output):
                    results.append(step(skipped["regression"], [], exit=None, seconds=0,
                                        tail=skipped["reason"], skipped="host_only"))
        except (subprocess.TimeoutExpired, Cancelled) as error:
            cancelled = isinstance(error, Cancelled)
            output = output_text(error.stdout) + output_text(error.stderr)
            note = "gate cancelled" if cancelled else "gate time budget exhausted"
            result.update(exit=130 if cancelled else 124, tail=(redact(output) + "\n" + note).strip()[-4000:])
        except (OSError, RuntimeError, ValueError) as error:
            result.update(exit=1, tail=redact(str(error))[-4000:])
        result["seconds"] = round(time.monotonic() - started, 3)
    passed = any(item["exit"] == 0 for item in results) and all(item["exit"] in (0, None) for item in results)
    # A host-only skip is never a pass: list it so "pass" with gaps can't read as fully verified.
    uncovered = [item["name"] for item in results if item.get("skipped")]
    return {"steps": results, "verdict": "pass" if passed else "fail",
            "complete": passed and not uncovered, "uncovered": uncovered}
