---
host_only: requires applying a fresh macOS Seatbelt profile from the host
---
# attack-config-symlink

A lane lands executable editor or CI config without the owner's flag.

Host-only (`check --darwin-probe` on macOS). Otherwise it prints SKIP and exits 0.
If nested `sandbox-exec` is refused (`sandbox_apply: Operation not permitted`), it
prints SKIP and exits 0.

A temporary repo is set up as a repo lane (bare checkout plus worktrees, as
`tasks/regressions/repo-lanes` builds them) and landed with the real `land` in
`_lanes.py`, `allow_agent_config` left false. Each of these is its own lane:

- `.vscode` symlink to `editor/` plus `editor/tasks.json`
- `.github` symlink to `gh/` plus `gh/workflows/x.yml`
- `.claude/settings.json` as a file symlink to a payload outside that directory
- `.VSCode/Tasks.json`
- `.vscode/mcp.json`
- `.husky/pre-commit`
- `.devcontainer/devcontainer.json`
- an edit to `cs/scripts/check`
- an edit to `cs/scripts/hooks/pre-commit`

A route fails when that lane lands. Output is booleans and route names only.
