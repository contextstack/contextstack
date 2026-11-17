# 0017. client-web holds pages only; `operator/` is retired

- Status: Accepted
- Date: 2026-11-11

## Context

ADR 0012 moved the Laravel app to `client-web` and said it would keep only
pages and rendering. Step 8 of the Director plan removed everything else
from it. `operator/` was the first web surface (arc A1). It was a second way
to start and inspect work beside the Director, and the Laravel director had
called into nodes through it. With two ways in, the Director isn't the single
record of what was asked and what ran.

## Decision

- **client-web holds pages only.** No database, no login, no shared state.
  Everything comes from the Director's Client API. Its settings are its
  connection to the Director: URL, pairing, the local credential and this
  device's identity. Sessions and cache are local files, nothing more.
- **client-web listens only on loopback or Tailscale.** Anyone who reaches it
  acts with its Director credential, and it has no login of its own to stop
  them.
- **`operator/` is retired.** There is no second way in beside the Director.
  The lab is fed through client-web intents. What existed only for the
  Laravel director's calls into nodes was removed first.

## Consequences

- Bots, teams, login, the old control path and the Laravel node API left
  client-web. Its tests went from 644 to 97, and its end-to-end journeys run
  with no database configured at all.
- Accounts and teams, when they come, belong to the Director (ADR 0012), not
  to a client.
- Not yet done: no code restricts client-web's listen address. Only the Rust
  Director's is checked (`node-release --director-rs-listen`, ADR 0016).
  Removing `operator/` itself is in progress (lane retire-operator), so
  `AGENTS.md` and `loop-watcher/` still mention it.
- History: `a6b3a4529` client-web retires the Laravel director, `637a5c244`
  connection settings, `bf9ac91db` no database, `4077ad245` operator's
  director-only paths removed, ROUTE `6e93a7433` (operator retired).
