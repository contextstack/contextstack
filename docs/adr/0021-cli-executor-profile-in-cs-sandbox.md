# ADR 0021: Generate the coding-agent sandbox profile in cs-sandbox

**Status:** Proposed  
**Date:** 2026-11-11

## Context

Lab tools run under `cs-sandbox` (deny-default Seatbelt on macOS, bwrap on Linux). Coding-agent
jobs run under a separate profile written by `cs/ops/bin/_executor.py` (`sandbox_cmd`: permissive
`(allow default)` plus write narrowing, read denies from `_cli_native.read_rules`, lane rules from
`_lanes.sandbox_rules`, login write exceptions, and the fcntl deny). Fixes to one path do not reach
the other; Linux agent jobs are refused unless `CS_EXECUTOR_ALLOW_UNSANDBOXED=1`.

The executor design calls for a `cli-executor` profile for Claude Code, Codex, Cursor, and peers: filesystem
scoped to the workspace or lane worktree (plus its git admin dir), a private run tmp, staged login
symlinks and `login_writes`, the CLI binary and its runtime paths, and `net: all` until an egress proxy
exists.

## Decision

1. **Host policy** lists `cli-executor` under `profiles` in `cs/policy/sandbox.json` (initially
   empty extra reads; network stays `all` in the emitted Seatbelt until the proxy ships).

2. **Request shape** (`executor-request.json`, written by the executor parent, not checked in):

   ```json
   {
     "workspace": "/abs/workspace",
     "tmpdir": "/abs/run-tmp",
     "home": "/abs/real-home",
     "extra_rules": ["(allow …)", "(deny …)"],
     "read_links": ["/abs/staged-link"],
     "login_writes": [{"literal": "~/.codex/auth.json"}, {"regex": "~/.codex/\\.auth-…"}],
     "protected": "/abs/budget-controls"
   }
   ```

   `extra_rules` carries lane `sandbox_rules` and template `deny_write` lines; `read_links` and
   `login_writes` mirror `_cli_native.stage` output and template fields.

3. **Emit mode** (macOS today):  
   `cs-sandbox --policy cs/policy/sandbox.json --profile cli-executor --request FILE`  
   prints a Seatbelt profile equivalent to today's `sandbox.sb` (no tool run). Linux emit is deferred
   until bwrap layout for executors is specified; running the CLI still goes through emit + `sandbox-exec`
   on macOS first.

4. **Executor cutover** (follow-up): `_executor.py` calls emit (or `cs-sandbox --policy … --profile
   cli-executor --request … -- /bin/sh -c 'sandbox-exec -p …'`) instead of `sandbox_cmd` writing
   `sandbox.sb`, with `CS_EXECUTOR_LEGACY_SANDBOX=1` keeping the Python writer for one release when
   golden tests are green.

## Consequences

- One implementation owns executor and tool sandbox semantics long-term; short term only emit is shared.
- Golden tests in `cs-sandbox` compare emit output to `sandbox_cmd` for representative templates.
- Nested `sandbox-exec` in agent CI stays blocked; the lead runs Darwin probe regressions at landing.
