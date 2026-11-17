Keep lab/ holding only the archived Go runner (cmd/, internal/, go.mod, go.sum,.gitignore,
README.md) and keep every live file pointing at the node tooling where it now lives, cs/
(Director.Plan step 0b). A stale `lab/ops`, `lab/scripts` or similar path, or an
absolute ~/sites/contextstack/lab path outside the frozen inputs, fails the check. History
(ADRs, docs/dev/artifacts, `git show <sha>:lab/...`, "removed by" notes, run evidence under
lab/ledger/) is not a reference.
