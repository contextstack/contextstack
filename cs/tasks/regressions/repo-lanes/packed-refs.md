# A commit's packed-refs.lock diagnostic

On `git version 2.54.0 (Apple Git-157)`, a successful lane commit can report a
nonfatal shared-lock error even with command-scope `maintenance.auto=false` and
`gc.auto=0`. This happens with both loose and packed lane refs. It is not an
update of the packed lane ref: Git writes a loose ref that shadows that entry.

Reproduced with `GIT_TRACE=1`, `GIT_TRACE_REFS` and `GIT_TRACE2_EVENT`, with the
common directory read-only and the worktree gitdir, objects and lane ref writable:

```text
built-in: git commit --allow-empty -qm 'packed lane commit'
transaction_prepare: 0 ""
transaction {
  0: HEAD <old> -> <new> ... "commit: packed lane commit"
  1: refs/heads/lane/worker <old> -> <new> ... "commit: packed lane commit"
}
finish: 0
read_raw_ref: CHERRY_PICK_HEAD: -1 (errno 2)
read_raw_ref: REVERT_HEAD: -1 (errno 2)
transaction_prepare: -1 "Unable to create '<common>/packed-refs.lock': Permission denied"
```

Trace2 records the lock error followed by `exit` with `code: 0`, with no
`child_start` event. There is no maintenance, gc, pack-refs, hook, fsmonitor,
receive or fetch subprocess. The packed-refs bytes remain unchanged. Running
`git update-ref -d AUTO_MERGE` independently produces the identical diagnostic.
The sandbox reports `Operation not permitted` instead of the POSIX fixture's
`Permission denied`.

The cause is Git's [sequencer_post_commit_cleanup](https://github.com/git/git/blob/v2.54.0/sequencer.c),
called after updating HEAD by [builtin/commit.c](https://github.com/git/git/blob/v2.54.0/builtin/commit.c).
It unconditionally deletes `AUTO_MERGE`, even when absent. The
[files ref backend](https://github.com/git/git/blob/v2.54.0/refs/files-backend.c)
prepares a packed transaction for deletion and locks packed-refs *before* checking
whether the packed transaction is needed. A normal unrestricted workspace lets
that unnecessary lock succeed, which hid the overly broad stderr assertion.

Keep shared packed-refs and its lock denied. Do not wrap Git, suppress its stderr,
or grant a global ref lock just to silence this cleanup. The regression accepts
only the exact independently reproduced cleanup diagnostic, with proof of one
successful lane transaction before the failed cleanup, successful commit exit,
no subprocess, disabled maintenance settings, absent cleanup refs, and unchanged
packed refs. An unexpected diagnostic or a failed lane update still fails. The
existing maintenance test separately reproduces auto-packing and verifies that
staged command-scope settings prevent it.

The executor fixture now starts with an already packed lane and receives
hook-shaped `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_PREFIX`, `GIT_EDITOR`,
`GIT_CONFIG_COUNT` and conflicting `GIT_CONFIG_PARAMETERS`. Its environment
allowlist drops inherited Git controls before `stage_git` installs the private
settings; the agent probe verifies the effective settings are `false` and `0`.
The common directory mode fixture exercises the same Git behavior even when a
coding-agent sandbox cannot launch another Seatbelt profile.

Run the focused Git/maintenance reproductions with
`python3 cs/tasks/regressions/repo-lanes/verify.py --commit-only`; run the complete
executor/landing regression with `cs/tasks/regressions/repo-lanes/check`.
The focused path also calls the real executor environment allowlist with those
hook variables before applying `stage_git`'s settings, so it checks precedence
without launching a nested sandbox.
