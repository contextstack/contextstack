# PINNED default model

`model:` is required in every spec. PINNED.md's `pinned:` line
(claude-code-sonnet since 2026-11-09, a subscription CLI template; local-coder before) is the default a spec author
copies, not a field a spec inherits. A spec without `model:` is refused
three times, each naming the pinned value: the runner at load, `enqueue`
before it queues, and `scripts/check` rule 3 before it commits.
