# cs-cli

On a laptop node, seeing what runs took someone over SSH typing launchctl,
curl and log paths; its Spark tunnels sat at exit 78 (a `WorkingDirectory` that no
longer existed) and its gateway prompted for the Keychain. The owner,
2026-11-10: a per-machine `cs`, Python, reading the install.

In a fake HOME built as node-install-layout builds one (two releases, a
director receiver), with `launchctl`, `curl` and `ssh` shims first on PATH,
running the release's own `code_root/scripts/cs`, the check must show:

1. `cs status --json` and `cs status` (one screen): the release, each
   service up or down with pid, last exit and a reason (the tunnel's exit 78
   names its missing `WorkingDirectory`), gateway health and catalog
   revision, the credential mode, the node's estate, director URL and
   whether it answers, the token set or not, the last pull, the outbox
   backlog, which tunnel ports answer, free disk.
2. `cs models` lists name, provider, available from `/v1/models`.
3. `cs jobs` reads the state root: accepted, running, finished, and a
   finished job waiting to send; the run-envelope backlog.
4. `cs logs gateway|node|tunnels -n N` tails the plists' log files; `-f`
   follows a new line.
5. `cs doctor` fails, each with a fix line: a stale tunnel control socket
   (ssh `-O check` fails), a plist `WorkingDirectory` that does not exist,
   an agent PATH without `/opt/homebrew/bin`, a node gateway on the Keychain
   (`CSGATEWAY_CREDENTIALS` unset), roots missing or not 0700, a PATH link
   into a missing release; and nothing else on a sound install.
6. `cs stop|start|restart` go through launchctl bootout, bootstrap and
   `kickstart -k`; `cs release list|switch|rollback` go through
   node-install (current moves; an unknown id is refused, current kept).
7. `CS_PLATFORM=linux`: start and status explain an unreachable systemd user manager;
   doctor still runs without a traceback. A HOME without an install: a clear message.
8. `cs ui --print` prints a one-time Director UI URL from a stub
   `bin/director-rs ui-link` with the install's `--data-dir` and a
   non-default `--listen`; `cs ui` opens it via `open`/`xdg-open` on
   PATH; no install is a clear error. The URL is never written to a log.
9. No value from `config/node.env`, a credentials file, a plist's or
   launchctl's environment, or a `*_API_KEY=` log line appears in any output
   or shim call; `security` is never called. ContextStack-owned PATH links
   under `~/.local/bin` and `com.contextstack.*` LaunchAgents plists are
   untouched; other files there may change.

Fails when `code_root/scripts/cs` is absent. Needs uv and the
pinned uv-managed interpreter; else SKIP and why. About 25 s.
Run: `tasks/regressions/cs-cli/check`.
