Pin the workspace HEAD before the CLI runs. After the CLI, every path
changed from that base (committed, unstaged, or untracked) must sit
under grant.paths. A CLI commit must not empty the grant diff.
