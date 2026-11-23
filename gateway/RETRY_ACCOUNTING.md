# Retry accounting audit

The gateway resolves one target and reserves once for a paid model call. It
retries at most twice against the same provider, snapshot, request ID, reservation
and deadline. Explicit provider-transient HTTP responses (408, 409, 425, 429
and 5xx) and connection establishment failures before dispatch are retryable
for every provider. Usage-shaped data in a refusal body is diagnostic.

For unary calls and stream opens, only a target with catalog cost class
`free_local` can replay a dispatched POST after a connection close or reset
before complete response headers. A transport failure after dispatch to a
paid target ends the exchange with `gateway.transport_failed`. The provider
may already have completed and billed the request, so replaying could
double-bill while the gateway settles only once. If a paid request body was
sent and a deadline, connection loss or downstream disconnect prevents
completion, the gateway settles once.
A positive provider-reported charge takes precedence. Otherwise the gateway
debits the reserved amount and records `basis: "reservation_estimate"` in the
trace. A failed connection before the body was sent releases its hold without
a charge. All retry waits share the request deadline.

After a 2xx stream opens, stream errors and disconnects never retry. The trace
records any available partial usage, including cache tokens. When the gateway
can return a terminal error frame, its cost carries the same settlement as the
trace. The gateway contract cases in `tests/gateway-spec/cases/upstream_retry.py`
exercise HTTP retries, post-send resets, deadlines and partial settlements.
