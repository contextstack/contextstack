# finalize-run-recovery

The contract. `ops/bin/finalize-run` turns a run dir whose execution has
ended (`terminal.json` present) into the check result, the transcript
footer, the evidence record and the ledger row, exactly once, and marks
the dir done with `finalized.json`. Plan.CS.v2.1 §4 ("Recovery contract
for `finalize-run`") names two tests as part of the op's definition and
one alert as the only thing that keeps a stuck finalization from going
silent. This task is those tests.

- concurrent: two finalizers started together on one run dir produce one
  row, one evidence record and one `finalized.json`; the loser reports
  `pending`, never `finalized`.
- kill-retry: the op is killed after each step in turn (`check`,
  `transcript`, `publish`, `row`, via the test-only
  `FINALIZE_RUN_CRASH_AFTER`); the retry keeps the original `check.json`
  verdict although a rerun would now fail, appends no second row or
  record, and completes.
- estate: a missing `ESTATE` refuses the row and leaves the dir pending;
  `pending-judgments` raises exactly one `finalization_pending` alert
  after the grace window, with `first_observed` fixed at the run's
  `ended` and the last `finalize.error`; a second failed retry does not
  reset it; the third retry, with `ESTATE` restored, clears it.
- publish: a failing `publish-evidence` still lets the row through,
  records `finalize.error`, leaves `finalized.json` absent; the retry
  publishes and appends nothing.
- marker: a complete `evidence/manifest.json` without `ready_for_inspection`
  (a crash between publish-evidence's two writes) is not a published run;
  the retry calls `publish-evidence` once, which writes the marker, and
  finalizes. With both present, or with a manifest whose `capture_state`
  is incomplete (publish-evidence never marks one), the retry calls it never.
- mismatch: a `check.json` whose `stimulus_sha` differs from
  `terminal.json`'s (an empty one included) is a fixture failure, not a
  rerun: pending, the reason naming both hashes, nothing appended or
  rendered; a matching file is reused as `existing`.
- refusals: paths outside `ledger/runs`, or a dir with no `terminal.json`.
- env: the check inherits only the runner's child allowlist.
- tick: a runner that exits 3 yields a dispatch TERMINAL row with
  `stop: finalization_pending`, `code: 3` and no invented ledger row,
  both when no row landed (`ESTATE` missing) and when the finalizer's own
  row did and publish failed; the completion callback is withheld
  (observed: no `ready_run` reaches the `shadow-cycle` stub for a
  production job); the next tick's `finalize_sweep` finishes the dir,
  appends no second row and reports its counts.
- orphan sweep (follow-up): a runner that dies before
  `terminal.json` leaves its `<state>/sandbox-reports/<cell>` dir behind.
  The sweep then removes a cell dir whose newest entry is older than
  `ORPHAN_AGE_S` (24 h, `_sandbox_policy`) when every report in it is
  readable and no pending run (terminal.json, no finalized.json) has its
  `cwd` as workspace; it keeps a young dir (not counted), a claimed one and
  one holding a torn report (counted `orphans_kept`) and reports
  `orphans_removed` / `orphans_kept` in `finalize_sweep`.

Run:

    tasks/regressions/finalize-run-recovery/check [--keep]

The template run dir is real: the default Rust runner (or
`$CS_RUNNER_BIN`) runs the create-file task against a scripted fake
gateway with a stub `finalize-run` that exits 3, so `terminal.json` and
the body-only transcript are on disk and nothing is finalized. Every test
copies that estate. Never touches `cs/ledger`. About 9 s.
