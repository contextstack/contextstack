# Contributing

Thank you for helping improve ContextStack during the alpha.

## Build

From the repository root:

```sh
cargo build --release
```

## Run checks

The main validation script is `cs/scripts/check`. It builds the workspace and runs linting, unit tests, and contract suites.

Unset the dev ledger root so a local run does not write into a personal state directory:

```sh
env -u LAB_STATE_ROOT cs/scripts/check
```

## Git hooks

Enable the repository hooks after you clone:

```sh
git config core.hooksPath scripts/git-hooks
```

Staged Go files require a `Demanded-by: <intent-id>` line in the commit message. Other hooks enforce project conventions.

## Changes and tests

Include tests when you change behavior that the check suite already covers, or add coverage when you introduce new behavior. A green `env -u LAB_STATE_ROOT cs/scripts/check` is the bar before you open a pull request.

## Issues and pull requests

Issues and pull requests are welcome while the project is in alpha. Please describe what you expected, what happened, and how to reproduce it. For install and runtime problems, include `cs doctor` output when you can.
