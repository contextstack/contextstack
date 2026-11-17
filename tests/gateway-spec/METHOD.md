# Gateway contract method and decisions

## Method

Black box only (§8): assertions are on HTTP status, headers, bodies, SSE frames, bytes sent upstream, child argv/stdin/environment, exit codes, files and log rows. Nothing asserts internal structure. Normalised: `latency_ms`, timestamps (compared by shape or relation), uptime, ephemeral ports and temp paths. Never normalised: the resolved model, cost amount/class/est_usd/charged_usd, error class and code, missing terminal events, upstream request counts. Finite fixtures show tested conformance, not equivalence.

**Dispositions.** There is no `--update`. A failing case is either fixed because the fixture was wrong, or recorded in `dispositions.json` for that implementation as `go_bug`, `rs_bug` or `contract_change`. The case keeps stating the intended contract either way. A passing case that still carries a disposition fails the run, so stale dispositions cannot linger. A deliberate contract change (for example G5's provider evolution) lands as a case edit in the same change that alters behaviour, applied to both implementations.

## Not covered (and why)

- Work-order `timeout_s` expiry: the gateway allows `3×timeout_s + 120` s, so even 1 s is 123 s. Cancellation by disconnect covers the kill path.
- Output limits on work orders and model replies: Go has none to pin (unbounded buffers). This is a gap against §8 "output limits", raised below.
- The director sync of the tenant policy, policy signatures, the persisted rollback high-water mark, and in-flight cancellation on revocation: not built in phase 5.
- Override endpoints (`Selection.OverrideEndpoint`): the policy guard wraps them, but `model.call` never sets one, so they are not reachable at the process boundary.
- Live providers, paid calls and real CLIs: never (§8 replay before live).

## Open contract questions (not Go bugs; for the owner)

1. ~~**Streamed tool calls are dropped.**~~ Settled: the stop chunk carries `tool_calls`, `provider` and `model`, and a folded stream equals the unary reply (`anthropic-stream-tool-use`, `anthropic-stream-equivalence`, the `stream-*-equivalence-*` cases).
2. **Error chunk index restarts at 0** after deltas (`anthropic-stream-eof-without-stop`).
3. **WIRE says malformed JSON "on the streaming path" reports under the v2 chunk family.** The gateway cannot know a body is a stream request until it decodes, so it answers a JSON envelope (`stream-schema-mismatch-is-envelope`). The WIRE wording should change, not the code.
4. **Non-object tool arguments** (`"[1]"`) are returned to the caller as-is. A runner echoing them back is then refused by the history validator on the next turn.
5. **Work-order output is unbounded.** §8 lists output limits.
6. **`providers_active` includes `anthropic`** because of the `claude-code` CLI entry, not an API credential.
7. **A removed endpoint's model refuses as `retryable_gateway` / `gateway.capacity_exhausted`** ("no provider registered"). It is $0 with no request and no fallthrough, which is the contract, but it tells the caller to retry a call that cannot succeed until the catalog changes. `fatal_session` (for example `gateway.unknown_model`) fits better. `overlay-removes-endpoint` and `revision-bound-per-call` assert only a classed $0 error.
8. **A keyless overlay endpoint's models are cost class `api`** at $0 (no `cost_per_1k`). A loopback overlay endpoint such as `vllm-spark2` is free_local in spirit. Not pinned.

## Decided: tool arguments that are empty or not JSON (2026-11-10)

`"arguments": ""` or whitespace means `{}`, the same as absent. Arguments that are not JSON fail the whole call as `runtime_fault` / `gateway.provider_failed`, $0, with no partial `tool_calls`, even beside a valid call. No WIRE change: the code already exists, and clause 5 already forbids a partial result. The gateway also encodes the whole reply before writing it, so an unencodable reply becomes that same classed error rather than an empty 200. Non-object JSON (`"[1]"`, `null`) is unchanged (question 4). Codex, asked for a second opinion, recommended exactly this over carrying the raw string (reports unusable output as success) or a new error code (no caller needs it). Recorded in the vault's `Overnight.2026-11-10.md` (G1 3).

## Phase 1 exit records (decided overnight with a Codex second opinion; the owner may revise)

**Primary benefit metric:** total agent-hours for **three predeclared, matched lifecycle changes**, one each for streaming, cancellation and process supervision. Each change is made in both Go and Rust, and both must pass the same acceptance cases and fault injection. Hours include implementation, debugging, review and verification. Both languages use the same agent and model configuration, and the order alternates per change. **Target: Rust ≤ 80% of Go's total.** This is directional evidence from n = 3, not a language-wide result. It extends phase 3's single follow-up change to three.

**Guardrail (not a benefit):** lifecycle-invariant escapes per 1,000 fault-injected runs of the suite's admission, cancellation, stream and work-order cases. Fault classes, seeds, concurrency and cleanup deadlines are frozen before any run. An escape is any of:
- a permit not idle within 2 s;
- a process from the child tree alive 5 s after the reply;
- a reply that is neither ok nor a classed $0 error;
- a stream with neither a stop nor an error terminal.

Target: Rust 0 per 1,000, and no more than Go. A zero–zero tie shows no comparative benefit. The empty-200 Go bug is a serialization defect, not lifecycle evidence. Further guardrails: full-suite parity, gateway-added p95 latency against mocks no more than Go's + 1 ms, RSS no more than 2× Go.

**Effort:** phase 1 took one agent session, about 1 h wall-clock, about 2,970 lines of Python and JSON, and no Go changes.

**Budget for phases 2–4:**
- **Hours:** 12 agent-hours planned, review at 18. This counts all work: Rust, Go parity changes, the injector, the comparison and checks, with concurrent agents' hours summed.
- **Lines:** 4,500 formatted Rust non-test lines across the first-party crates, as a forecast rather than a completion gate, with review at 6,750.

Continuing is pre-authorised. An unmet target is reported plainly; continuation itself is not evidence of success.

## Check wiring

`cs/scripts/check` runs the fast subset against the Rust gateway it builds (`target/release/cs-gateway`, `--fast --impl rust`, about 20 s) as a background rule (G1 2). With no cargo on PATH it is skipped and says so. The six slow cases (stale discovery, catalog poll and cache expiry, policy expiry, credentials resolved once, a surviving work-order descendant) wait on real timers and stay in the full run.
