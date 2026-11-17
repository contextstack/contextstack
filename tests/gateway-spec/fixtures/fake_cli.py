"""Fake CLI child: the gateway's lab work-order executor (models.json
lab_executor). The gateway runs `<python3> fake_cli.py --gateway-owned` with the
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
  lines             [[line, sleep_s], ...]: print each line (newline added,
                    flushed), then sleep: a coding agent's canned streaming
                    output (claude stream-json, codex --json, gemini
                    stream-json), so live output and mid-run cancel can be seen

The default stdout is a JSON report: argv[1:], the stdin line, the sorted
environment names, and every LAB_* value, so the suite pins argv, stdin and
environment filtering at the process boundary.
"""
import json
import os
import subprocess
import sys
import time

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
        "lab": {k: v for k, v in sorted(os.environ.items()) if k.startswith("LAB_")},
    }, sort_keys=True))
sys.stderr.write(fake.get("stderr", ""))
sys.exit(fake.get("exit", 0))
