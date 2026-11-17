A coding-agent run streams while the agent runs, and leaves the run artifacts the Rust runner leaves
Under the gateway (`--gateway-owned`) each line the agent prints reaches the executor's
stdout as it arrives, so a streamed lab-execute delivers live deltas; the executor's report stays the
last line. The agent's text goes to `<run_dir>/events.jsonl` in runner.rs `Events` lines (seq from 1,
kind model.output, payload turn/partial/delta, closed with partial false, plus discard true when the
attempt failed, timed out or was cancelled). Its reply goes to `messages.jsonl` as a `turn` event (an
`error` event when it failed), so a conversation turn on this path has its reply. A cancel mid-run
kills the agent and still closes the turn with discard; usage parsing is unchanged. Fake agents only.
