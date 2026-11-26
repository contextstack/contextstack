# Host-owned lead MCP credential boundary (#38)

The hosted lead's MCP command runs on the host, while the CLI's stdio proxy
runs within its sandbox. A temporary decoy credential remains unreadable by
the sandbox and absent from proxy configuration. JSON requests round-trip
through the bridge; a connection without its capability cannot launch MCP.
All fixtures live in a system temporary directory; no installed state is read.
