# Can a coding agent read secrets in the user's home?

ContextStack runs coding agents inside a macOS Seatbelt profile the executor generates. `sandbox_cmd` in `ops/bin/_executor.py` allows by default, then denies a list of paths. `muse-code` and the `cli-profiles.json` templates leave before that call and run under `_muse.sandbox_command`, which denies reading the whole home. Any secret the denylist does not name stays readable to every other template.

## Check

`check --darwin-probe` on macOS:

1. Plants a temporary HOME (never the real one) with random markers in `~/.zsh_history`, `~/.bash_history`, `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`, `~/.docker/config.json`, `~/.kube/config`, `~/.aws/credentials`, `~/.ssh/id_ed25519`, `~/.config/gh/hosts.yml`, Chrome and Firefox cookie stores, `~/.contextstack/director-db-backup/x.sqlite`, and `~/.acme-cli/session.json`.
2. Builds every `executor-templates.json` profile the way that template runs in production, with HOME pointed at the temporary HOME.
3. Runs `sandbox-exec -f <profile> python3 probe.py`. For each decoy the probe tries a direct open, a symlink it creates in its workspace, `os.listdir` of the parent, `/bin/cp` into the workspace, and a read through `/dev/fd` after opening the parent. It prints booleans only.
4. Fails, naming template and path, when a route returns the marker, or when `listdir` shows the decoy's name. It passes only when every route is blocked for every decoy.

Without `--darwin-probe`, off macOS, or when nested `sandbox-exec` fails with `sandbox_apply: Operation not permitted`, the check prints SKIP and exits 0. It is not wired into `scripts/check`.

A green run means every route was blocked. Dropping a decoy or a route to turn today's profile green hides the leak. The denylist misses several of these paths, so this check is expected to fail until those reads are closed.
