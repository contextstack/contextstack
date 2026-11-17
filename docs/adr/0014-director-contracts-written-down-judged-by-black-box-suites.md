# 0014. The Director's contracts are written down and judged by black-box suites

- Status: Accepted
- Date: 2026-11-10

## Context

ADR 0012 made the Director a standalone service with nodes on one side and
clients on the other. It said the node contract would be written first, with
the Laravel node endpoints as its reference. The gateway had already shown
what works: a written contract (WIRE.md) and a suite that drives any
implementation from outside (`tests/gateway-spec/`, ADR 0010). Nodes, the
Director and clients are installed and upgraded separately. A mismatch has to
be refused cleanly, not guessed at.

## Decision

- `tests/director-spec/` holds both contracts and their suites. `NODE-PROTOCOL.md`
  covers Director and node, and `CLIENT-API.md` covers Director and client.
  `tests/director-spec/run` checks the Director over HTTP and the node client
  against a stand-in Director.
- **Node protocol.** Every request names its protocol version
  (`ContextStack-Protocol`) and the Director's installation ID
  (`ContextStack-Installation`). A version the Director does not speak gets
  `426`, with the versions it does speak. A wrong installation gets `421`.
  The Director answers in the version asked for, and the node checks it.
- **The node protocol grows by raising the version.** A new call, a new
  required field or a changed rule is version N+1. Data a reader can safely
  ignore needs no new version. Version 1 is the original five endpoints plus
  version, installation and usage. Version 2 adds control messages, held-open
  work, live run events, status and schedules, and changes none of version 1.
- **The Client API is versioned additively.** New routes, fields and record
  kinds that change no existing behaviour stay in version 1. A change that
  breaks a client is published as version 2 under `/api/v2/client/`.
- A contract change lands with its cases in the same commit.
- **The Rust Director (`director-rs/`) is the reference.** The Laravel target
  is dropped from the suite, and Laravel's node API is removed (Director.Plan
  step 8). Nodes talk only to `director-rs`.

## Consequences

- This replaces ADR 0012's consequence that "the Laravel node endpoints are
  its reference". 0012's decision stands. Protocol 1 was first described from
  the Laravel code, then held by `director-rs` passing the same cases.
- Only `director-rs` speaks protocol 2. The node client is still protocol 1
  until lane node-protocol2 lands.
- `tests/gateway-spec/` follows the same pattern for the gateway and now runs
  in the check against the Rust gateway (`92496e69a`).
- History: `80a7a8e81` protocol 1, `157c7c755` protocol 2, `176af12c0`
  Client API version 1, `ce9b8344a` Rust as reference, `a6b3a4529` Laravel
  node API removed.
