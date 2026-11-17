#56: host MCP bridge sockets stay unreachable from agent profiles.

Offline checks cover deny rules and connection bounding. On macOS the host
runs `--darwin-probe` with the real executor profile against a temporary
lead-mcp socket under /tmp.
