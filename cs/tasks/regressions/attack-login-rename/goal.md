---
host_only: requires applying a fresh macOS Seatbelt profile from the host
---
Protect owner login files while coding agents use private per-job copies.
For every executor template copying Codex, Pi or Claude login data, stage
marker JSON from a temporary HOME and exercise the real Darwin sandbox profile.
Require EPERM for workspace, /tmp and .auth-XXXXXX renames, hardlinks,
symlinks, unlink plus create, in-place overwrite and chmod against the owner file.
Allow rewriting the private job copy, then finish it through the production
non-refresh path. The owner bytes and 0600 mode must survive unchanged and
completion must discard attacker data without writing it back.
Use decoys only. Run with --darwin-probe on macOS; skip cleanly when an outer
sandbox refuses sandbox-exec. The lead runs the probe on the host at landing.
