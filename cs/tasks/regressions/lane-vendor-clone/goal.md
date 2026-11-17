New lane worktrees receive independent copies of main's clients/web/vendor
and clients/web/node_modules when present. Editing a lane copy must not change
main. Reusing a lane preserves its dependencies; missing source directories
remain absent. Demand: lanes have no network to install ignored dependencies.
