The CLI executor must stop a spinning lane before its normal budget stop when deterministic stuck signals trip.
Reproduce with a fake Claude stream that emits the same failing tool result repeatedly, verify stop `stuck:repeat`,
and verify `stuck.md` is written into run evidence and surfaced in the row reason.
Also verify a healthy stream is not stopped, and `budget.stuck: off` disables the stop.
