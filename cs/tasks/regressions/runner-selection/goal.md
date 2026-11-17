A node selects the runner by CS_RUNNER_BIN, falling back to LAB_RUNNER_BIN,
then the checkout's Rust runner, ../target/release/cs-harness (Go is
archived and runs only when named). Changing that setting switches launch,
process accounting, rebuild and freshness checks together; clearing
it restores the Rust default.
Selection never changes the spec, task, trial, or executor route. Unknown
runner implementations are runnable but cannot be rebuilt as Go by mistake.
An installed release names its runners `bin/cs-go` and `bin/cs-harness`.
`runner_kind` maps them to Go and Rust, and freshness accepts them.
