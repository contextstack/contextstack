Commit messages accept ordinary changes and unregistered request descriptions.
The hook strips co-author and generated attribution, runs both remaining gates,
and propagates a failed gate. No allocator, specs or ledger is needed.
Run: `tasks/regressions/commit-message/check`.
