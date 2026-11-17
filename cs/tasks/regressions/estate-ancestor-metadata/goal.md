# estate-ancestor-metadata

A run's private tmp lives under `~/.contextstack/state/runs/<id>/tmp`, and the agent profile denies the rest of `~/.contextstack`. Path resolution (`realpath(3)`, which Rust's `canonicalize` uses) needs metadata on every parent. The profile grants metadata only, never contents, on those parents.

On 2026-11-12, release d5567b60c denied that metadata, so every Codex job failed with "failed to canonicalize CODEX_HOME".

Host-only (`--darwin-probe`). Under the real profile, the check verifies three things:
- the run tmp resolves;
- a sibling run's file stays unreadable;
- the runs directory stays unlistable.
