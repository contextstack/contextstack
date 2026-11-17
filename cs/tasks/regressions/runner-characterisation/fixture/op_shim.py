#!/usr/bin/env python3
# Fixture op shim, installed as <estate>/ops/bin/<name>. Records what the
# runner handed the op (stdin JSON, cwd, environment names), then runs the
# REAL op from <estate>/ops/real-bin/<name> with the same stdin. A name
# listed in <estate>/fixture/fail-ops.json fails instead, so a case can
# pin what the runner does when an op dies.
import json
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ESTATE = HERE.parent.parent
NAME = Path(__file__).name
data = sys.stdin.buffer.read()
try:
    payload = json.loads(data)
except ValueError:
    payload = data.decode("utf-8", "replace")
with open(ESTATE / "fixture" / "ops-calls.jsonl", "a", encoding="utf-8") as f:
    f.write(json.dumps({"op": NAME, "cwd": os.getcwd(), "env_names": sorted(os.environ),
                        "stdin": payload}, sort_keys=True) + "\n")
fail = ESTATE / "fixture" / "fail-ops.json"
if fail.is_file() and NAME in json.loads(fail.read_text(encoding="utf-8")):
    print("fixture: forced %s failure" % NAME, file=sys.stderr)
    sys.exit(1)
r = subprocess.run([str(ESTATE / "ops" / "real-bin" / NAME)], input=data)
sys.exit(r.returncode)
