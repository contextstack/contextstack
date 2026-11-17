#!/usr/bin/env python3
"""Missing run records retain job diagnostics; other Director faults stay errors."""
import importlib.util
import sys
from pathlib import Path

sys.dont_write_bytecode = True
kit = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location("cs_mcp", kit / "scripts/cs_mcp.py")
mcp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mcp)


class Director(mcp.Director):
    def __init__(self, status=404, run_ids=False):
        self.status = status
        self.job = {"job_id": "failed", "state": "error", "error": "executor_exit_1",
                    "run_ids" if run_ids else "run_id": ["missing"] if run_ids else "missing"}

    def request(self, method, path, **kwargs):
        assert method == "GET"
        if path == "jobs/failed":
            return 200, {"job": self.job}
        assert path == "runs/missing", path
        return self.status, {"error": "fixture response"}


for run_ids in (False, True):
    out = mcp.Tools(Director(run_ids=run_ids), "fixture").task_output("failed")
    assert out["state"] == "error" and out["error"] == "executor_exit_1", out
    assert out["run_id"] == "missing" and out["output"] is None, out
    assert "missing" in out["note"], out

for status in (401, 500):
    try:
        mcp.Tools(Director(status=status), "fixture").task_output("failed")
    except mcp.Refused as exc:
        assert str(status) in str(exc), exc
    else:
        raise AssertionError("Director %s must remain an error" % status)

print("ok   task_output: missing run returns job state/error; other failures propagate")
