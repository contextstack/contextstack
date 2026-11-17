# cs-mcp

Demand: Plan.RealWork.Install (2026-11-11, the owner: "Recommended"), real-work
M1 and Proposal.RealWork.Orchestrator G1. The lead agent (Claude Code in the
terminal now, a hosted lead later) runs lanes through the Director instead of
spawning subagents, so it needs the Client API as MCP tools: `cs mcp`.

Against a real `director-rs serve` on a temp data dir and a free port, with a
protocol-2 stand-in node (director-spec's harness), driving `cs mcp` over
stdio JSON-RPC, the check must show:

1. `initialize` and `tools/list` answer; the eleven tools are listed.
   `list_agents` lists the shipped profiles (cs/agents: coder, reviewer,
   researcher, verifier), all parsing; a budget with seconds over 3600 is
   refused before any upload.
2. `start_task` submits an intent whose bundle is spec.yaml (model
   cursor-sol, the Director's `intent:` stamp, `repo:` and `lane:`),
   prompts/system.md, tasks/<lane>/goal.md and an executable check (the
   "no check" when none is given); meta names source cs-mcp and the lane.
   Shipped profiles default to Cursor templates (coder cursor-sol, reviewer
   cursor-codex, researcher cursor-composer, verifier cursor-agent).
3. `list_tasks` and `task_status` show it offered; after the node accepts and
   sends live deltas, `task_status` names the live run and its event count
   and `task_output` returns the partial text.
4. `wait_tasks` times out while the job runs, then returns early, naming the
   job, when the node reports it over during the wait. A repo without a lane,
   or a lane name the executor would refuse, is refused before any upload.
5. `task_output` then gives the check verdict, diff stat and transcript tail;
   `decide` approves the run and `task_status` shows the decision. An errored
   job end (node `job_terminal` with a short `reason`, no run) shows that
   reason as `error` on `task_status`, `list_tasks` and `wait_tasks`, and
   `task_output` returns it instead of "no run yet".
6. `cancel_task` records a cancel on a second lane; `usage` and `machines`
   answer; `land_task` relays `cs land --json` (refused for an unknown repo)
   or says it is not in this release; an unknown job is an
   `isError` result, not a crash.
7. The local credential never appears in the server's output.
8. Sub-agent types come from the Director code root; local `CS_AGENTS_DIR` profiles cannot affect `list_agents` or start_task. `list_agents`
   reads the Director profiles, ignoring local-only profiles;
   `start_task(agent=…)` carries the profile's model, budget, system prompt
   and check (a body without #! gets /bin/sh), and meta names `agent`,
   `agent_sha256` (the profile file's hash), `model` and `task_type`;
   explicit arguments win (budget merged key by key); a read-only profile in
   a lane gets a check that fails if the worktree changed. An unknown agent
   (naming the Director's known ones), a valid local-only profile, explicit
   seconds over 3600, repo+lane for a `lane: false` profile and none for a
   `lane: true` one are refused with nothing uploaded or submitted.

Needs target/release/cs-director (or $DIRECTOR_BIN), else SKIP. About 15 s.
Run: `tasks/regressions/cs-mcp/check`.
