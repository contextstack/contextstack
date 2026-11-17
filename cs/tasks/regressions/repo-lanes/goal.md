# repo-lanes

Demand: Plan.RealWork.Install 2026-11-11, decisions 4 and 5 (the owner:
"Recommended"). Real work runs as lanes: a coding-agent job names `repo:`
and `lane:` in its spec, and the agent works in the git worktree
`<parent of main>/<lane>` on branch `lane/<lane>`; the lead lands a lane on
local main with `cs land REPO LANE` after the repo's gate is green. Only
the owner pushes.

With temp repos only (a gwt layout, bare `.bare/` plus worktrees, and a
plain repo), a temp repos file (`CS_REPOS_FILE`) and a fake agent, the
check must show:

1. The executor creates the lane worktree from main's head on
   `lane/<lane>`, runs the agent there, and on a pass commits its changes
   on the lane branch; main's head and work tree are untouched. A second
   job on the same lane reuses the worktree, its base the lane's head.
2. On macOS the agent's sandbox can write the worktree and its own git
   metadata (the agent can commit), and cannot write the main checkout,
   another lane, the repo's config or main's ref.
3. The evidence diff is the change against the base commit (committed and
   uncommitted), and capture is complete.
4. A spec naming one of repo/lane, an unknown repo, a bad lane name, or a
   task with `setup/` is refused before any ledger row.
5. `cs land`: green squash-merges into main, removes the worktree and the
   branch; a red explicit gate, or a red commit hook when the change is
   under `hook_gated`, leaves main exactly as it was and names the FAIL
   line; a conflict leaves main as it was; a dirty main, a dirty lane, an
   empty lane and a `check-red:` message are refused with nothing changed.
