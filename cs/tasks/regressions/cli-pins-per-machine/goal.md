Profiled CLI templates pin each machine's binary under the config root, not in the repo.

1. First use of a template records `cli-pins.json` under `LAB_CONFIG_ROOT` and the job proceeds.
2. A later job with the same binary hash passes.
3. A changed binary refuses with the agreed message naming `cs doctor --repin`.
4. `cs doctor --repin` (via the module API) re-records the pin and a following job passes.
5. Nothing is written under the code root.
