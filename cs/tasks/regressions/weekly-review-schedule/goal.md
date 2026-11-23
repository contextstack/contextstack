# weekly-review-schedule

`cs review weekly` only ran by hand. It now rides the node's scheduler: `cs review schedule`
appends one host row to `<state>/schedules.jsonl` (Monday 08:00 local, `missed: catch_up`).
`schedule-tick` (already run by tick and node-listen) fires `command: review-weekly` instead of
enqueueing a cell. Delivery is `<state>/reviews/<date>.md` plus one `needs_you` row in
`<state>/attention.jsonl` that links to the report.

Idempotent: the first Monday fires once; a later tick that week does not; a Monday missed while
the machine was off catches up exactly once. A second `cs review schedule` does not append again.

A delivery that keeps failing is a void terminal on the dispatch shelf (the history
`repeated_failure` reads). After the same consecutive-failure threshold as any other schedule,
the row is disabled with a `hold` reason and a deferred entry, and later ticks do not retry it.
A successful delivery records a pass on that shelf and resets the streak.

Run: `env -u CS_STATE_ROOT -u CSGATEWAY_ENDPOINT tasks/regressions/weekly-review-schedule/check`.
Fake clock, temp state root. Never `~/.contextstack`.
