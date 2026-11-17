An installed estate names itself in its config root, never in the release.

With LAB_CONFIG_ROOT holding ESTATE "m9" and a throwaway LAB_STATE_ROOT,
_estate_io.estate_id reads m9; _grant.live_error accepts an m9 grant and
refuses a dev one. A node-bundle release carries no ESTATE. Without
LAB_CONFIG_ROOT the checkout reads lab/ESTATE. No live ledger is written.
Run: `tasks/regressions/estate-config-root/check`.
