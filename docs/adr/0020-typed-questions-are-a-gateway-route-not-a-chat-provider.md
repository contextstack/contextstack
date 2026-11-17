# 0020. TypeSafe typed questions are their own gateway route, not a chat provider

- Status: Proposed
- Date: 2026-11-11

## Context

TypeSafe's System One models (Jev) answer typed questions (noul, choice,
score) about a state with calibrated probabilities. The API is one call,
`POST https://api.typesafe.ai/v1/systemone`, with a Bearer key. It is billed
per input token ($0.042 per million in November 2026); output tokens are free.
It should be a pay-as-you-go source the gateway can call, under the same
key handling and budgets as the other paid APIs.

It is not a chat model: there are no messages, no reply text and no tool calls.
Making `model.call` carry it would mean inventing a translation from messages
into `state` and `questions`, and then flattening answers back into text. Both
directions would lose meaning.

## Decision

- **A separate route, `POST /v1/ai/typed-question`.** The body is TypeSafe's
  own request, forwarded unchanged to the catalog endpoint `typesafe`. The
  reply is TypeSafe's status and body, also unchanged. Callers write TypeSafe
  requests exactly as its docs describe.
- **The gateway adds only what every keyed source gets.** That means the key
  from the credential store (`csgateway-rs credentials set typesafe`), egress
  policy, admission through the `api-typesafe` deployment, and the `typesafe`
  source budget from ADR 0018.
- **How a call is priced.** Admission uses an estimate: request bytes / 4 at
  the catalog rate. A 2xx answer settles at `usage.input_tokens` × rate, or at
  the estimate if the answer reports no usage. Other statuses count nothing.
  The cost is returned in `X-ContextStack-Cost-Usd`.
- **The catalog gets a `typesafe_systemone` protocol** that has no chat
  capabilities. If a `models.json` chat model names this provider,
  `model.call` refuses it with `policy_denied` and sends nothing.

## Consequences

- Any caller that can build a TypeSafe request can use it with no new
  schema. The route is a pass-through, so a TypeSafe API change needs no
  gateway change unless `usage` moves.
- Typed questions are not in `/v1/models`. The bound model ids live on the
  `api-typesafe` deployment.
- The spec suite covers the route with a loopback mock and fake keys
  (`tests/gateway-spec/cases/typesafe.py`).
