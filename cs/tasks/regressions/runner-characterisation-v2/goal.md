# Runner characterisation fixtures, contract v2

Follow the runner through phase 1b of Plan.CS.v2.1 (§4, "Reduce the
runner's responsibilities first") and then be the fixture set the Rust
port is checked against. `check` builds a temporary estate per case,
serves a scripted fake gateway (`_fake_gateway.py`), runs the runner binary
as a process and compares everything visible at the process boundary with
`cases/<name>/expected.json`: exit code, stdout, stderr, ledger rows, run-dir
entries, transcript, frozen stimulus, workspace files, every outbound gateway
request, and every op the runner invoked (name, cwd, env names, stdin).

The binary under test is `$CS_RUNNER_BIN`. Unset, it is the default Rust
runner, `../target/release/cs-harness` (scripts/check builds it; ADR 0011),
and the working tree's real tools, hooks, ops and prompt are copied into each
estate. Pointing `$CS_RUNNER_BIN` at a Go `lab` build runs the identical
cases against the archived Go runner.

This set started as a byte-for-byte copy of `../runner-characterisation`
(contract v1, pinned to the pre-1b runner). One case changes or is added
per extraction in the §4 interface inventory: finalization (`terminal.json`,
`messages.jsonl`, `finalize-run`, exit status), the task × trial loop
(`run-spec`), `-step` (`pre_tool` hook, `step-confirm`), transcript
rendering, the `PINNED.md` fallback (`model` required), the environment
allowlist (`env:` and node policy, with a negative case), `image_out`
(neutral observation), and accounting (`est_usd`, `charged_usd`). Each
change lands with the extraction it describes, under the same
`Demanded-by:` as the Go change. The diff between this `cases/` tree and
v1's is the record of what the shrink changed.

The harness (`_harness.py`) is shared from the v1 directory; `cases/`,
`fixture/` and `_fake_gateway.py` are this set's own and may diverge. A
mismatch prints the first differing field and the full normalised actual
output and is dispositioned by a human as fixture wrong, Go bug, or
deliberate contract change; there is no `--update`. `--dump DIR` writes the
actual output for review.
