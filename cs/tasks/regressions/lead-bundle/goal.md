# lead-bundle

Demand: Design.HostedLead.2026-11-11 lane 2. Ship the conversation bundle a hosted lead runs with and
`cs lead create` to mint the conversation plus lead credential.

1. **Template** (`cs/lead/`): `spec.yaml` lists `conversation/context.md` and `tasks/turn`; system prompt
   covers lane orchestration via cs mcp; `session/mcp.json` wires `cs mcp --credential-file`; no forbidden paths.
2. **`validate_bundle`** matches director-spec §11 reserved-path rules and required spec fields.
3. **`cs lead create`** against a temp Director: 201 conversation, 201 lead credential, 0600
   `config/lead-credential` and `config/hosted-lead.json`, token never in stdout/stderr.

Needs `target/release/cs-director` (or `$DIRECTOR_BIN`), else SKIP. Run: `tasks/regressions/lead-bundle/check`.
