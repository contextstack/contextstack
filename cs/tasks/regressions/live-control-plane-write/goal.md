# Live control-plane write isolation

A task check may use `Path.cwd` as its fixture root, but it must
refuse to run when that root is the canonical code_root checkout before
writing any code_root control-plane file. Scan every task check so a future
ledger, dispatch, grant, spec, or executor-template writer cannot be
introduced without the same guard.

Spawned by the 2026-09-01 reconstruction of the tail after
`tasks/work/grants-as-data/check` truncated the live ledger.
