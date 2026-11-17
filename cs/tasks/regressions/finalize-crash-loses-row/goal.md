# finalize-crash-loses-row

The fall (observed against the pre-1b runner, notes.md). The Go runner
computed the check verdict, wrote `transcript.md`, called
`publish-evidence` and appended the ledger row, in that order, inside
one `defer` (run.go:138-166 at b8534dca0). A process death after the
transcript and before the row lost the row, the check result and the
evidence record. The verdict survived only as prose in the footer.

Two claims, kept apart:

- Detection. For dispatched cells `tick` already notices: it takes
  `ledger_size()` before the trial and `execution_row(rel, sid, before)`
  after it, and raises "trial produced no fresh matching execution row"
  (ops/bin/tick:290, 303-305). A manual `lab run` has no such check: the
  caller gets exit status -9, nothing else.
- Recovery. Exists for neither. Nothing re-derives the row, the
  check result or the evidence record from what is on disk.

The boundary. SIGKILL at a known point, not SIGINT and not a sleep.
The temp estate's `ops/bin/publish-evidence` records its stdin, then
SIGKILLs the runner and its own parent (`finalize-run`; the pre-1b
runner itself) while both are blocked on it: check done, transcript on
disk, row not yet written. Deterministic; no timing. Fired once; later
calls forward to the real publisher so recovery can publish.

Run:

    tasks/regressions/finalize-crash-loses-row/check [--keep]

Builds a temp estate (seeded with two prior rows, one for the same
spec), a fake gateway scripted for the create-file task (fs_write then
a final text turn, so the check passes), and the default Rust runner
(`../target/release/cs-harness`) unless `CS_RUNNER_BIN` is set. Never touches lab/ledger.

Since extraction 1 of phase 1b the runner writes `terminal.json` and
calls `ops/bin/finalize-run`; the same kill now leaves `terminal.json`
and `check.json` behind and the recovery section is green:
`finalize-run` on the crashed run dir appends the row with the original
verdict and the evidence record, exactly once, and a second call appends
nothing. Wired into scripts/check as finalize-run's acceptance.
