# spool-delivery

The contract of the first spool increments (Plan.CS.v2.1 §9, phase 4b S2–S3), so a
later receiver cannot weaken it silently. `finalize-run` writes each run's envelope to
`<state>/outbox/<run_id>.json`; `ops/bin/spool-send` delivers it to `policy/spool.json`'s receiver.

- Null receiver (shipped state, until phase 6): nothing is sent; the reply says so and counts the outbox.
- At-least-once: a send killed after the copy leaves no ack and the entry pending; the next send acks.
- Idempotent: an `event_id` already acked with the same `envelope_sha256` is not copied again and
  returns the same ack.
- Ack only after verification: the receiver re-hashes every file; a mismatch writes no ack.
- Admission stop: below `min_free_bytes` on the state root `tick` admits nothing (one dispatch
  `yield` row, `error.class: spool_full`) and `pending-judgments` raises `spool_full` until space returns.

Run: `tasks/regressions/spool-delivery/check`. Isolated estates; `_tick_estate.py` builds them.
