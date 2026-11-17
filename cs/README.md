# cs

The node tooling of the ContextStack experimentation lab (moved out of
`../lab/` by Director.Plan step 0b). Specs, tasks, and the ledger are
the estate; the runner (`../harness/`, Rust) is a disposable loop. Behavior
changes by spec, never by service. See INVARIANTS.md (one page).

Born 2026-08-25 from the lightweight reset, after the Bot-era
finding: production governance pointed at learning yields paperwork,
not evidence. This repo is the inverse: cheap runs, remembered
verdicts.

## Layout

- `specs/` — experiments (YAML). `specs/archive/` — losers + verdicts.
- `tasks/` — eval tasks as directories. `tasks/regressions/` — falls.
- `prompts/` — system prompts / context packs. Hypotheses, not lore.
- `tools/bin/` — the tool surface: executable + `<name>.json` pairs.
- `hooks/bin/` — post_turn executables; spec-declared, deny-by-default.
- `ops/bin/` — operator harvest/merge/setup-refresh/enqueue/tick; never palette.
- the ledger — append-only rows and run evidence, under the state root (`LAB_STATE_ROOT`, ~/.contextstack/state).
- `PINNED.md` — pin, routing, recent changelog (older: ledger/CHANGELOG.md).
- `scripts/check` — the constitution as a linter (also git hooks).
  The monorepo's `scripts/git-hooks` chain to `scripts/hooks` here.
- the runner: `../target/release/cs-harness` (`cargo build --release -p cs-harness`); `cs-harness run`.

## Build

`cargo build --release -p cs-harness -p cs-tools -p cs-sandbox -p cs-gateway`
at the repo root (scripts/check does this). The gateway is
`../target/release/cs-gateway` (listens on :9200).

## Manual use (a human at the keyboard)

From this repo root, with the gateway up:

    export CSGATEWAY_ENDPOINT=http://localhost:9200/v1/agent/model.call
    ../target/release/cs-harness run specs/hello-smoke.yaml  # one cell (spec, task, trial)
    ops/bin/run-spec specs/<spec>.yaml                # full tasks x trials
    ../target/release/cs-harness run -task tasks/<t> specs/<s>.yaml  # one task
    # watch each tool call (Enter=run, q=abort): spec `pre_tool: [step-confirm]`

Read results: summary table on stdout; per run,
`<state root>/runs/<id>/transcript.md` (every turn, tool call, verdict) and
`workspace/` (the evidence, left untouched). `scripts/report` gives
per-EXP pass rate / cost / seconds; grep `<state root>/ledger.jsonl` for
anything else — rows are append-only truth.

`browser_fetch` gives every host class an isolated browser profile and
stops the browser when the fetch exits, including error paths. Visibility
defaults to `auto`: authenticated hosts use short-lived headed installed
Google Chrome, while public pages use headless Playwright Chromium. The
authenticated partitions import only the matching cookie domain from the
regular profile; they never control the regular profile itself. Set
`LAB_BROWSER_ENGINE=chrome` to use installed Chrome for public pages too,
`LAB_BROWSER_VISIBILITY=headless` for an authenticated comparison run, or
`LAB_BROWSER_VISIBILITY=headed` for a visible public diagnostic.
`LAB_BROWSER_KEEP_OPEN=1` is an operator-only debugging override; normal
scheduled runs must leave it unset.

Try a thought (everything runs through a spec so it lands in the
ledger; there are deliberately no unledgered runs):

    1. Copy specs/<nearest>.yaml to specs/<slug>.yaml and edit.
       SCHEMA.md lists the fields.
    2. ops/bin/run-spec specs/<slug>.yaml
    3. Record the verdict as one PINNED.md changelog line. Loser →
       move the spec to specs/archive/ + a verdict: line. Winner →
       update the pin/routing lines, evidence cited.

Add a task by hand: `mkdir tasks/<name>` with `goal.md` (stimulus
only — no comments), `setup/` (start state), an executable `check`
(cwd = workspace, exit 0 = pass), and optional `notes.md`/`solution/`
(lab-facing, never mounted). `scripts/check` lints all of this and
runs as the pre-commit hook.
