# Config/credential void hold

A scheduled run that voids on no_provider_credential,
provider_unsupported, or unknown_model writes one ledger row, holds
the schedule with that reason, and is never retried. Spawned by the
2026-09-04T10:00Z probe slot: exp-003-h first void
ledger/runs/20260904-100049.130750000-exp-003-h-create-file-t1, then
five more jobs (~60 rows) because attempt_state treated void as
retryable.
