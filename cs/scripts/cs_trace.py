"""cs trace check: assemble a run's traces and grade them with tracing/check-trace.

Builds the combined input the checker needs: the run's trace.jsonl, the gateway's
trace for the same run when present, and a run_directories map so content pointers
resolve. Invokes the release's tracing/check-trace (never a copy). Stdlib only.
"""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

# Real run ids start with YYYYMMDD-HHMMSS or YYYYMMDD-HHMMSS.fffffffff.
_RUN_TS = re.compile(r"^(\d{8})-(\d{6})(?:\.(\d+))?")


def check_trace_bin():
    """The release (or checkout) that contains this cs script's tracing/check-trace."""
    path = Path(__file__).resolve().parents[2] / "tracing" / "check-trace"
    if not path.is_file():
        raise FileNotFoundError("tracing/check-trace is missing beside this release's cs/")
    return path


def resolve_state(cs):
    """State root: CS_STATE_ROOT wins, else the install's state_root()."""
    override = os.environ.get("CS_STATE_ROOT")
    if override:
        return Path(override).expanduser()
    return Path(cs.state_root())


def gateway_traces(cs):
    """Gateway traces live under the install root, not under CS_STATE_ROOT."""
    return Path(cs.ROOT) / "gateway" / "traces"


def is_run_dir(path):
    """A run dir holds terminal.json (finalize-run) and/or the checker's trace.jsonl."""
    return path.is_dir() and not path.name.startswith(".") and (
        (path / "terminal.json").is_file() or (path / "trace.jsonl").is_file()
    )


def run_recency_ns(path):
    """Newest-first key: timestamp embedded in the run id, else folder mtime."""
    match = _RUN_TS.match(path.name)
    if match:
        date_s, hms, frac = match.group(1), match.group(2), match.group(3) or "0"
        try:
            instant = datetime.strptime(date_s + hms, "%Y%m%d%H%M%S").replace(tzinfo=timezone.utc)
            return int(instant.timestamp()) * 10**9 + int(frac.ljust(9, "0")[:9])
        except ValueError:
            pass
    try:
        return path.stat().st_mtime_ns
    except OSError:
        return 0


def list_runs(state):
    runs = state / "runs"
    if not runs.is_dir():
        return []
    dirs = [p for p in runs.iterdir() if is_run_dir(p)]
    dirs.sort(key=run_recency_ns, reverse=True)
    return [p.name for p in dirs]


def select_runs(state, run_ids, last):
    available = list_runs(state)
    if run_ids:
        return list(run_ids)
    if last is not None:
        if last < 1:
            raise ValueError("--last requires a positive integer")
        return available[:last]
    return available[:1]


def span_count(paths):
    count = 0
    for path in paths:
        text = path.read_text(encoding="utf-8")
        try:
            data = json.loads(text)
        except ValueError:
            count += sum(1 for line in text.splitlines() if line.strip())
            continue
        if isinstance(data, list):
            count += len(data)
        elif isinstance(data, dict) and "spans" in data:
            count += len(data["spans"])
        elif isinstance(data, dict) and "trace_id" in data:
            count += 1
    return count


def exit_for(result):
    status = result.get("result")
    if status == "complete":
        return 0
    if status == "complete_with_findings":
        return 1
    return 2


def directory_map(run_id, run_dir, run_trace):
    mapping = {run_id: str(run_dir.resolve())}
    try:
        for line in run_trace.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            span = json.loads(line)
            tid = span.get("trace_id") if isinstance(span, dict) else None
            if tid:
                mapping[tid] = str(run_dir.resolve())
            break
    except (ValueError, OSError):
        pass
    return {"run_directories": mapping}


def check_one(run_id, state, traces_dir, checker):
    run_dir = state / "runs" / run_id
    run_trace = run_dir / "trace.jsonl"
    if not run_trace.is_file():
        return {
            "run_id": run_id,
            "spans": 0,
            "result": "missing",
            "missing": ["run trace %s" % run_trace],
            "findings": [],
            "gateway_trace": False,
        }, 2

    gateway = traces_dir / ("%s.jsonl" % run_id)
    inputs = [run_trace]
    if gateway.is_file():
        inputs.append(gateway)

    with tempfile.TemporaryDirectory(prefix="cs-trace-check-") as raw:
        directories = Path(raw) / "run_directories.json"
        directories.write_text(json.dumps(directory_map(run_id, run_dir, run_trace)) + "\n",
                           encoding="utf-8")
        argv = [sys.executable, str(checker), *(str(p) for p in inputs), str(directories)]
        try:
            proc = subprocess.run(argv, capture_output=True, text=True, timeout=60,
                                  stdin=subprocess.DEVNULL)
        except (OSError, subprocess.TimeoutExpired) as error:
            return {
                "run_id": run_id,
                "spans": span_count(inputs),
                "result": "missing",
                "missing": [str(error)],
                "findings": [],
                "gateway_trace": gateway.is_file(),
            }, 2

    try:
        result = json.loads(proc.stdout)
    except ValueError:
        result = {
            "result": "incomplete",
            "missing": [proc.stderr.strip() or proc.stdout.strip() or "check-trace failed"],
            "findings": [],
        }
    if "error" in result and "result" not in result:
        result = {"result": "incomplete", "missing": [result["error"]], "findings": []}
    out = {
        "run_id": run_id,
        "spans": span_count(inputs),
        "result": result.get("result", "incomplete"),
        "missing": result.get("missing") or [],
        "findings": result.get("findings") or [],
        "gateway_trace": gateway.is_file(),
    }
    return out, exit_for(out)


def format_line(row):
    kinds = sorted({f.get("kind") for f in row["findings"] if isinstance(f, dict) and f.get("kind")})
    parts = ["%s  %d spans  %s  missing=%d"
             % (row["run_id"], row["spans"], row["result"], len(row["missing"]))]
    if kinds:
        parts.append("findings=%s" % ",".join(kinds))
    return "  ".join(parts)


def add_arguments(parser):
    actions = parser.add_subparsers(dest="action", required=True)
    check = actions.add_parser("check", help="grade one or more runs with tracing/check-trace")
    check.add_argument("run_ids", nargs="*", metavar="RUN_ID")
    check.add_argument("--last", type=int, metavar="N", help="check the last N runs")
    check.add_argument("--json", action="store_true", help="emit full results as JSON")


def main(args, cs):
    if args.action != "check":
        raise cs.Stop("usage: cs trace check [RUN_ID ...] [--last N] [--json]", 2)
    state = resolve_state(cs)
    try:
        selected = select_runs(state, args.run_ids, args.last)
        if not selected:
            print("[]" if args.json else "no runs yet")
            return 0
        checker = check_trace_bin()
    except (FileNotFoundError, ValueError) as error:
        raise cs.Stop("cs trace check: %s" % error, 2)
    traces = gateway_traces(cs)

    rows, code = [], 0
    for run_id in selected:
        row, row_code = check_one(run_id, state, traces, checker)
        rows.append(row)
        code = max(code, row_code)

    if args.json:
        print(json.dumps(rows, sort_keys=True))
    else:
        for row in rows:
            print(format_line(row))
    return code
