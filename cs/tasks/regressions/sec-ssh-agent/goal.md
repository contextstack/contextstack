# Regression: lanes cannot reach the owner's ssh-agent

P1 finding #58: the executor strips `SSH_AUTH_SOCK` from the child env, but
launchd's per-user agent socket under `/private/tmp/com.apple.launchd.*` stayed
reachable. A lane could learn the path via `launchctl getenv SSH_AUTH_SOCK` and
sign with the owner's loaded keys.

Policy: deny launchd ssh-agent socket trees, `~/.ssh/agent*`, 1Password and
Secretive agent sockets, and the host's `SSH_AUTH_SOCK` path when building the
profile; deny exec of `/bin/launchctl` in agent profiles (no supported lane CLI
needs it — host `cs` / node-install use launchctl outside the sandbox).

`scripts/check` on Darwin runs this check with `--darwin-probe` under the
production executor profile; a decoy unix listener on the launchd path must get
`EPERM` on `connect()`, never the real ssh-agent.
