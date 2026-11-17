# Retry accounting audit

The suspected double billing was not reproduced. The regression cases in
`tests/gateway-spec/cases/upstream_retry.py` exercise the actual gateway with
an isolated stub upstream and check both `USAGE` records and persisted spend.

`server/model_call.rs::handle` resolves one target and takes one spend reservation.
`invoke_with_retry` and `stream_with_retry` retry provider-transient HTTP errors
(408, 409, 425, 429 and 5xx) and network errors, at most twice, using the same snapshot, provider,
request id, reservation and deadline. `Gateway::invoke` / `Gateway::stream` select
one adapter; neither switches providers. An unknown model or unavailable provider
refuses rather than falling back (`WIRE.md`, routing).

The OpenAI and Anthropic adapters return classified errors for non-2xx responses.
Their per-attempt `provider_http_trace` rows are diagnostics, not `USAGE` settlement.
Usage-shaped fields in an HTTP error body are not interpreted as billable usage.
Only the final unary reply reaches `ok`, which logs one `USAGE` row and consumes
its reservation once. Failed attempts never enter that path.

Streaming retries cover opening the HTTP response only. Once a 2xx stream opens,
`StreamState::frame` consumes it without retrying, even if it fails before the
first delta. A stop logs and settles once, taking the reservation. On a fault or
client disconnect after output, the existing documented partial-settlement policy
logs `partial=1` and consumes the reservation, so drop cleanup cannot charge again.
The regression sends usage and a provider charge before disconnecting and checks
one labeled partial record, its reported token counts, and one charge. It does
not retry or merge that billable attempt with a successful attempt.

Timeouts differ from transient HTTP errors: `protocol::until` enforces the shared
request deadline and returns a local transport fault, which is not retryable.
There is no shorter per-attempt response timeout to fail once then succeed.
The timeout regression queues a success after a delayed response and verifies
that it is never reached, with no usage record or spend. A network connect timeout
is classified as retryable, but must still fit the same request deadline.
After-2xx unary body read/decode errors are also local faults and are not retried.
