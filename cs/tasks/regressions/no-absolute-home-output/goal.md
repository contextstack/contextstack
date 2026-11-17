Read `CHECK.py` first. Edit only `scripts/checklib.sh`. Add a shared automatic
rail that rejects absolute paths below the current `$HOME` when they appear in
candidate output: modified tracked files, staged files, or untracked generated
files. Do not reject an unchanged input merely because it contains such a path.

Keep the scan bounded to candidate output and preserve every existing checklib
behavior. The failure must say `absolute home path` and identify at least one
offending file. Run
`python3 CHECK.py` until it prints `check: ok`. Do not commit.
