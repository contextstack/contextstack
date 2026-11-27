# node-install

The contract (Plan.CS.v2.1 §7 "Packaging the rest", "Runtime inventory";
phase 2 exit "a fresh Ubuntu machine runs the manifest tools with no system
Python"). The node image's Python side is `pyproject.toml`,
`uv.lock` and `.python-version` at `code_root/`; `scripts/node-bundle build` makes
an offline bundle (one release) on a connected machine; `scripts/node-install install
BUNDLE` installs it with no network and no system Python into
`~/.contextstack/releases/<id>` and writes its JSON report to `verify.json` there
(the layout around it is `node-install-layout`).

- The twelve manifest tools and every helper they import are standard
  library only (`scripts/node-bundle deps`): the project has no dependencies
  and nothing is downloaded at tool execution time, ever.
- The interpreter is the uv-managed python-build-standalone build pinned in
  `.python-version`, installed from the bundle's `file://` mirror under
  `UV_OFFLINE=1`; `uv sync --locked --offline` makes the release's `code_root/.venv`.
- `node-install` refuses a bundle for another OS or architecture, a directory
  that is not a bundle, and bad arguments, each with exit 1 and a reason.
- The report verifies the interpreter version and location, every bundled
  file's sha256 in the bundle and in the installed release, each binary and
  its `target/release/` link, and each manifest
  tool's runtime: python3 from the release; node, curl, bwrap or
  sandbox-exec and the public browser broker from the host, present or
  missing per the manifest's `requires`. Exit 0 complete, 2 a host runtime
  missing (the tools needing it are named), 1 the install is wrong.
- A refused install writes nothing under HOME; no uv `~/.local/bin` shim, no
  file in `code_root/`. A bundle for a target this host cannot build records every
  binary required, not bundled.
- A bundle for this host ships `bin/` cs-harness, cs-sandbox, cs-tools,
  cs-gateway and browser-host, plus the Chrome extension. It builds with no `go` on PATH: the Go runner and
  gateway are archived ; earlier releases keep theirs.
- A production bundle is built from an export (`git archive HEAD | tar -x`,
  then `node-bundle build --commit <sha>`): there `release_files` is the
  archived tree, never the checkout's untracked files (other lanes'
  work in progress), and `build` without `--commit` refuses before writing,
  also with a commit hook's `GIT_DIR` and `GIT_INDEX_FILE` set (node-bundle
  runs git without the caller's `GIT_*`).
- In a git work tree, `release_files` is tracked paths only (plus
  `tools/defuddle/node_modules` from `npm ci`); `node-bundle build` refuses
  when `git status --porcelain` shows untracked or modified files under bundle
  paths unless `--allow-dirty` (CI checkouts are clean).
- `node-bundle build` assembles in a system temp directory and moves the finished bundle onto OUT_DIR
  only when it is complete. A failure leaves OUT_DIR absent and no partial bundle beside it.
- The installed interpreter runs the manifest tools through
  `scripts/tool-fixtures --python <release>/code_root/.venv/bin/python3`.

Run: `tasks/regressions/node-install/check`, about 6 s. The dependency scan
and the refusals always run. The install itself runs only when this host has
`uv` and a uv-managed interpreter of the pinned version (`uv python dir`):
the check seeds the release's `python/` in a fake HOME from that copy so no archive is needed, and
blocks the network with a dead proxy on top of `UV_OFFLINE=1`; otherwise it
reports SKIP for that part and says why.
