# Python transports requests; Rust emits Seatbelt policy

Scan every production Python module and executable under `cs/ops/bin` for
allow/deny rule literals, `_sbpl` imports and profile text appends. Exercise the
scanner against representative forbidden constructs. No exceptions are needed:
URL and shell quoting and writing a complete Rust-rendered profile are allowed.
