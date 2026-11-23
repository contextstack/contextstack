# backup-omits-terminal-record

The fall (observed 2026-11-09, notes.md). `ops/bin/evidence-backup`
retained `stimulus/`, `evidence/`, `transcript.md`, `transcript.jsonl`,
`check.out`, `check.output` and `ready_for_inspection`. Since phase 1b a
run dir also holds `terminal.json` (the runner's terminal record),
`messages.jsonl` (the structured transcript), `check.json` (the persisted
check result) and `finalized.json`; none of them reached the DGX, so the
M4 held the only copy.

The check takes a run dir's entries from the v2 fixture
`happy-fs-write` (what both runners leave after finalize-run), builds that
dir, and runs the real op through GNU rsync with the op's own filter (a
fake ssh; the remote target is a local dir). Every entry except
`workspace/`, `finalize.lock`, and `execution.lock` must arrive, including
`execution-receipt.json`.

Run: `tasks/regressions/backup-omits-terminal-record/check`. Skips (exit 0,
one line) when no GNU rsync 3.x is installed: the op refuses then anyway.
