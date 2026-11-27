---
host_only: Exercises live Unix listeners, process cleanup, and the platform sandbox.
---
# browser-broker

The `browser` profile cannot
browse logged in; logged-in browsing goes through a dedicated Chrome the host runs
(`ops/bin/browser-broker`), reached by `tools/bin/browser_session` over one unix socket per origin
set that `cs-sandbox` grants by name (`"sockets"`). Offline: `fake-chrome` answers CDP on the
debugging pipe (fds 3/4) as Chrome does, and logs argv and every method.

The check must show:

1. Broker contract. Chrome is started with `--remote-debugging-pipe`, `--user-data-dir` the root's
   `agent-profile`, headless, with `--use-mock-keychain --password-store=basic` (recorded as `mock` in
   `keychain-mode`), never `--no-sandbox` or a debugging port. Each set's socket is 0600, the
   socket dir and profile 0700. `open` returns an allowlisted page's text; an in-set redirect is
   followed with both hops recorded; another set's origin or http is refused before any tab opens; a
   redirect hop off the set is failed in Chrome (`Fetch.failRequest`, `BlockedByClient`) and refused; a
   final URL off the set returns no text; a window the page opened is closed with the tab; any other
   verb, an oversized or malformed request is refused. Every request is one log row, blocked hops named.
2. Lifecycle. A live socket refuses a second broker before Chrome starts; SIGTERM sends
   `Browser.close` and removes the sockets; a stale socket is replaced; a non-socket path, a
   world-writable socket dir and a root outside `~/.contextstack` refuse startup. A `mock` profile
   refuses a `--keychain` start; a used profile with no `keychain-mode` record refuses; `--keychain` on
   a fresh profile drops the mock flags and records `keychain` (the owner 2026-11-09: opt-in, not default).
   Where the host blocks Chrome's own sandbox (fake: `FAKE_CHROME_NO_SANDBOX`, as Ubuntu's AppArmor
   userns restriction does), the broker refuses to start, naming `--allow-no-chrome-sandbox`, and leaves
   no Chrome or socket; with that flag it retries once with `--no-sandbox`, warns, and reports
   `chrome_sandbox: false`; on a host whose sandbox works the flag changes nothing.
3. Confinement, through the release `cs-sandbox` (skipped, said, when unbuilt). Wrapped
   `browser_session` reaches its set's socket and writes evidence. A set is a symlink `browser_<set>`
   to it with its own descriptor: one asking for `browser-other` reaches that set; the same name with a
   descriptor asking for `browser-public` cannot. A probe requesting `browser-public` (`net: none`) sees only
   `CS_SOCKET_BROWSER_PUBLIC`, talks to it, and cannot reach the other set's socket, the gateway's
   socket path, a live 127.0.0.1 port or example.com:443; without the request the broker's socket is
   unreachable; an absent and an unlisted socket are narrowed and reported.

`browser_session` is a manifest tool (operator root, the owner 2026-11-09), so `../tool-fixtures` runs its
contract fixtures.

4. Cleanup. Each broker runs as the leader of its own process group; on every exit path (normal end,
   SIGINT, SIGTERM, SIGHUP) the check stops each group (SIGTERM, then SIGKILL) and requires that no
   process naming its temp root survives. It runs itself with `--hold` and interrupts that run with
   SIGINT and with SIGTERM: no broker, no Chrome and no temp root may remain (the 2026-11-09 19:30
   orphan: the check was killed mid-run and the broker and fake Chrome outlived it).
