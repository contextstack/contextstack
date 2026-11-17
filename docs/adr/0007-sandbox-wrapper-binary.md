# 0007. Every tool call runs inside a separate sandbox wrapper, `cs-sandbox`

- Status: Accepted
- Date: 2026-11-09

## Context

Tools ran with the workspace as cwd and no OS confinement except `exec`'s
own profile. A regression observed 15 escapes: six shipped tools and the
shared call-log helper read or wrote outside the workspace through a
planted symlink or `sh -c` (`tasks/regressions/tool-escapes-workspace`).
The sandbox must be callable by two runners in two languages and later by
the gateway. Shipped runtimes from coding agents were evaluated (vault
`Phase3.SandboxEvaluation.md`); none met the requirements as shipped.

## Decision

- A small Rust binary, `cs-sandbox`, wraps each tool call:
  `cs-sandbox --policy FILE [--report-dir DIR] -- TOOL`. The launcher
  sets `UAH_TOOL_PREFIX`; both runners only prepend it.
- Policy is the intersection of the host policy (`lab/policy/sandbox.json`)
  and the tool descriptor's request; it is never widened, and every
  narrowing is recorded. Default deny: the workspace, a per-call temp dir
  outside it, declared runtime paths read-only, no network.
- Backends: Seatbelt on macOS; bubblewrap plus a seccomp filter on Linux.
  Landlock was dropped (bwrap's empty-root namespace already covers it).
  Starting text is OpenAI Codex's Seatbelt files and seccomp list
  (Apache-2.0, vendored with attribution); nothing is taken from Claude
  Code's sandbox runtime.
- Exit 101 means the policy cannot be enforced on this host; the tool is
  never run unconfined. Without a built wrapper the launcher says loudly
  that tools run unconfined (development mode).
- Writes to directories named by environment variables are allowed only
  under the host state root (`~/.contextstack`, ADR 0008).
- Per-call reports let `finalize-run` record `sandbox_denied` or
  `sandbox_unenforceable` on the run's row, attributed per tool.

## Consequences

- The escape regression is green confined (0 of 15) on macOS, Arch and
  Ubuntu with its AppArmor user-namespace restriction; `scripts/check`
  enforces it whenever the wrapper is built. Overhead is about 7–11 ms per
  call.
- Sandboxes do not nest, so `exec` skips its own inner sandbox when
  already confined.
- macOS cannot enforce per-call memory or process limits; the probe says
  so, and a job requiring them refuses on the Mac.
- Not yet built: the browser profile, denial attribution per call, Windows
  (on hold), and the gateway's Unix-socket listener for `net: gateway`.
- History: `504169a61`, `ac5c5bfad`, `1466217cc` (macOS); `af00eb866`
  (Linux); `5a2088b43` (visible denials).
