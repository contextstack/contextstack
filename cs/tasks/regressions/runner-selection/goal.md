The node selects the configured `CS_RUNNER_BIN` or defaults to the Rust harness.
Dispatch supplies the child policy, work-order identity, schedule fields and
explicit external-effect grants. Count runners by executable path, and rebuild
the Rust harness atomically, preserving the existing executable on failure.
