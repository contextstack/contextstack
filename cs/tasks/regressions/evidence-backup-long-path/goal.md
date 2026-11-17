# Evidence backup long-path sender

Edit only `ops/bin/evidence-backup`. Use `fs_read` once and one `fs_edit`.
Do not search or run commands. Apply every change in that single edit:

1. Add `import shutil` after `import shlex`.
2. Between `fail` and `main`, add `_rsync_version_compat(candidate)`. Run
   `[candidate, "--version"]` with captured text and timeout 5. Return false
   for `OSError`, `subprocess.TimeoutExpired`, or `ValueError`. Otherwise
   accept only return code zero, no `openrsync` marker, and a match for
   `r"rsync\s+version\s+(\d+)"` whose integer major is at least 3.
3. Add `_resolve_rsync()`. If `EVIDENCE_BACKUP_RSYNC` is set, return it only
   when compatible; otherwise raise `ValueError` containing the exact phrase
   `GNU rsync 3.x required`. Without an override, test executable files at
   `/opt/homebrew/bin/rsync`, `/usr/local/bin/rsync`, then the distinct result
   of `shutil.which("rsync")`. Raise the same actionable error if none pass.
4. Remove the old `/usr/bin/rsync` assignment. Immediately before `lock_path`,
   after all target validation, insert:

```python
    try:
        rsync = _resolve_rsync()
    except ValueError as error:
        return fail(str(error))
```

Keep every transfer argument and the JSON success contract unchanged. Stop
after the edit; the automatic check is the only verification surface.
