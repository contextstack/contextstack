# runner-identity

The contract (ADR 0011: the Go runner and Go gateway are archived,
Rust is the working line). Nothing picks the Go runner by default, and every
run records what executed it.

1. The selector (`ops/bin/_runner.py`): with neither `CS_RUNNER_BIN` nor
   `CS_RUNNER_BIN` set, the runner is the checkout's Rust build,
   `<repo>/target/release/cs-harness` (scripts/check builds it). An installed
   release selects `rust` (`bin/cs-harness`) unless `select --runner go`.
2. The record: the Rust runner passes the endpoint it posts to as
   `gateway_endpoint`; capture-stimulus writes `runner_path` and `gateway`
   {endpoint, implementation, version} (from the gateway's `/v1/health`,
   once, before the first call; null fields when it does not say) into the
   frozen manifest, outside env_sha. finalize-run copies them into the row as
   `executed_by` {runner {kind, path, sha256}, gateway}.
3. The report: `ops/bin/run-identity [RUN_ID]` (read-only; no id: the
   latest row) prints runner kind, path, sha256 (recorded and as the file is
   now) and the gateway, from the row, or from the manifest for older rows.

The fall this guards: a run launched with the default configuration silently
executed by the Go runner, with nothing in its evidence to say so.

Run: `tasks/regressions/runner-identity/check`. A temp code root, the built
Rust runner copied to where the default looks, a fake gateway whose health
names itself, no selector or roots in the environment; one `run-spec` cell,
then the row and `run-identity` are checked. Never touches the real state
root or `cs/ledger`.
