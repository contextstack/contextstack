Exercise the tracing proposal v5.2 Step 0 and Step 0b gates with real producers.
Run the Rust harness against a real gateway and a loopback provider, let
`finalize-run` publish its evidence, and give the checker the merged records
and run directories. For partial settlement, send the actual finalized envelope
to a fresh Director and compare its settled job charge with the gateway's
durable spend counter. Check the Director work trace alongside the retry run.

Build the release binaries from the repository root with `cargo build --release`.
Run `cs/tasks/regressions/trace-e2e/check` for every case, or append letters to
select cases, for example `cs/tasks/regressions/trace-e2e/check A G J`.
The suite uses scratch homes, ephemeral loopback servers, and fixture Git
configuration. It honors `CSGATEWAY_BIN`, `CS_RUNNER_BIN`, and `DIRECTOR_BIN`.

The loopback transport in J gives the gateway a shorter deadline than the
harness so the terminal billing chunk can reach the harness. In O it models
a stale streaming capability and supplies the two gateway sends to the checker
in reverse order after the run. In R–T the fixture node reports a stuck first
dispatch and publishes a real run for the retry. U sends three canned native
CLI events through the machine launcher, harness, gateway, and finisher. X
uses the same route to verify a live Director event, cancellation before
launch, a cancelled receipt and ledger row, and cleanup of the fake CLI and
its descendant. Y starts a native harness lane through MCP, edits the worktree
through the real gateway and harness, then checks the committed branch, the
machine verdict, reported usage and the merged trace.

In L and O the provider outlasts the run deadline. The harness keeps the
in-flight call alive during its settlement grace period and receives the
gateway's deadline response. Both traces must be complete with no findings,
all sends marked delivered, and the terminal charge equal to the gateway
settlement. L requires a positive charge. O checks the streaming refusal and
the zero-charge fallback settlement in reverse exchange order.

The proposal's table expected a spend discrepancy in G and J. The
`partial-charge-total` fix on 2026-11-19 made the gateway's settled partial
charge reach the harness run total. Both cases now expect `complete` with no
findings and require the gateway settlement, harness terminal charge, and
Director settled job charge to agree. N expects a zero wallet charge and
allows a positive estimate. P and Q require a collector and are out of scope.
