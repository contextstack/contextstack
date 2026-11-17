# claude-budget-hook-read

A budgeted Claude run can read its own `budget-controls/cli-budget-hook.py` under the real macOS profile, cannot write or unlink anything in `budget-controls/`, and still cannot read other run files such as `events.jsonl`.

Before the fix every budgeted Claude lane failed its first tool call with "can't open file …/cli-budget-hook.py" (2026-11-12).
