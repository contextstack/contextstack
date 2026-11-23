# env-policy

The contract. `policy/env.json` is node policy for what a child of the runner
inherits (Plan.CS.v2.1 §4, decision 20): `always` reaches every child, a spec
may request names with `env: [NAME]`, and only `on_request` can grant them.
The launcher resolves the request and hands the runner CS_CHILD_ENV; the
runner never reads the policy and honours no request on its own.

- default: the resolver's fallback, the committed `always`, and the Go
  runner's compiled literal (`defaultChildEnv` in internal/code_root/run.go) are
  one list.
- request: the launcher reads `env:` as the Go loader does (spec.go, a YAML
  list): flow form bare or quoted, block form (`- NAME` lines), `env: []`,
  and a trailing ` #` comment is never a name; a refused name is refused by
  its bare name in every form; a bare scalar (`env: NAME`) is refused as the
  loader refuses it, never read as a one-item request.
- run-spec: a permitted request reaches the cell's CS_CHILD_ENV; a refused
  request exits 2 naming the variable and the file before any cell runs.
- tick: `child_env` sets CS_CHILD_ENV from the resolution and never inherits
  one; `run_spec` turns a refusal into a config-void trial row (`stop`
  `env_policy: env "NAME": not allowed by policy/env.json`) and launches
  nothing.
- finalize-run: its children (check, publish-evidence) get `always` only,
  whatever the spec requested or CS_CHILD_ENV says.
- malformed file: a named `policy/env.json:` error, never a silent default.

Run: tasks/regressions/env-policy/check. Isolated estates only; never
touches cs/ledger. The runner's own side is `TestChildEnvironmentAllowlist`
and runner-characterisation-v2 (`env-policy-launcher`, `env-request-denied`,
`env-direct-run-ignores-request`, `env-policy-empty`: CS_CHILD_ENV set but
empty is an empty child environment, never the compiled default).
