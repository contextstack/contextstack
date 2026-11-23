"""Fixture transport for tests that exercise the CLI child without a socket."""
import json
import os
import signal
import subprocess
import sys
import time
from contextlib import ExitStack
from pathlib import Path

OPS = Path(__file__).resolve().parents[3] / "ops/bin"
sys.path.insert(0, str(OPS))
from _roots import runs_dir


def command(*child):
    return [sys.executable, str(Path(__file__).resolve()), *map(str, child)]


def reserve(root, env, order, *, run_id=None):
    trial = order.get("trial") or int(env.get("CS_TRIAL") or "1")
    run_id = run_id or f"{time.strftime('%Y%m%d-%H%M%S', time.gmtime())}.{time.time_ns() % 10**9:09d}-fixture-t{trial}"
    run = runs_dir(root, env) / run_id
    run.mkdir(parents=True)
    channel = run / "fixture-receipt"
    return run, {
        **env, "CS_TRIAL": str(trial), "CS_RUN_ID": run_id, "CS_EXECUTION_ID": "cli-" + run_id,
        "CS_CLI_RECEIPT_PATH": str(channel),
    }


def finish_command(root):
    ops = Path(root) / "ops/bin"
    # Some partial estates replace sandbox-exec. Resolve their modules first.
    return [sys.executable, "-c", """
import json, sys
from pathlib import Path
root, source = map(Path, sys.argv[1:])
sys.path[:0] = [str(root / 'ops/bin'), str(source)]
from _machine_finish import finish_receipt, print_result
run = Path(json.load(sys.stdin))
print_result(finish_receipt(root, run, json.loads((run / 'execution-receipt.json').read_text())))
""", str(root), str(OPS)]


def main():
    order = json.load(sys.stdin)
    script = next((Path(arg).resolve() for arg in sys.argv[1:] if arg.endswith("/_executor.py")), None)
    root = script.parents[2] if script else Path.cwd()
    from _api_run import reserve_cli_lane

    with ExitStack() as locks:
        reserve_cli_lane(root, order, locks)
        run, env = reserve(root, os.environ, order)
        with subprocess.Popen([*sys.argv[1:], "--gateway-owned", "--execution-only"],
                              env=env, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                              text=True, start_new_session=True) as child:
            def forward(signum, _frame):
                os.killpg(child.pid, signum)

            for sig in (signal.SIGINT, signal.SIGTERM):
                signal.signal(sig, forward)
            child.stdin.write(json.dumps(order) + "\n")
            child.stdin.flush()
            code = child.wait()
        if code:
            return code if code > 0 else 128 - code
        return subprocess.run(finish_command(root), input=json.dumps(str(run)), text=True).returncode


if __name__ == "__main__":
    sys.exit(main())
