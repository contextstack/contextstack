# tool-fixtures

The contract (Plan.CS.v2.1 §7 "Fixtures, enforced").
Every tool in `tools/manifest.json` runs under contract fixtures beside the
tool, `tools/bin/<name>.fixtures/<case>/`, exec'd exactly as the Go runner
execs a tool (`internal/code_root/run.go` Tool.Run): argv the tool path alone, cwd a
fresh workspace, stdin the case's `args.json`, stdout and stderr combined,
timeout max(descriptor `timeout_seconds`, 60) s, env the `policy/env.json`
`always` names. Tools are never imported.

- Every manifest tool has a fixtures dir with at least one positive and one
  negative case; a manifest tool without fixtures fails, as do fixtures with
  no tool.
- A case pins exit code, combined output (exact, contains, or regex), the
  files the tool created or changed (exact, sha256, contains, or must not
  exist) and, with `untouched`, that nothing else in the workspace moved.
  `.cs-calls.jsonl` is pinned where the tool writes it, so the doom-loop
  hook's view of each call is part of the contract (okf writes none).
- Fixtures never touch the network. web_fetch and web_search ship negative
  cases only (README.md in each dir says why); browser_fetch renders through
  a fake `browse` named by `env.json`; defuddle_extract uses the vendored
  `tools/defuddle` under local `node`.
- Only inherently variable output is normalised: the workspace, case and
  temp paths, and (opt-in per case) RFC 3339 instants. Nothing else.
- `--python P` runs the same fixtures through a given
  interpreter; lane 3's offline-install proof depends on this.
- A fixture is never edited to match a tool whose behaviour looks wrong; the
  surprise goes in notes.md.

Run: `tasks/regressions/tool-fixtures/check` (= `scripts/tool-fixtures`),
about 3 s for 60 cases on Darwin. The `exec` cases need `sandbox-exec`
(Darwin) or `bwrap` (Linux), as the tool itself does.
