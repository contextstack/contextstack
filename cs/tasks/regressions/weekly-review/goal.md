# Regression: `cs review weekly`

Verifies `cs/scripts/cs_review.py` and `cs review weekly [--since DAYS] [--json] [--plist]`
against a real `director-rs` instance (`director-spec` harness `RustTarget` + `Node2`)
seeded with a dozen lane jobs and runs across multiple `task_type` × `agent/model` pairs:

1. Pulls the last `N` days of lane jobs using the owner credential (excluding older jobs outside `--since`).
2. Computes the `task_type × agent/model` table: jobs, pass rate, approve rate, median minutes,
   median tokens, and `stuck` / `provider_limit` / `memory_cap` stop counts.
3. Identifies the worst lanes (rejected, stuck, retried, errored), each with a two-line autopsy
   synthesized from the run's reason/decision note and `stuck.md` when present, with secrets redacted.
4. Aggregates provider-limit events by template across runs and node status.
5. Emits up to three concrete, checkable proposed changes from deterministic rules over the table
   (including `"reviewer profile: move from cursor-codex to codex — 4/5 rejects came from it"`),
   plus a valid weekly launchd plist template via `--plist`.
