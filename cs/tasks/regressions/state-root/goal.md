# state-root

The contract. Code, configuration, workspaces and state are four distinct
directories with distinct defaults, resolved by one function with validated
containment (Plan.CS.v2.1 §9 "Four roots"). The rule
lives in `ops/bin/_roots.py`. As with the environment allowlist (decision 20),
the launcher validates and the runner inherits: `tick` and `run-spec` call
`roots` before launching anything; the Go runner (`StateRoots`, run.go) only
reads `LAB_STATE_ROOT` and `LAB_WORKSPACES_ROOT` with the same defaults; every
op that touches state validates again on its own entry.

| variable              | default         | holds                                   |
| --------------------- | --------------- | --------------------------------------- |
| `LAB_CODE_ROOT`       | the checkout    | ops/bin, tools/bin, hooks/bin, specs    |
| `LAB_CONFIG_ROOT`     | code            | policy/, PINNED.md, ESTATE, prompts/    |
| `LAB_STATE_ROOT`      | `<code>/ledger` | runs/, ledger.jsonl, dispatch.jsonl     |
| `LAB_WORKSPACES_ROOT` | `<state>/runs`  | `<id>/workspace` (inside the run dir)   |

A set root must be an absolute existing directory (the code root: one holding
`INVARIANTS.md`); state and workspaces may not lie inside the code root unless
they equal the defaults. Containment is never relaxed to make relocation work.

A row's `transcript` is `ledger/runs/<id>` in every layout: the run's identifier
under the state root, which `_roots.run_dir` resolves; never a filesystem path.
The default layout is today's, byte for byte.

- default: with no `LAB_*_ROOT` set, `roots` equals today's paths; `run_dir`
  takes the transcript or a bare id and refuses every other shape; a root set
  to its own default is the default, not a refusal.
- relocated: one cell runs end to end with `LAB_STATE_ROOT` outside the estate
  (v2 characterisation case `state-root-outside-lab`, a direct `lab run` with
  the variable set): run dir, messages.jsonl, terminal.json, check.json,
  evidence/, finalized.json and the row land under the state root; the estate's
  `ledger/` gains nothing; every op is handed a state-root path and inherits
  `LAB_STATE_ROOT`; the transcript resolves.
- refusals: a relative, a missing, and an inside-the-code-root state root each
  refuse before anything is written. `run-spec` exits 2 naming the variable
  and launches no cell; `tick` records a config-void trial row `stop: roots:
  <message>` (holding the schedule) and launches nothing; `finalize-run` and
  `capture-stimulus` exit 2 on their own entry. The runner itself refuses
  nothing: that is the launcher's authority, as `env-policy` says of requests.
  A direct `lab run` with a bad root ends in `capture-stimulus`'s refusal, a
  pending run dir and no row. `finalize-run` still refuses a run dir outside
  `runs_dir` in both layouts and resolves the identifier under it.

Run:

    tasks/regressions/state-root/check [--keep]

Isolated estates only; the live `lab/ledger/runs` and `ledger.jsonl` are
asserted untouched. About 8 s; the default Rust runner unless `$CS_RUNNER_BIN`.
