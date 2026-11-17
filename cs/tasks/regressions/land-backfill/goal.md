# land-backfill

One-shot `land-backfill` reads landing commits on a repo's `main`, finds
the newest finished Director job for that repo and lane that finished before
each commit, and posts a `landed` record. Squash subjects `Land lane/<name>`,
legacy `lane <name>: …`, and merge subjects `Merge branch 'lane/<name>'` all
count. Re-running is idempotent; `--dry-run` posts nothing.
