# director-rs-install

Demand: Director.Plan step 3 (vault `runtime-modernization/Director.Plan.md`):
the standalone Rust Director runs as a service on the Director host, and nodes
register with it. Until now the release tooling packaged only the Laravel
director (`--director`, `--serve-director`), which is parked as client-web.
The layout is node-install-layout's; this task adds the Rust Director to it.

In a fake HOME, offline, the check must show:

1. `node-bundle build --director-rs` builds `-p cs-director` and ships
   `bin/director-rs`, listed bundled and hashed; a bundle without the flag
   names no director-rs at all (a node never lists it as required); the flag
   for another target is refused before anything is written.
2. `--serve-director-rs` writes `com.contextstack.director-rs.plist`:
   `current/bin/director-rs serve --data-dir <root>/state/director --listen
   127.0.0.1:8780 --gateway-owner-token <root>/config/gateway/owner-token`,
   KeepAlive, an explicit PATH, absolute paths only, logs under `logs/`.
   `--director-rs-listen` (repeatable) takes loopback or Tailscale addresses
   only (it serves plain HTTP); others are refused before any write.
3. The data dir is created 0700 when missing; one open to other users is
   refused and never re-moded. With the real binary built, the plist's own
   command answers 401 without a credential and keeps director.sqlite and
   local-credential 0600 in that dir (else SKIP, and a stand-in database).
4. Install, rollback and switch keep the data dir byte for byte (state, not
   the release); the recorded listen addresses follow every switch; a switch
   to a release without `bin/director-rs` is refused while it is served.
5. With `--serve-director` in the same command, or recorded, it is refused:
   one director per host, and no Laravel plist is written.
6. The Director host may be a node of its own Director (this Mac): the node
   plist and the director-rs plist side by side.
7. Registration, node side: `--director-token-from FILE` takes the JSON line
   `director-rs node register NAME` printed (or a bare token; `-` is stdin):
   the token goes to `config/node.env` 0600 as `export
   CONTEXTSTACK_NODE_TOKEN=...` (other lines kept), the installation into
   spool.json's receiver; never printed, never in install.json. A token for
   another node, a differing `--installation`, a malformed one, or a host
   that is not a node is refused.
8. `cs status` shows the Director's URL, whether it answers, its
   installation ID (read-only; no `-wal`/`-shm` written) and its data dir
   modes; `cs doctor` fails a Director that does not answer and a data dir
   file open to others, each with its fix; `cs start|restart|stop director`
   go through launchctl. No credential is ever printed.
9. `uninstall --purge` removes the plist and the rest of state/, never
   state/director.

- Never the real launchd: node-install-layout's launchctl shim on PATH,
  `--no-load` except one shimmed load, and the check fails if the real gui
  domain gains or restarts a com.contextstack.* job (f999e170c).

Run: `tasks/regressions/director-rs-install/check`, about 30 s. Needs uv and
the pinned uv-managed interpreter, else SKIP and why.
