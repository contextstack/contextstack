# client-web-install

Demand: ADR 0017: client-web holds pages only (no database, no
login) and talks only to the Rust Director's Client API. 3ef9c93e1 retired
the Laravel director's serving mode, which had been repointed at
client-web, so client-web had no packaged install path. This task adds one
to node-install-layout's layout.

In a fake HOME, offline, the check must show:

1. `node-bundle build --client-web` ships `release/client-web/` from the
   app's source folder (one constant, `CLIENT_WEB_SRC`): committed files,
   `vendor/`, `public/build/`, `bootstrap/cache/{packages,services}.php`
   and `.env.install.example`; never `.env`, `tests/`, `storage/`,
   `node_modules/` or agent files. A build missing an output stops.
2. `--serve-client-web HOST:PORT` writes one agent,
   `com.contextstack.client-web.plist`: `php -S HOST:PORT -t
   current/client-web/public` with Laravel's router,
   `PHP_CLI_SERVER_WORKERS=16`, KeepAlive, an explicit PATH, absolute paths,
   logs under `logs/`. No schedule or queue agent, no database.
3. HOST must be loopback or Tailscale (no login: whoever reaches it acts
   with its Director credential); anything else is refused before any
   write. So is a release without `client-web/`, a php older than 8.3 or
   none, and an env file or storage open to other users.
4. `config/client-web/client-web.env` is seeded once, 0600 in a 0700 dir,
   with a fresh APP_KEY that is never printed; DIRECTOR_URL is the served
   Rust Director, the local credential is `state/director/local-credential`,
   storage is `state/client-web`, sessions and cache are files, the queue
   is sync.
5. With php and the checkout's built `vendor/` present, the plist's own
   command (port changed) serves `/intents` with 200 from a stand-in
   Director that requires the local credential, and writes its session
   under `state/client-web`, never in the release. Else SKIP.
6. Install, rollback and switch keep `state/client-web` and the env file
   byte for byte; a switch to a release without client-web is refused
   while it is served; `uninstall --purge` keeps `state/client-web`.
7. `cs status` and `cs doctor` show its URL, answer, Director, env file and
   storage modes; `cs start|stop|restart|logs client-web` work through
   launchctl. No APP_KEY or credential is ever printed.

- Never the real launchd: node-install-layout's launchctl shim on PATH,
  `--no-load` except one shimmed load, and the check fails if the real gui
  domain gains or restarts a com.contextstack.* job (f999e170c).

Run: `tasks/regressions/client-web-install/check`, about 30 s. Needs uv and
the pinned uv-managed interpreter, else SKIP and why.
