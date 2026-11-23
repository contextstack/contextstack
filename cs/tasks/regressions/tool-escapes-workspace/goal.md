# tool-escapes-workspace

The fall (observed 2026-11-09, notes.md). The runner execs a manifest
tool with cwd = workspace and the policy/env.json environment, nothing
more (run.go:50-54). The claim "a manifest tool can read ~/.ssh today"
holds: six of the twelve shipped tools, and the `_calllog` helper that
all eleven python tools share, read or write outside the workspace when
pointed there. Reproduced with a random sentinel in a temp dir and in a
directory created under $HOME for the run; no real dotfile was touched.

Two claims, kept apart:

- Per-tool checks. Every path-taking tool refuses an absolute path and
  a `..` component by string test (`os.path.isabs`, `".." in parts`).
  Only `markdown_section_upsert`, `defuddle_extract`, `evidence_assert`
  (entry, body, assertions) and the `_web` evidence store also resolve
  the path and refuse symlinks. `fs_read`, `fs_write`, `fs_edit`,
  `search`, `okf` (files under a realpath-checked root) and `_calllog`
  follow a symlink planted in the workspace to wherever it points
  (fs_read, fs_edit and okf now implemented by cs-tools ).
- OS confinement. Only `exec` has any, and only for its child: a
  sandbox-exec profile that denies `file-write*` outside cwd (held in
  every write probe, symlinks included) and allows every read. Its argv
  check (`cat /abs` refused) is a string test that `sh -c "cat /abs"`
  walks past, so the child reads any absolute path, `../`, and $HOME.
  The tool itself runs unconfined: its `.cs-out` spill follows a
  symlinked dir out of the workspace.

The boundary is the symlink (or `sh -c`), not the string. A model
needs one `fs_write`-free step to plant a symlink: `exec` with
`ln -s "$HOME" h` is inside cwd and allowed, after which `fs_read`
reads `h/<anything>`. Nothing in the twelve stands between the model
and the home directory except each tool's own string test.

Run:

    tasks/regressions/tool-escapes-workspace/check

Builds the sentinel layout in temp dirs (`outside/sentinel.txt`,
`outside/doc.md`, a `$HOME/.cs-escape-probe-<r>/` it creates and
removes), symlinks inside a fresh workspace per tool, and execs each
tool exactly as scripts/tool-fixtures does. One line per probe:
`FAIL` = the sentinel crossed the boundary, `ok` = the tool or the OS
refused, `ERR` = inconclusive. Cleans up in `finally`.

Verdict, two-sided. With `CS_TOOL_PREFIX` set to the wrapper's words
(`<repo>/target/release/cs-sandbox --policy <code-root>/policy/sandbox.json --`)
it exits 0 only when every probe is `ok`: the acceptance for CS phase 3a,
green since 2026-11-09 (notes.md). Unset, the tools run unconfined, the
escapes are expected and it exits 1 with the final line
`unconfined: N escapes (expected until CS_TOOL_PREFIX is set)`.
The operator wires the wrapped form into scripts/check.

notes.md holds the full table, including the probes against the tools
that refuse on their own (web_fetch, web_search, browser_fetch,
defuddle_extract, markdown_section_upsert, evidence_assert entry/body/
assertions, okf root), and the fs_read spawn baseline for the
under-100 ms overhead rule.
