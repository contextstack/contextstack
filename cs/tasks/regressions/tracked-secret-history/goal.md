# tracked-secret-history

A lane's sandbox can read the repo's object store, so a secret-named file is
still readable after it leaves the branch tip: an older commit, another
branch, a stash or notes ref, or a different case (`.ENV`, `Secrets.Env`).

`refuse_tracked_secrets` runs on the host before a worktree is created. It
matches names case-insensitively, scans every reachable ref (`git log --all`,
which includes stash and notes) for added and renamed paths, and never reads
file contents. It refuses with the paths and the commits that introduced
them, and names `git filter-repo` or BFG as the purge. The owner allowlist
still matches exact paths only. `.example` and `.sample` names stay exempt.

The scan is cached for the repo's current ref tips (memory and
`.git/cs-tracked-secret-scan.json`). A later lane start with the same tips
does not walk history again; a new tip does. Temp repos and decoy blobs only.
