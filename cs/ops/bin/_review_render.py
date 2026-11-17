"""cs review weekly's rendering: the Markdown report and the launchd plist template. Not an op.

Split out of cs/scripts/cs_review.py (the `cs review weekly` CLI). Renders the report dict
scored by _review_score.py as the Markdown tables and lists `cs review weekly` prints. The weekly
run is `cs review schedule` plus schedule-tick, not launchd. `--plist` only prints a template
(nothing is installed; `cs review weekly --plist` prints one).

Stdlib only.
"""
import os

PLIST_TEMPLATE = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
\t<key>Label</key>
\t<string>com.contextstack.weekly-review</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>__HOME__/.contextstack/current/cs/scripts/cs</string>
\t\t<string>review</string>
\t\t<string>weekly</string>
\t\t<string>--since</string>
\t\t<string>7</string>
\t</array>
\t<key>WorkingDirectory</key>
\t<string>__HOME__/.contextstack</string>
\t<key>EnvironmentVariables</key>
\t<dict>
\t\t<key>PATH</key>
\t\t<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
\t\t<key>LANG</key>
\t\t<string>en_US.UTF-8</string>
\t</dict>
\t<key>StartCalendarInterval</key>
\t<dict>
\t\t<key>Weekday</key>
\t\t<integer>1</integer>
\t\t<key>Hour</key>
\t\t<integer>8</integer>
\t\t<key>Minute</key>
\t\t<integer>0</integer>
\t</dict>
\t<key>StandardOutPath</key>
\t<string>__HOME__/.contextstack/logs/weekly-review.out.log</string>
\t<key>StandardErrorPath</key>
\t<string>__HOME__/.contextstack/logs/weekly-review.err.log</string>
\t<key>RunAtLoad</key>
\t<false/>
</dict>
</plist>
"""


def render_plist(home=None):
    """Return the launchd plist template with __HOME__ filled in (nothing is installed)."""
    resolved_home = str(home or os.environ.get("HOME") or "/Users/Shared")
    return PLIST_TEMPLATE.replace("__HOME__", resolved_home)


def _fmt_pct(rate, num=None, den=None):
    if rate is None:
        return "—"
    pct = f"{int(round(rate * 100))}%"
    if num is not None and den is not None:
        return f"{pct} ({num}/{den})"
    return pct


def _fmt_num(val, suffix=""):
    if val is None:
        return "—"
    if isinstance(val, float):
        return f"{val:.1f}{suffix}"
    return f"{val}{suffix}"


def render_markdown(report):
    """Render the weekly review report dict as Markdown."""
    lines = [
        f"# Weekly Lane Review (last {report['window_days']:g}d — {report['total_jobs']} jobs)",
        "",
        "## Task Type × Agent/Model",
        "",
        "| Task Type | Agent/Model | Jobs | Pass Rate | Approve Rate | Median Min | Median Tokens | Stuck / Limit / Mem |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    if not report["table"]:
        lines.append("| — | — | 0 | — | — | — | — | 0 / 0 / 0 |")
    else:
        for r in report["table"]:
            pass_str = _fmt_pct(r["pass_rate"], r["passed"], r["jobs"])
            app_str = _fmt_pct(r["approve_rate"], r["approved"], r["decided"]) if r["decided"] else "—"
            min_str = _fmt_num(r["median_minutes"])
            tok_str = _fmt_num(r["median_tokens"])
            stops_str = f"{r['stuck_stops']} / {r['limit_stops']} / {r['memory_stops']}"
            lines.append(
                f"| {r['task_type']} | {r['agent_model']} | {r['jobs']} | {pass_str} | "
                f"{app_str} | {min_str} | {tok_str} | {stops_str} |"
            )

    lines.extend(["", "## Worst Lanes", ""])
    if not report["worst_lanes"]:
        lines.append("None in window.")
    else:
        for w in report["worst_lanes"]:
            tags = ", ".join(w["categories"])
            lines.append(
                f"- **{w['lane']}** (`{w['job_id']}`, `{w['task_type']}`, `{w['agent_model']}`) [{tags}]"
            )
            for autopsy_line in w["autopsy"][:2]:
                lines.append(f"  {autopsy_line}")

    lines.extend(["", "## Provider-Limit Events by Template", ""])
    if not report["provider_limits"]:
        lines.append("None in window.")
    else:
        lines.append("| Template | Events | Job Stops | Active Node Limits | Resets At | Detail |")
        lines.append("| --- | ---: | ---: | ---: | --- | --- |")
        for p in report["provider_limits"]:
            resets = str(p["resets_at"]) if p.get("resets_at") else "—"
            msg = p.get("message") or "—"
            lines.append(
                f"| {p['template']} | {p['count']} | {p['job_stops']} | "
                f"{p['active_limits']} | {resets} | {msg} |"
            )

    lines.extend(["", "## Proposed Changes", ""])
    if not report["proposals"]:
        lines.append("No rule-triggered changes for this window.")
    else:
        for idx, prop in enumerate(report["proposals"][:3], start=1):
            lines.append(f"{idx}. {prop}")

    return "\n".join(lines) + "\n"
