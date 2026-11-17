# 0019. Coding-agent runs and runner runs finish the same way

- Status: Accepted
- Date: 2026-11-10

## Context

A node runs work on two paths. The Rust runner (`cs-harness`) drives a model call by
call through the gateway. The executor runs a subscription coding agent
(Claude Code, Codex, Gemini CLI) as one process, also launched through the
gateway. The Director's features (cancel, live output, usage, conversation
replies) read a run's files and envelope. The executor read the agent's
output only when the agent exited, and wrote no `terminal.json`. So its runs
couldn't stream or be cancelled cleanly, and their reply never reached the
Director: a conversation turn on that path ended as an error.

## Decision

- Both paths leave the same record. Each writes `terminal.json` and
  `check.json` in the runner's shapes, `events.jsonl` (numbered live output,
  byte-identical lines) and `messages.jsonl` (the turn's reply or error). Each
  then calls `finalize-run`, which builds the same `run_finalized` envelope.
- Live output works on both. The runner reads streamed replies, and a
  finished streamed turn is identical to a non-streamed one. The executor
  passes each agent output line on as it arrives.
- Cancel works on both. The gateway stops a coding-agent run by its
  execution ID or by the job's `LAB_JOB_ID`, and the run ends `cancelled`
  with the output so far.
- Usage is recorded on both. Coding-agent runs record the tokens and
  allowance the agent reports, and `null` where it reports nothing.
- The Director doesn't need to know which path ran a job.

## Consequences

- Each execution path can grow on its own. A new feature on one is done only
  when the other produces the same records, or a case says why not.
- The executor carries a mapper for each agent's stream format, mirrored in
  the gateway (`agentstream.rs`). An agent changing its output format breaks
  both, and the `executor-streaming` and `agent-usage` regressions catch it.
- Not covered yet: the muse/profiled executor path.
- History: runner `a719b500d`; gateway `8cf0a1709`, `76bb1b398`; executor
  `88dd6b64f`, `6d9237876`, `b59d41ef8`.
