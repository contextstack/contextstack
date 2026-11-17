#!/usr/bin/env python3
import os
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path.cwd()
SUBJECT = ROOT / "scripts/checklib.sh"
HOME = "/Users/fixture"


def fail(message):
    sys.exit("check: " + message)


def git(root, *args):
    subprocess.run(["git", "-C", str(root), *args], check=True,
                   capture_output=True, text=True)


def source(root, check):
    return subprocess.run(
        ["/bin/sh", "-c", '. "$1"', "check", str(SUBJECT)],
        cwd=root, env={**os.environ, "HOME": HOME, "LAB_CHECK": str(check)},
        capture_output=True, text=True,
    )


if not SUBJECT.is_file():
    fail("scripts/checklib.sh missing")

with tempfile.TemporaryDirectory() as raw:
    root = Path(raw)
    fixture, task = root / "workspace", root / "task"
    setup = task / "setup"
    fixture.mkdir(); setup.mkdir(parents=True)
    check = task / "check"
    check.write_text("#!/bin/sh\n", encoding="utf-8")
    git(fixture, "init", "-q")
    git(fixture, "config", "user.email", "fixture@example.test")
    git(fixture, "config", "user.name", "Fixture")
    (fixture / "tracked.txt").write_text("portable\n", encoding="utf-8")
    (fixture / "input.txt").write_text(HOME + "/baseline-input\n", encoding="utf-8")
    (setup / "tracked.txt").write_text("portable\n", encoding="utf-8")
    (setup / "input.txt").write_text(HOME + "/baseline-input\n", encoding="utf-8")
    git(fixture, "add", "tracked.txt", "input.txt")
    git(fixture, "commit", "-qm", "seed")

    clean = source(fixture, check)
    if clean.returncode:
        fail("unchanged input was scanned: " + (clean.stdout or clean.stderr))

    (fixture / "tracked.txt").write_text(HOME + "/tracked-output\n", encoding="utf-8")
    tracked = source(fixture, check)
    if tracked.returncode == 0 or "absolute home path" not in (tracked.stdout + tracked.stderr):
        fail("modified tracked output was accepted")

    (fixture / "tracked.txt").write_text("portable\n", encoding="utf-8")
    generated_path = fixture / "generated receipt.json"
    generated_path.write_text('{"path":"' + HOME + '/generated"}\n', encoding="utf-8")
    generated = source(fixture, check)
    if generated.returncode == 0 or "generated receipt.json" not in (generated.stdout + generated.stderr):
        fail("untracked generated output was accepted")

    generated_path.write_text('{"path":"artifacts/generated"}\n', encoding="utf-8")
    git(fixture, "add", "generated receipt.json")
    staged = source(fixture, check)
    if staged.returncode:
        fail("portable staged output was rejected: " + (staged.stdout or staged.stderr))

print("check: ok")
