# estate-split

The contract of CS phase 6 on the node side (Plan.CS.v2.1 §9, §10 phase 6), so a later
change cannot weaken it silently. A fleet node is a host whose `policy/spool.json` receiver is
`{kind: "director", url, node, token_env}`; its code root holds no `ledger/`, `specs/` or `tasks/`.

- Jobs come only from the director (`ops/bin/job-pull`): every object sha256-verified, the bundle
  materialised once under `<state>/jobs/<job>/`, queued once, then accepted. A bad object: nothing
  written, nothing accepted.
- A job's relative prompts resolve in its job dir (`capture-stimulus`), never a config root.
- Every fact goes back through the outbox (`spool-send` kind `director`): the run envelope and the
  job's terminal row (`job_terminal:<job>`). Unreachable or unacked = still pending; nothing lost.
- The director's row is the node's row byte for byte; a re-sent event gets the same ack, no second row.
- `retention: prune_acked` deletes a workspace only after an ack naming its envelope with the run restored.
- A tick killed mid-run: the next tick closes the job `node_restart` and spools it; it never re-runs it.
- An accepted job survives to the next tick and runs exactly once.
- A job may carry a grant (`grants/<id>.yaml`, hashed into the bundle): the node enforces it before
  queueing (active, unexpired, estate = the node's ESTATE); a refused grant runs nothing and the job closes
  at once (`grant:...`, `node_config`), reported like any terminal row. The run freezes the carried copy.
- `schedule-tick` fires a schedule naming `node` through `CS_DIRECTOR_DISPATCH` (never `enqueue`) and reads
  the slot's state from `CS_DIRECTOR_ESTATE_ROOT/dispatch.jsonl`; its grant must name that node as estate.

Run: `tasks/regressions/estate-split/check` (`CS_RUNNER_BIN` picks the runner; default the Rust `../target/release/cs-harness`).
`_fake_director.py` stands in for the Director (`api/v1/nodes/me`); the Rust Director's own side of the
node protocol is `tests/director-spec/run --side director`.
