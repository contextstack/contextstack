# Discover agents on installs that predate `cs init`

A node without `config/agents.json` reports executor templates for installed,
signed-in agents. If it checks successfully and finds none, it reports an
empty template list. If discovery fails, it omits agent capabilities so the
Director treats them as unknown.

The node reports native harness support when an executable `bin/cs-harness`
is present in its release, including when the node runs through `current`.
