# lead-session-host

Demand: Design.HostedLead.2026-11-11 lane 3. The node session host runs hosted-lead turn jobs
through one live stream-json CLI per conversation; session id persists under state; resume after restart.

Run: `tasks/regressions/lead-session-host/check` (offline stub CLI, no network).
On macOS at landing, `check` also runs `darwin_sandbox_smoke.py` when `sandbox-exec` is available.

Require a per-serve token inside an owner-only socket directory; refuse unsafe directories,
unauthenticated requests and bundles belonging to another conversation. Build MCP configuration
from the host template, and keep socket directories inaccessible to sandboxed agents (#39).
