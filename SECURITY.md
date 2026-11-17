# Security

## Reporting a vulnerability

Please report security issues privately. Use [GitHub private vulnerability reporting](https://github.com/contextstack/contextstack/security/advisories/new) for the `contextstack/contextstack` repository.

Do not open a public issue for something that could harm users before a fix is available. We will acknowledge reports and work with you on a timeline.

## Scope

In scope:

- ContextStack binaries and services in this repository (Director, gateway, runner, sandbox, `cs` tooling, and the web client).
- Install and update flows shipped from this repository.
- Misuse of agent sandboxes, credentials, or landing workflows that could affect a user's machine or data.

Out of scope:

- Vulnerabilities in third-party coding agents you configure yourself (Claude Code, Codex, Cursor, and similar).
- Issues in dependencies that are already tracked upstream, unless ContextStack's use makes them exploitable in a default install.
- Social engineering or physical access to an already-unlocked machine.

## Known limits (alpha)

This release is alpha software. Read [alpha known issues](docs/release/alpha-known-issues.md) for the current picture.

Two items matter for security reviews:

- **Landing checks** run outside the agent sandbox, with your user privileges, when you land a task lane.
- **Sandbox hardening** is ongoing. Agent tool calls are confined, but the policy is not complete for every threat model.

We treat reports about these areas seriously, especially when you can show practical exploitation beyond what the known-issues page already describes.
