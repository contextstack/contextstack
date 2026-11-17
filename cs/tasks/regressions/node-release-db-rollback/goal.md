# node-release-db-rollback

When a new release's smoke fails after its Director migrated the database, rollback restores the pre-upgrade snapshot (kept beside the data in state/director-snapshots, newest two) before the previous release starts. Review 2026-11-15.
