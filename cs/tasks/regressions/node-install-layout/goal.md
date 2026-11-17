# node-install-layout

Phase 2's
`node-install BUNDLE PREFIX` put an interpreter and a node tree in a scratch
prefix; phase 6 built a node by hand under `~/.contextstack/phase6-node/<ts>`
and removed it. A host that stays a node needs a persistent install
(the owner, 2026-11-10: the layout in `scripts/node-release`'s docstring).

In a fake HOME, offline, the check must show:

1. `node-install install BUNDLE --node N --director URL` lays out
   `~/.contextstack/{releases/<id>,current,config,state,workspaces,logs}`
   (0700), `current -> releases/<id>`, every binary in the release's `bin/`,
   `target/release/cs-tools` the same file, no `bin/lab`, `bin/cs`, `bin/cs-go`
   or `bin/csgateway` (Go is archived, the owner 2026-11-10), no `ledger/`, `specs/`, `tasks/`
   in the code root, `config/ESTATE` and a director receiver.
2. PATH links `csgateway`, `cs` (the CLI), `node-install` in
   `~/.local/bin` (on the login PATH) point through `current`, and there is no
   `lab` link. An upgrade removes our old `lab` link and reports it; a foreign
   `lab` (GitLab's CLI) is never refused or removed. With it off the PATH the link dir is
   `~/.local/bin`, created if missing; `--link-dir` wins. A TTY asks before adding a marked
   shell PATH line; --yes/non-TTY print it. Reinstalls never duplicate it; uninstall removes it.
3. Both plists name absolute paths into the install, set an explicit PATH,
   never name a PATH link; the node's PATH starts with the release's pinned
   interpreter, and it sets the config, state and workspaces roots and
   `CS_RUNNER_BIN` (`bin/cs-harness` by default). The gateway is
   `cs-gateway` . `select --runner go` or `--gateway go`
   on a release without the Go binary is refused, naming the way back: a
   switch to an earlier release that has it, where the select works and the
   runner environment and `csgateway` link follow; selections survive switches. A node's
   gateway plist sets `CSGATEWAY_CREDENTIALS=off` (prod-1 2026-11-10: an
   OPENROUTER item untrusted for /usr/bin/security prompted at every
   discovery pass and every crash-loop restart); `--credentials file|keychain`
   changes it, recorded in `config/install.json`. A director host defaults to
   `file` (the owner 2026-11-10: ContextStack drops the Keychain):
   `CSGATEWAY_CREDENTIALS=~/.contextstack/credentials`, the dir created 0700 when
   missing, an existing one untouched. `keychain` is an explicit opt-in, written
   as `keychain`; no choice relies on the gateways' unset default.
   The gateway plist's own command, with the real `cs-gateway`
   and a DGX-shaped overlay, answers `/v1/health` 200 (first fall, prod-1
   2026-11-10: the shipped `models.json` named the dev checkout's
   `lab_executor` and the gateway exited; activation now derives
   `~/.contextstack/gateway/models.json` pointing it into `current`).
4. Installing the same bundle again changes nothing.
5. The plist's own command, with its environment and the director down, is
   a harmless tick: exit 0, one JSON line naming the `job_pull` error, empty
   stderr, nothing written into the code tree, its shelf under `state/`.
6. A second release switches `current`, keeps the first; `rollback` and
   `switch` move it back and forth; an unknown release is refused.
7. A foreign file where a link goes refuses the install, untouched, with no
   link, plist, config or `current` written; `--skip-link` installs the
   rest. A different `--node` is refused. No director: no node plist.
8. `uninstall` removes plists, links, `current` and `releases/`, keeps
   `config/` and `state/`; `--purge` removes those too, never a foreign file.
   The real `~/.local/bin` and `~/Library/LaunchAgents` are never touched.
9. A director host (`--estate workstation`, no receiver, no node plist) whose
   `~/.contextstack` already holds `gateway/` (with `providers.d`), `lab/setups`,
   `browser/`, `director/`, `credentials/`, `estate-deploy/` and a state root
   with a ledger: a gateway plist that runs from the checkout is refused and
   nothing outside `releases/` changes; `--adopt` keeps the original in
   `config/replaced-agents/` and writes ours; every existing file keeps its
   bytes, mtime and mode; `providers.d` is copied into `config/gateway`.

10. The host's uv-managed interpreter keeps every file's mtime: the seeded
    release interpreters are independent copies (`cp -c` clones on macOS).
    Hard links let `uv python install`'s in-place rewrite of `BUILD`,
    `EXTERNALLY-MANAGED` and `_sysconfigdata` reach the host and every
    parallel check's release (red only inside scripts/check, 2026-11-10).

11. A restarting `switch`/`rollback`/`install` drains active jobs first (`--wait SECONDS`; `--no-wait` exits 2
    with a JSON reason; `--force`); on start, orphans end as error "node restarted while the job ran".

- Never the real launchd: a launchctl shim is first on PATH; the check fails if the real gui domain gains or restarts a com.contextstack.* job. --purge refuses while director.env exists unless --purge-secrets.

12. `bundles` and `state/runs` are relative symlinks to `bundles.noindex` and `runs.noindex` (Spotlight skips that suffix). An existing real directory is renamed on install and stays readable through the symlink. Only the newest `keep` bundles remain (default 3): never the one just installed, never one a kept release came from, never a non-bundle, never via a symlink.

Fails on the phase 2 `node-install` (no `install` command). Needs uv and the pinned uv-managed interpreter
(seeded into each release); else SKIP and why. Run: `tasks/regressions/node-install-layout/check`, ~20 s.
