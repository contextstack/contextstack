"""Fake CLI child: the gateway's CLI work-order executor (models.json
cli_executor). The gateway runs `<python3> fake_cli.py --gateway-owned --execution-only` with the
compact work order as one line on stdin. Behaviour comes from the work order's
`fake` object:

  stdout / stderr   text to print (default: a JSON report, below)
  exit              exit status (default 0)
  sleep_s           sleep before exiting (cancellation and deadline cases)
  pidfile           write {"pid", "pgid", "grandchild"} here once running
  grandchild        spawn a sleeping grandchild that inherits stdout and
                    outlives this process (surviving-descendant case)
  exit_early        with grandchild: exit at once, leaving the grandchild
  eof_marker        block reading stdin; on EOF (the gateway died) write
                    this file and exit: parent death is visible to the child
  wait_for          after printing lines, wait for this file before exiting
  lines             [[line, sleep_s], ...]: print each line (newline added,
                    flushed), then sleep: a coding agent's canned streaming
                    output (claude stream-json, codex --json, gemini
                    stream-json), so live output and mid-run cancel can be seen

The default stdout is a JSON report: argv[1:], the stdin line, the sorted
environment names, and every CS_* value, so the suite pins argv, stdin and
environment filtering at the process boundary.
"""
import json
import os
import subprocess
import sys
import time

receipt_path = os.environ.pop("CS_CLI_RECEIPT_PATH", None)
receipt = open(receipt_path, "w") if receipt_path else None
if receipt:
    os.unlink(receipt_path)

line = sys.stdin.readline()
order = json.loads(line) if line.strip() else {}
fake = order.get("fake") or {}
grandchild = None
if fake.get("grandchild"):
    grandchild = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], stdout=sys.stdout, stderr=sys.stderr)
if fake.get("pidfile"):
    with open(fake["pidfile"] + ".tmp", "w") as f:
        json.dump({"pid": os.getpid(), "pgid": os.getpgid(0), "grandchild": grandchild.pid if grandchild else None}, f)
    os.rename(fake["pidfile"] + ".tmp", fake["pidfile"])
if fake.get("exit_early"):
    sys.stdout.write(fake.get("stdout", "early\n"))
    sys.stdout.flush()
    os._exit(fake.get("exit", 0))
if fake.get("eof_marker"):
    sys.stdin.read()
    with open(fake["eof_marker"], "w") as f:
        f.write("stdin closed\n")
    sys.exit(0)
for text, pause in fake.get("lines") or []:
    sys.stdout.write(text + "\n")
    sys.stdout.flush()
    time.sleep(pause)
if fake.get("wait_for"):
    deadline = time.monotonic() + 10
    while not os.path.exists(fake["wait_for"]) and time.monotonic() < deadline:
        time.sleep(0.02)
    if not os.path.exists(fake["wait_for"]):
        sys.exit("test did not release fake CLI")
if fake.get("sleep_s"):
    time.sleep(fake["sleep_s"])
if "lines" in fake and "stdout" not in fake:
    pass
elif "stdout" in fake:
    sys.stdout.write(fake["stdout"])
else:
    sys.stdout.write(json.dumps({
        "argv": sys.argv[1:], "stdin": line.rstrip("\n"),
        "env_names": sorted(os.environ), "path": os.environ.get("PATH"),
        "work_order_env": {k: v for k, v in sorted(os.environ.items()) if k.startswith("CS_")},
    }, sort_keys=True))
sys.stderr.write(fake.get("stderr", ""))
if receipt:
    if fake.get("malformed_receipt"):
        receipt.write("{invalid")
    elif not fake.get("omit_receipt"):
        assert sys.argv[1:] == ["--gateway-owned", "--execution-only"]
        trial = int(os.environ["CS_TRIAL"])
        digest = "a" * 64
        json.dump({
            "version": 1, "adapter": "executor",
            "run_id": fake.get("receipt_run", os.environ["CS_RUN_ID"]),
            "job_id": fake.get("receipt_job", os.environ["CS_JOB_ID"]),
            "execution_id": os.environ["CS_EXECUTION_ID"], "trial": trial,
            "spec_id": "fixture", "task_ref": "/fixture/task", "task_name": "task",
            "started": 123.0, "native": {"stop": "executor_exit_7", "exit": fake.get("native_exit", 7)},
            "stimulus": {"manifest": "stimulus/manifest.json", "sha256": digest},
            "workspace": "/fixture/workspace", "usage": {"input_tokens": 1, "output_tokens": 2},
            "row": {"transcript": "fixture", "trial": trial, "stimulus_sha": digest, "base_sha": "",
                    "model": order["executor"], "auth": "none", "auth_verified": False,
                    "cost_usd": None, "cost_class": "free_local", "cli_invoked": True},
            "finish": {"mode": "task", "verdict": None, "lane": None, "read_only": False,
                       "grant_paths": [], "check_window": 60, "memory_cap_mb": None},
            "sandbox": {"policy": "policy/sandbox.json", "policy_sha256": None, "caller": {},
                        "read_roots": [], "protected": None, "env_drop": [], "work_order_env": {}},
            "trace": {},
        }, receipt)
    receipt.close()
sys.exit(fake.get("exit", 0))
