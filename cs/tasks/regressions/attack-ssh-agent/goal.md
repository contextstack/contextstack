# attack-ssh-agent

An agent signs with the owner's ssh-agent.

Host-only (`check --darwin-probe` on macOS). Otherwise it prints SKIP and exits 0.
If nested `sandbox-exec` is refused (`sandbox_apply: Operation not permitted`), it
prints SKIP and exits 0.

A decoy unix socket listens at a launchd-shaped path
(`/private/tmp/com.apple.launchd.<random>/Listeners`) when that directory can be
created safely, otherwise under a temp directory. That path is `SSH_AUTH_SOCK` in
the profile builder's host environment. Decoy sockets also sit at
`~/.ssh/agent.sock`-style paths under the temporary HOME.

Under every template's production profile the check tries `connect()` to the
launchd socket, `launchctl getenv SSH_AUTH_SOCK` (boolean: a path came back, the
path is not printed), and `connect()` to the home decoys. A successful connect
fails the route. Output is booleans and route names only.
