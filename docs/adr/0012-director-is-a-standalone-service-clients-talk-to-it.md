# 0012. The Director is a standalone Rust service; everything a person uses is a client of it

- Status: Accepted
- Date: 2026-11-10
- Supersedes: 0009
- See also: 0014 (the Rust Director, not Laravel, is the node contract's reference), 0017, 0018

## Context

ADR 0009 made the director the holder of the estate, with nodes pulling work
and pushing verified envelopes. The director was the Laravel app in
`director/`: service, storage and web pages in one program, which also called
into nodes over an older operator path. The target is one coordination
service that a web page, a phone, a terminal client or a desktop app can all
reach, on whatever machine suits. The contract is in the vault,
`runtime-modernization/Director.Contract.Outline.v1.5.md`.

## Decision

- The Director is its own Rust service and takes the folder name `director/`.
  It owns the shared work record, the node protocol and coordination. It
  starts with an empty database; nothing is migrated from the Laravel app.
- Clients are named `client-*`. The existing Laravel app moves to
  `clients/web/` and keeps only pages and rendering, reaching everything else
  through the Director's client API. It must work against a Director on
  another machine.
- What carries over from ADR 0009: nodes open the connection, pull jobs and
  push envelopes; the Director never calls a node. The older operator path is
  retired with no fallback.
- Work is named in three layers: an Intent is what was asked, a Job is one
  assignment to a node, a Run is one execution attempt.
- Nodes, runners and gateways run ordinary jobs and hold no bot-specific
  behaviour. Conversations and, later, bots are Director features built from
  jobs.
- A basic install has no accounts or login. Local clients use a credential
  created at install; other devices pair once with an owner-approved code.
  Accounts and teams arrive later and belong to the Director.
- Local administration (`cs` start, stop, restart, diagnose) works with the
  Director down or absent.

## Consequences

- A client API has to be designed and built; today people reach the same
  logic through Livewire pages calling PHP directly.
- Bot turns, cancellation and the Today and fleet views that used the operator
  path need replacements over node pull, or are deferred and said to be.
- The node contract is written down first, with a suite that tests both the
  Director and the node client; the Laravel node endpoints are its reference.
- The lab's bot-turn operations leave the node installation.
- Usage is reported per run and budgets are enforced by the Director and the
  gateway, replacing the dispatcher's call-count cap.
- Recorded in `ROUTE.md` at `4153b953d`. Order of work: vault
  `runtime-modernization/Director.Plan.md`.
