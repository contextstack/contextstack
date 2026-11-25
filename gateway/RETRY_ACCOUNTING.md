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
A positive provider-reported charge takes precedence. With known usage, the
gateway records `basis: "measured"` and charges at least the measured price
and the reservation. When usage is unknown, it charges the reservation and
records `basis: "reservation_estimate"`. A failed connection before the body
was sent releases its hold without a charge. All retry waits share the request
deadline.

Once the provider answered 2xx the call is billed even if it cannot complete:
a unary body that does not parse, exceeds 64 MiB or reports a negative charge
settles at the higher of the measured price and the reservation when usage is
known, or at the reservation when usage is unknown. An HTTP refusal (300 and up) still
releases at $0. Typed questions settle at the reservation after a timeout,
transport failure or disconnect once the body was sent.

After a 2xx stream opens, stream errors and disconnects never retry. The trace
records any available partial usage, including cache tokens. A stream that
fails after 2xx (with or without text, including tool-call-only turns, bad
tool JSON and a close before the final event) settles at its measured usage,
uncapped by the reservation; when usage is unknown or the stream was aborted,
at no less than the reservation. Settlement writes spend synchronously, so
calls still in flight when the gateway shuts down keep their charge; queued
connections no longer cancel the shutdown drain. When the gateway
can return a terminal error frame, its cost carries the same settlement as the
trace. The gateway contract cases in `tests/gateway-spec/cases/upstream_retry.py`
exercise HTTP retries, post-send resets, deadlines and partial settlements.
