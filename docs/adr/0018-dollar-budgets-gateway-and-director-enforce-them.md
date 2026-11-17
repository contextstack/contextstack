# 0018. Spending is opt-in by dollar budget, enforced by the gateway and the Director

- Status: Accepted
- Date: 2026-11-10
- See also: the 50-calls-a-day cap was removed 2026-11-11 (Director.Plan step 4; `cs status` now shows today's CLI runs as a count)

## Context

Before this, the lab's only spending limits were a cap of 50 coding-agent
calls a day in the dispatcher and one daily paid-API total in the gateway.
An amount named on a request could raise that total. The rule is that nothing should spend tokens unknowingly, without artificial
limits on top of that. So usage must be visible, and money is spent only
where a person has set a budget. Several jobs can run at once, so budgets
need to hold under concurrency and across restarts.

## Decision

- **Pay-as-you-go sources are off until they have a dollar budget.** Setting
  a budget is consent to spend. Subscriptions and local models have no dollar
  budget; they show usage and, where known, the remaining allowance.
- **The gateway enforces per-source budgets.** Each budget is a day or month
  amount per pay-as-you-go source, with `*` as a shared one. "Off",
  "exhausted" and "ceiling" refusals are told apart.
- **Only the owner token can set source budgets.** That is a 0600 file at
  `~/.contextstack/config/gateway/owner-token`, sent from a local peer.
  The shared bearer token can't set them, and neither can the tokenless
  socket. Sandboxed tools can reach the socket, so a socket-only rule would
  let a tool set a budget and then spend it.
- **A job's amount only lowers a call's ceiling.** The runner sends the job's
  remaining dollars as `budget.max_usd` on every call. A ceiling refusal ends
  the run as `budget:usd_ceiling`.
- **The Director reserves before dispatch and settles on actual charges.**
  Project and conversation budgets are optional, on top of source budgets.
  If nothing is left, a job waits with a reason. Waiting jobs are offered,
  oldest first, as settling frees room. Reservations survive a restart.
- **Release rule.** A reservation whose report never comes closes on the
  first of these: the job's end, a cancel outcome, a node silent past 3600 s,
  or 86400 s. It counts in full, so nothing is freed on a guess. A late report
  recomputes the charge and releases the difference once.
- **Periods are local calendar days and months** on the host, the same for
  gateway and Director. Times on the wire stay UTC.
- **API-price estimates are never shown or counted as charges.** For example,
  Claude Code's `total_cost_usd` on a subscription run is recorded as
  `est_basis: api_list`.

## Consequences

- This carries out ADR 0012's consequence on usage and budgets.
- The dispatcher's 50-calls-a-day cap (`cli_calls_per_day` in `tick`) is
  **not yet removed** as of `04d9e5184`. It goes once usage is visible in a
  client.
- The runner's own stop on estimated cost stays as a safety net, not as the
  budget.
- The gateway has no revision number for budgets, so the Director's
  read-modify-write can overwrite an outside writer that lands in between.
- History: gateway `fa45f4947`, `6806c816d`, `7e309666f`; Director
  `d92860881`, `2fa8ea675`, `cf6b9017b`, `55bb918ab`; runner `e7a3df9e2`.
  Earlier interim caps: `bc7f83eeb`, `0d40e6d87`.
