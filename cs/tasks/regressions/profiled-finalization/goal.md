# Profiled runs finalize and send every outcome

Demanded by run `20261112-000503-opencode-go-missing-run-record-t1`:
its failed profiled executor published evidence but omitted terminal/check
records, finalization and the Director run envelope.

Exercise OpenCode, OpenCode Go and pi with fake CLI outcomes. Each adapter
execution must hand off one execution receipt and leave finalization to
`machine-finish`. Pass, failed check, executor error, malformed terminal,
provider limit, timeout, cancellation and preflight refusal must persist
terminal/check records with the expected verdict and stop, write one ledger
row, and send a verified run envelope to a fake Director. A finalization
interrupted after the row write must exit 3 and recover from the same receipt
without appending a second row. Agent text quoting a usage limit must not
classify a completed run as limited.
