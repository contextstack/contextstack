# Runner characterisation fixtures, contract v1

Pin the pre-1b runner's external behaviour, exactly as it was at commit
`b8534dca0`, so that the phase 1b shrink (Plan.CS.v2.1 §4) has a fixed
point to be measured against. `check` builds a temporary estate per case,
serves a scripted fake gateway (`_fake_gateway.py`), runs the runner binary
as a process and compares everything visible at the process boundary with
`cases/<name>/expected.json`: exit code, stdout, stderr, ledger rows, run-dir
entries, transcript, frozen stimulus, workspace files, every outbound gateway
request, and every op the runner invoked (name, cwd, env names, stdin).

This set is pinned, not live. Unset, `check` extracts `cmd`, `internal`,
`go.mod`, `go.sum`, `prompts`, `tools/bin`, `hooks/bin` and `ops/bin` from
git ref `V1_REF` (`b8534dca0`; override with `$CS_RUNNER_V1_REF`) with
`git archive`, builds `./cmd/lab` inside that tree and copies the real
tools, hooks, ops and prompt from it, never from the working tree. The
working tree can change freely without touching this set's verdict.
`$CS_RUNNER_BIN` still overrides the binary when set.

These fixtures are not re-cut when the runner changes. Phase 1b's contract
is followed by `../runner-characterisation-v2`, which shares `_harness.py`
from this directory and has its own `cases/`. The diff between the two
`cases/` trees is the record of what the shrink changed. A mismatch here
prints the first differing field and the full normalised actual output and
is dispositioned by a human as fixture wrong, Go bug, or deliberate
contract change; there is no `--update`. `--dump DIR` writes the actual
output for review.
