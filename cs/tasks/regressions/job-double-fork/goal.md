# job-double-fork

A job's descendants must not outlive the job or escape its memory cap, including after:
- a double fork, `setsid` and `chdir("/")`;
- a cleared environment;
- an exec of an Apple platform binary (`/bin/sh`, `/bin/sleep`), whose environment other processes can't read.

Tracking uses macOS process responsibility: a disclaimed supervisor is responsible for every descendant, combined with the job's env marker and verified birth identities. Each signal re-checks the target's start time first, so a reused PID is never killed.

Host-only parts run with `--darwin-probe`. The lead's bypass variants are required cases. Background: 5th pass #24 and the #48 PID-reuse finding.
