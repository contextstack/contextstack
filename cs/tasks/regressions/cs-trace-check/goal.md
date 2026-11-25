# cs-trace-check

Checking a run's trace used to mean assembling the run `trace.jsonl`, the
gateway's exchange file, and a `run_directories` map by hand. `cs trace check`
does that and grades with the release's `tracing/check-trace`.

In a temp state root (never the real `~/.contextstack`), with `hermetic_env()`:

1. A CLI-only run (no gateway trace) checks `complete`.
2. A harness run is `incomplete` without its gateway trace and `complete` once
   that file is present.
3. A run with a delivery finding reports `complete_with_findings`.
4. Exit codes: 0 complete, 1 findings, 2 incomplete/missing.
5. `--last` orders by the run-id timestamp (else mtime) and skips non-run
   folders such as `regression-zzz` that lack `terminal.json` / `trace.jsonl`.

Run: `env -u CS_STATE_ROOT tasks/regressions/cs-trace-check/check`.
