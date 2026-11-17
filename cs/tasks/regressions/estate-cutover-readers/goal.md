Every reader of a state shelf reads a cut-over root exactly as it read the
shelves before the cutover (workstation, 2026-11-11).

Ops/bin/estate-cutover appends one {"event": "estate_cutover",...} row to
every shelf. It has no id, run_id, slot or verdict. On workstation, node-listen then
logged `schedule-tick: 'id'` every minute: _void.schedule_rows indexed
row["id"] on every row of schedules.jsonl. trigger-check (_trigger_metrics)
and pending-judgments crashed the same way. run-identity's "latest row" became
the cutover row. shadow-report counted it as an unknown shadow event, which
raises shadow-cycle's unknown_shadow_events alert.

The rule is in one place: _estate_io.read_jsonl/iter_jsonl skip
BOOKKEEPING_EVENTS rows unless asked (with_bookkeeping=True), and readers
that parse rows themselves test _estate_io.bookkeeping(row). estate-cutover
verify and estate-identity.py still read the raw bytes.

Pass, on a fixture root with rows on all 15 shelves, cut over dev -> workstation by
the real estate-cutover, and on its twin with only the estate_cutover rows
removed: every reader op, script and helper (schedule-tick dry and as
node-listen runs it, node-listen's schedules report, void-watch,
trigger-check, pending-judgments, shadow-report, run-identity,
corpus-audit, prune-workspaces, run-prune, await, lifecycle-check,
shadow-casebook-pack, scripts/report, the _void/_dispatch/_trigger_metrics/
_shadow_*/_decision/... helpers) runs without a traceback and prints the
same on both roots. The shelves they append to end with the same domain
rows, and verify still passes. `check --state-copy DIR` runs the same against
a copy of a real state root, never ~/.contextstack itself.
Run: `tasks/regressions/estate-cutover-readers/check`, about 5 s.
