# Profiled runs finalize and send every outcome

Demanded by run `20261112-000503-opencode-go-missing-run-record-t1`:
its failed profiled executor published evidence but omitted terminal/check
records, finalization and the Director run envelope.

Exercise OpenCode, OpenCode Go and pi through their shared lifecycle with
fake CLI outcomes. Pass, failed check, executor error, malformed terminal,
provider limit, timeout, cancellation and preflight refusal must persist
terminal/check records, finalize once and send a verified run to a fake
Director. Pending finalization must exit 3 and recover without a second row.
Agent text quoting a usage limit must not classify a completed run as limited.
