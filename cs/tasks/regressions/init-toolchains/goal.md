`cs init` records login-shell toolchains the node service PATH lacks in
`config/toolchains.json`, merging without dropping hand-edited entries.
A failed or timed-out shell lookup changes nothing and prints a plain line.
`cs doctor` reports a declared binary that no longer exists.

Demand: lanes on machines with minimal service PATH (Herd PHP, nvm, asdf)
should not require hand-written toolchains.json.
