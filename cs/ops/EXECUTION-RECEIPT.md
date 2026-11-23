# Execution receipt, version 1

[JSON Schema](execution-receipt.schema.json)

For CLI jobs the machine's `executor` launcher starts `cs-harness cli-run`.
The harness reserves the UTC run directory and requests a receipt from the
gateway. The gateway starts its child in `--gateway-owned --execution-only`
mode. The child writes `execution-receipt.json` atomically and returns the same
JSON over a private channel after execution classification, usage collection,
and login cleanup. Its stdout carries CLI events. The harness holds
`execution.lock` through exit, then the launcher hands the receipt to
`machine-finish`. Exit 0 from the gateway child means it produced a receipt.

For API/DGX cells, `cs-harness run` executes one cell and writes an
`adapter: "harness"` receipt. Its stdout identifies the run directory. The
process holds `execution.lock` until exit so a concurrent sweep cannot begin
finishing while the harness is alive. The machine's `run-api` launcher waits
for the process, invokes `machine-finish`, and prints the cell summary. Tick
and `run-spec` both use this launcher.

Invoke `ops/bin/machine-finish` after the execution process exits and its output
has drained. Send a JSON object with `run_dir` (absolute run directory) and
`receipt` (the versioned object) to stdin. The command verifies the receipt
against the machine's saved record and checks the frozen stimulus manifest
and its input hashes when present. Exit 0 means pass, 1 means a non-passing
verdict, 2 means invalid request, and 3 means pending. For a harness
receipt it writes the terminal record and calls `finalize-run`; the response
carries the finalized verdict or pending state. Tick sweeps receipt-only runs
after the usual grace period. Once `terminal.json` exists, `finalize-run`
resumes from the durable check and publication records.

- `run_id`, `job_id`, `execution_id`, and `trial` identify the execution.
  The gateway binds these fields to the registered work order and the run
  directory reserved by the harness.
- `native` describes execution, with a classified `stop` and a nullable
  process `exit`. `stimulus` refers to the frozen manifest and its SHA-256.
- `workspace`, `finish`, and `sandbox` carry machine-local references and
  the frozen check settings. `finish.mode` selects ordinary task finishing,
  a semantic no-work check, hosted-turn finishing, a profiled CLI check, or a
  terminal outcome that skips the check. A terminal outcome supplies an
  already determined verdict.
- For the harness, `finish.mode` is `harness`, `sandbox` is null, and `row`
  carries the terminal metadata. The existing finalizer supplies the verdict
  and ledger row from that metadata and the frozen check. A void execution
  can have an empty stimulus hash if capture failed.
- Executor `row` contains execution-era CLI ledger fields. The finisher adds the
  verdict, final stop, evidence identity, and branch or lane commit when
  applicable. `usage` mirrors reported execution usage. Missing or null
  usage fields represent unknown values.
- `trace` carries metadata for closing the observed run trace on the machine.

The receipt is trusted machine metadata. Keep it and the referenced run state
outside the agent's write grant. The gateway frames the receipt independently of agent stdout. Paths, grants, and sandbox
arguments from agent output are never receipt inputs.
