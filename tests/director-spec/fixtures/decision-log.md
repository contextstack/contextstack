| # | Decision | Why | Reopen when *(proposed unless sourced)* | Source |
|---|---|---|---|---|
| D1 | Develop in the private monorepo; publish `contextstack/contextstack` by fresh export | Lab history, governance and personal material stay private without scrubbing history; the same pattern as Google's Copybara and Meta's ShipIt | Outside contributors send PRs regularly, or the monorepo-only material is gone | memory 2026-11-15; ADR 0004 |
| D2 | Teams and orgs are the default owner of every record | Bolting teams on later means migrating every record's owner | Never for the default; the timing is open | 2026-11-17 |
| D3 | The Director is the single source of record; clients hold only presentation | A mobile app and a TUI are coming; private client state drifts | A client needs to work offline | ADR 0012; 2026-11-15 |
| D5 | Installed release is production; the checkout is development | Editing code changed live services; tests touched live state | — | ADR 0016 |
| D12 | No backward compatibility in renames | Sole user; shims accumulate | ContextStack has outside users (0.1.x is public, so this is due now) | memory 2026-11-15 |
