# director-backup

Demand: the owner (2026-11-20) wants the machine's irreplaceable state backed up
off-machine to S3-compatible storage, first DigitalOcean Spaces. Nothing backed up
`~/.contextstack` before. Runs and traces are regenerable; the Director database
and the eval sets (including held-out cases) are not.

Hermetically (fake HOME, a local fake S3 server, no network), the check must show:

1. `cs backup run` snapshots a live WAL-mode database while a writer is active,
   and the restored copy is identical and passes an integrity check.
2. The uploaded object is encrypted: no SQLite header and no plaintext eval text.
3. Retention keeps 14 dailies and 8 weeklies and deletes only older objects under
   the configured prefix.
4. `cs backup restore` refuses to overwrite the live database path.
5. A missing credential fails with an error naming the credential, never a value.
