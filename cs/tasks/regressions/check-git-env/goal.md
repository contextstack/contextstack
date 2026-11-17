# check-git-env

A commit
hook runs `scripts/check` with `GIT_DIR`, `GIT_INDEX_FILE` and `GIT_WORK_TREE`
exported. Every rule inherited them, so a regression's fixture repo
(`git init`, `add`, `commit` in a temp dir) wrote into the real repository and
its index instead. On 2026-11-10 that left 18 stray commits on `main`, a
garbage index and a Fixture user in `.bare/config`, after a hook-env
simulation; the operator repaired it. `node-install-layout` was also red only
inside the hook.

`scripts/check` unsets every `GIT_*` before any rule runs. The check must show,
with `GIT_DIR`, `GIT_INDEX_FILE`, `GIT_WORK_TREE` and `GIT_PREFIX` pointed at a
throwaway bare repo and index in a temp dir (never the real repository):

1. Rule 9 alone (`LAB_SPILL_ONLY=1`, `LAB_SPILL_DESTS` = a fixture dest with
   `.lab-out` tracked) reports that dest's spill and exits 1. Its git ran
   against the dest, not the decoy. Without the scrub it reads the decoy's
   empty index and says `spill ok`.
2. The decoy repo is byte for byte unchanged (config, refs, objects, index).
3. lab itself is judged by its own repository, not the decoy.

Fails on the previous `scripts/check` (case 1). Run:
`tasks/regressions/check-git-env/check`, about 2 s.
