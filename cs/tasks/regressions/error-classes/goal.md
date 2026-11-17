# error-classes

The error vocabulary `error-v1` (Plan.CS.v2.1 §4, phase 4b E1–E2) as a contract.
`ops/bin/_errors.py` classifies what the runners already write: terminal.json's `stop` for the
run, messages.jsonl `tool` events for each call. finalize-run writes the object on row_version 2
rows; every other reader calls `row_error(row)`, which classifies a version 1 row on read.

Because the input is the runners' fixtured strings, a reworded message must stay loud: every
`error:` stop in the fixed ledger extract (`historical-stops.json`) must classify to a class other
than `unclassified`, every non-error stop to none, and the per-class counts are pinned. tick's
dispatch rows carry the same object (orphaned_running = `node_restart`; launcher refusals =
`node_config` with code = the kind); `_void`'s readers answer identically for v1 and v2 rows.

Run: `tasks/regressions/error-classes/check`. The row shape under both runners is pinned by the
v2 characterisation cases `error-*`, `call-errors-*` and `happy-row-v2`.
