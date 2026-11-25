# Tracing

This directory defines the ContextStack trace contract and supplies reference
data for Rust and Python producers.

Run `python3 tracing/check-trace --self-test` to check the fixtures, or run
`tracing/check-trace FILE...` to check span JSONL or fixture files.

Changes to `CONTRACT.md`, `fixtures/`, and `check-trace` require owner approval
and their own lane. Keep them out of lanes that change the harness, gateway or node.
