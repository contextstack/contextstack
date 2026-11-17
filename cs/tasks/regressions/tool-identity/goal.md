# tool-identity

A run's frozen identity names the tools and hooks it executed. Two runs of the
same spec in the same estate, with one byte of one tool changed between them,
record different `tool_builds` for that tool alone and different `env_sha`.
A changed helper the tools import changes every importer's digest. A tool name
that cannot be read records null, never a refusal. Go and Rust agree.
