# Coding-agent CLIs get a runtime they can write, and finish only on their own receipt

Cursor-agent, Antigravity (`agy`), Gemini CLI and Codex keep runtime state under `~/.cursor`,
`~/.gemini` and `~/.codex`, which the executor's sandbox denies writes to; the 2026-09-03 gemini-cli
runs on workstation all hung to the 300 s timeout with no output, and Codex failed in 2 s trying to
create PATH aliases and initialize its in-process app-server. Each template now names where its
CLI writes (`_cli_native`): cursor's `CURSOR_CONFIG_DIR`/`CURSOR_DATA_DIR`, gemini's `GEMINI_CLI_HOME`
with read-only links to its login files, agy's private HOME with the login Keychains linked in,
Codex's private `CODEX_HOME` with a 0600 `auth.json` copy and a sibling `TMPDIR` (release Codex
refuses PATH aliases when `CODEX_HOME` lies under `temp_dir`). Codex also bypasses its own sandbox
(the outer seatbelt remains). A zero exit without the CLI's own completion receipt (cursor's `result`
event, agy's `result.status` SUCCESS, gemini's `result.status` success) is a failure, not `done`.
API-key env never reaches the CLI; links into personal files are gone before evidence is kept. Cursor model templates (`cursor-sol`,
`cursor-codex`, `cursor-composer`, `cursor-grok`, `cursor-sonnet`) pass `--model <id>` and are admitted by
`gateway/models.json` on the cursor CLI pool; `cursor-agent` stays Auto (no `--model`). Real templates,
fake binaries; no model call.

Audit.CodexComparison.2026-11-11: `read-denies` verifies every template's profile denies
credential stores, Director state, gateway owner/node tokens, personal secrets and `.env*`
(except `.env.example`), followed by exact staged login target allows. Keychain symlinks
resolve to the real directory: Claude and private-home templates retain that directory
exception for their staged login; other templates deny it. Network remains open pending
per-template provider allowlists (BACKLOG). Linux launches through the native bubblewrap executor. Unsupported platforms refuse execution.
`scripts/check` runs `tasks/regressions/cli-agent-runtimes/read-denies --darwin-probe`
on macOS and prints a skip reason elsewhere. Run check outside the lead's sandbox:
nested sandbox-exec is blocked there. The probe uses only
planted files under a temporary HOME and tests blocked reads and permitted staged auth.

Issue #57: Codex and Pi refresh private auth copies, with no real-login write grants.
The host writes back valid, single-link JSON only when stable account identity matches
and the owner file still matches the staging snapshot; refusal reasons enter run evidence.
`login-copies` covers write-back with decoys. `login-rename-probe --darwin-probe` uses
the real profile and a temporary HOME: workspace rename onto owner auth must fail,
while rename onto the private copy succeeds. Pi locks also stay in its private home.
