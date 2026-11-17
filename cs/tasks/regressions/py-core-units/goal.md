# py-core-units

Decision E4 (90% line coverage on core Python libraries before 0.1.0): behaviour
tests for `_toolchains`, `_estate_io`, `_provider_limits` and Rust-rendered profiles in
`ops/bin/`, imported directly with hermetic temp fixtures. Coverage is reported
when `python3 -m coverage` is available; it is not a hard gate.

Run:

    cd cs && env -u LAB_STATE_ROOT tasks/regressions/py-core-units/check
