# 0022 — Embed a loopback web UI in the Director with one-time browser pairing

Status: Accepted
Date: 2026-11-11

## Context

A newcomer installing the Director needs status and conversations without PHP, Laravel, a frontend build, or an external asset service. client-web remains the richer client. The owner credential must never enter a URL, page, script, browser storage, or browser response.

## Decision

Embed HTML, CSS and vanilla JavaScript with `include_str!` and serve them at `/ui/`. Reuse the existing Client API handlers through a narrow authenticated adapter: list nodes, jobs and conversations, read a conversation thread, and post a turn. The adapter resolves the current local owner's identity inside the Director; it never reads or sends the owner's bearer token. Existing API bearer authentication remains unchanged.

`director-rs ui-link --data-dir DIR [--listen 127.0.0.1:8780]` prints a five-minute URL containing a random 256-bit code in its fragment. `--listen` also honors `DIRECTOR_LISTEN`; the default is the normal plain-HTTP loopback listener. A link targets that address and data directory; it does not contact or start a Director. The browser clears the fragment from history before exchanging the code by JSON POST. Fragments do not reach request logs or referrers. The SQLite exchange atomically deletes the code and creates an eight-hour session, persisting only hashes. This is separate from device pairing, which intentionally returns a reusable API bearer token and is unsuitable for browser login.

The session is a host-only `HttpOnly; SameSite=Strict; Path=/ui` cookie with an eight-hour maximum age. The cookie also has `Secure` when served over TLS. Plain HTTP is deliberate for the default local install, so `Secure` cannot be required there. Session cookies do not authenticate requests to `/api/v1/client/`. Sessions expire after eight hours across restarts; generating a new link does not revoke existing sessions. The first slice has no session-management screen.

The UI is accepted only on explicitly loopback-bound listeners and from loopback peers, including when the Director has other API listeners. Host must exactly name the actual numeric listener address and port, preventing DNS rebinding. Browser Origin must match that host and scheme; cross-site and same-site fetches are refused. The UI does not inherit the API's configurable origin allowlist. The default link is for HTTP; a separately configured TLS loopback listener requires opening the same link using HTTPS.

Every UI response, including errors, carries a CSP with `default-src 'none'`, self-only script/style/connect sources, no framing, no base URL, and self-only form actions. Responses are uncached, disable sniffing, and set a no-referrer policy. Scripts use text nodes for API content and no inline handlers. Every authenticated write additionally requires a session-bound `X-CSRF-Token`; the token is derived with domain separation from the random session secret. Turn writes retain the existing Client API idempotency contract. Pairing itself requires the one-time secret, JSON content type, and the same origin checks.

## Consequences

No new dependencies, web build step, credential proxy over HTTP, or second conversation implementation is required. The UI is intentionally local: it is unavailable through wildcard, Tailscale, or remote TLS listeners. A user must open a fresh CLI link after session expiry. A valid UI session grants the owner's small allowlisted read/post surface for its lifetime; other owner operations remain outside this adapter. Unit tests and director-spec exercise pairing, expiry, replay, headers, origin checks, CSRF, reads and turn posting using temporary state only.
