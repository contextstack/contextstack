+A deployed schedules manifest registers exactly one corpus as disabled rows. The operation validates the complete corpus before appending, is idempotent by request identity, and emits a receipt bound to the manifest plus prior and new grant authority. Registration can never activate a schedule.

