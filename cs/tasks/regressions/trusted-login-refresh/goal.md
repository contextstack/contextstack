# Trusted login refresh

A task's private login is never written back. Pi uses a fixed no-tools print;
Codex uses the app-server's proactive account refresh; Grok remains notice-only.
Fresh host staging must preserve the owner's concurrent login and reject unknown
or changed identities. Failed maintenance emits one actionable needs_you notice
per template per UTC day and pauses dispatch until a successful run.
