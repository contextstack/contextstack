Scheduled dependencies defer without consuming the downstream slot. A downstream run catches up once after its required upstream slot succeeds, a transport-void upstream slot is retried after backoff instead of being mistaken for completed work, and a dead running row left by a crash or reboot becomes an explicit retryable void.
An operator can also authorize one append-only retry of a named hard-failed
current slot after its implementation is fixed; launchd never does this by
default.
A failed catch-up freezes that slot for the day (prior_attempt_failed): no
second automatic catch-up, and dependents stay dependency_not_ready until the
next natural cron slot or an explicit retry_failed.
