# director-job-refused-spec

A fleet node receives a job with an unknown spec field (`surprise: true`). The runner refuses it.
The node must report the failure and close the dispatch cleanly.

The contract, so the next fall is loud:

- A trial child (the runner, or the executor) that exits without an execution row ends the job at once: no
  later trial starts. The trial row is `verdict: void`, `stop: "error: no_row: <reason>"`, and the error object
  `{class: node_config, code: no_row, source: runner}`, where the reason is the child's last stderr lines.
- tick never crashes on it: it appends the job's `error` terminal row (closing the `running` row), spools a
  Director job's `job_terminal`, and exits with its JSON line.
- The Director learns the job ended in `error`, with the reason.

Director task links (`intent: <ULID>`) are optional for hand-written specs and accepted when stamped.

Run: `tasks/regressions/director-job-refused-spec/check` (`CS_RUNNER_BIN` picks the runner). It reuses
estate-split's stand-in Director and the characterisation fake gateway.
