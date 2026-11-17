# schedule-firing-identity

Node protocol 2 (NODE-PROTOCOL.md §9.5): a node holds and fires its own schedules, and every firing has a
stable identity `<schedule_id>@<slot>` recorded before its job starts, so a crash never re-runs a job blindly.

- `schedule-tick` appends a `firing_recorded` row (fsync'd, with the schedule's revision) before it enqueues or
  dispatches the job, then the fire row as before.
- A slot recorded but never fired (a crash between the two) is `uncertain`: deferred `firing_uncertain`, never fired
  again unless `retry_failed` names the schedule.
- `missed` sits next to `catch_up_window_minutes`; a row without it takes the caller's `missed_default`
  (node-listen passes `skip`, protocol 2's default); with neither, the slot fires as it always has. `skip` records a slot first seen more than
  `missed_grace_minutes` (60) after it was due as a `missed` deferral and runs nothing; that slot stays settled.
  `catch_up` fires it and marks the fire row `catch_up: true`. A slot already deferred (an outage, a dependency)
  was seen, not missed: it still fires when it can, as before.

Run: `tasks/regressions/schedule-firing-identity/check`.
