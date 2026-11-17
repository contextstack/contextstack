#!/usr/bin/env python3
"""cs review weekly: pull recent lane jobs from the Director and emit a weekly review report.

Computes from the Director's Client API (owner credential, via cs_mcp):
  1. task_type x agent/model table: jobs, pass rate, approve rate, median minutes,
     median tokens, and stuck/provider_limit/memory_cap stop counts.
  2. Worst lanes (rejected, stuck, retried, errored), each with a two-line autopsy
     built from the run's reason/decision note and stuck.md when present.
  3. Provider-limit events by template (from runs/jobs and node status).
  4. Up to three concrete, checkable proposed changes derived deterministically
     by rules over the table (never by a model call).

Scheduling: `cs review schedule` appends one host row to <state>/schedules.jsonl
(Monday 08:00 local, missed catch_up). schedule-tick, which node-listen and tick
already run, fires it. The report lands at <state>/reviews/<date>.md and one
attention item in <state>/attention.jsonl links to it. `--plist` only prints a
launchd template; nothing here installs a plist.

Stdlib only.
"""
import argparse
import json
import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
KIT_ROOT = HERE.parent
for _p in (str(HERE), str(KIT_ROOT / "ops" / "bin")):
    if _p not in sys.path:
        sys.path.insert(0, _p)

from cs_mcp import Refused  # noqa: E402
from _review_collect import collect_entries, resolve_director  # noqa: E402
from _review_render import render_markdown, render_plist  # noqa: E402
from _review_score import build_table, build_worst_lanes, generate_proposals  # noqa: E402

ClientError = Refused


def collect_weekly_review(director, since_days=7.0, now=None, agents_dir=None):
    """Pull the last N days of lane jobs from the Director and compute the review report dict."""
    collected = collect_entries(director, since_days=since_days, now=now)
    entries = collected["entries"]
    table_rows = build_table(entries)
    worst_lanes = build_worst_lanes(director, entries, token=collected["token"])
    proposals, proposal_details = generate_proposals(
        table_rows, collected["provider_limits"], entries, agents_dir=agents_dir
    )
    return {
        "window_days": collected["window_days"],
        "generated_at": int(collected["now_ts"]),
        "cutoff_at": int(collected["cutoff_ts"]),
        "total_jobs": len(entries),
        "table": table_rows,
        "worst_lanes": worst_lanes,
        "provider_limits": collected["provider_limits"],
        "proposals": proposals,
        "proposal_details": proposal_details,
    }


def install_cli(args, root=None):
    """Append the weekly-review schedule row. Idempotent when that row is already current."""
    from _review_schedule import install_schedule

    state = Path(args.state) if getattr(args, "state", None) else None
    if state is None:
        base = Path(root) if root else Path(os.environ.get("HOME") or os.path.expanduser("~")) / ".contextstack"
        state = base / "state"
    result = install_schedule(state)
    if result["already"]:
        sys.stdout.write("cs review: weekly-review already scheduled (Monday 08:00 local) in %s\n" % result["state"])
    else:
        sys.stdout.write("cs review: scheduled weekly-review (Monday 08:00 local, catch_up) in %s\n" % result["state"])
    return 0


def run_cli(args, root=None):
    """Execute `cs review weekly` or `cs review schedule` from a parsed argparse Namespace."""
    if getattr(args, "period", None) == "schedule":
        return install_cli(args, root)
    if getattr(args, "plist", False):
        sys.stdout.write(render_plist())
        return 0
    if args.since <= 0:
        raise Refused("--since DAYS must be > 0")
    director = resolve_director(
        director_url=getattr(args, "director", None),
        credential_file=getattr(args, "credential_file", None),
        root=root,
    )
    report = collect_weekly_review(director, since_days=args.since)
    try:
        token = director.token()
    except Refused:
        token = ""
    out = json.dumps(report, indent=2) + "\n" if getattr(args, "json", False) else render_markdown(report)
    if token and len(token) >= 8:
        out = out.replace(token, "[REDACTED]")
    sys.stdout.write(out)
    return 0


def build_parser():
    parser = argparse.ArgumentParser(
        prog="cs review",
        description="Summarize recent lane jobs from the Director Client API.",
    )
    parser.add_argument(
        "period",
        nargs="?",
        default="weekly",
        choices=["weekly", "schedule"],
        help="weekly: print the review. schedule: install the Monday 08:00 schedule (default: weekly)",
    )
    parser.add_argument(
        "--state",
        default=None,
        help="State root for `review schedule` (default: <root>/state)",
    )
    parser.add_argument(
        "--since",
        type=float,
        default=7.0,
        metavar="DAYS",
        help="Lookback window in days (default: 7)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit JSON instead of Markdown",
    )
    parser.add_argument(
        "--plist",
        action="store_true",
        help="Print the weekly launchd plist template and exit (installs nothing)",
    )
    parser.add_argument("--director", default=None, help="Director base URL")
    parser.add_argument("--credential-file", default=None, help="Path to owner credential file")
    parser.add_argument("--node", default=None, help="Default node name")
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return run_cli(args)
    except ClientError as exc:
        print(f"cs review: error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
