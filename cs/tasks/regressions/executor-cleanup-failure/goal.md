# executor-cleanup-failure

A job-process cleanup or tracking `RuntimeError` (seen live: `job process cleanup deadline exceeded`
from `_marked_pids`) after the CLI finished must not crash the executor before it writes a row. The
executor keeps the CLI output in `cli.log` and ends with a retryable `interrupted` row, not a
non-retryable `node_config`/`no_row` void.
