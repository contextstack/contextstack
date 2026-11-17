# state-root-readers

The contract of CS phase 6 step 10 (ADR 0008's legacy readers): every op that reads or
appends a state shelf (ledger, dispatch, finalizations, judgments, shadow, schedules, runs, locks,
journals) resolves it through `_estate_io.state_dir(lab)`, which is `<lab>/ledger` byte for byte unless
`LAB_STATE_ROOT` is set, then `_roots`' validated state root. So relocating an estate's state root moves
every reader at once, and a later op cannot quietly read the old in-repo shelf.

- Static: no `<lab> / "ledger/..."` path construction remains in `ops/bin` or `tools/bin` (the guarded
  fallback for estate copies without `state_dir`, and the repo-tier `estate-commit`, are the exceptions).
- Relocated: `run-identity` reads the relocated ledger and `<lab>/ledger` is never created.

Run: `tasks/regressions/state-root-readers/check`.
