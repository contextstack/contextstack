# policy — node policy the launcher resolves (Plan.CS.v2.1 §4, decision 20; `cs-sandbox`, phase 3a)

Node authority, never spec authority: a spec may request, this directory decides.
Rules tier (`scripts/estate_classify.py`): a change lands only in a green-check commit.

`env.json` — what a child of the runner (tool, hook, op) inherits from its environment.
- `always`: inherited by every child. Equals the list the runner compiled in before
  2026-11-08, which it keeps as its only fallback (`defaultChildEnv`, internal/cs/run.go);
  `tasks/regressions/env-policy` keeps file, Go literal and `_env_policy.DEFAULT` equal.
- `on_request`: names a spec may ask for with `env: [NAME]`. The launcher (`ops/bin/tick`,
  `ops/bin/run-spec`, via `ops/bin/_env_policy.py` `resolve`) resolves `always` ∪ (`env` ∩ `on_request`)
  into `CS_CHILD_ENV` for the runner; any other request is refused by name before a cell
  runs. A direct `cs-harness run` has no launcher: the default list, no request honoured. An empty
  resolution (`always: []`, nothing requested) is an empty child environment, not the default.
- To admit a name: a rules commit adding it here. tick owns which `CS_*` values are set; this
  file owns which reach children. `finalize-run` gives its own children `always` only.

`sandbox.json` — what `cs-sandbox` lets every tool reach (CS phase 3a).
- `runtime_read`: paths any tool may read (not the whole code root: `tools/`, `ops/bin`, policy,
  scripts, hooks, `.venv`, and `$REPO_ROOT/target/{release,debug}` for runtime exe tools;
  interpreters and system paths); `$CODE_ROOT`, `$REPO_ROOT` (the directory holding the code root),
  `$HOME` expand. Secret env files (`.env`, `.env.*` except `.env.example`) are denied under every
  readable root (Seatbelt regex; Linux masks with `/dev/null`). `$TOOL_PYTHON` (read lists only) is
  the install the code root's `.venv` runs on:
  its `pyvenv.cfg` `home`, canonical, less `bin/`. A checkout's is uv's managed Python; an installed
  release's is `releases/<id>/python/cpython-…`, outside the code root (without it every
  Python tool aborted in dyld on workstation). Unresolved (no venv): the entry is dropped. One list per OS, `{"macos": [...], "linux": [...]}` (a plain list holds on
  every OS; an OS with no list is exit 101). Linux names system files, not all of `/etc`; a
  path absent on the host is skipped and reported as a `read` narrowing (phase 3b). `allow_net`: the `net` values a descriptor's `"sandbox"` request may get
  (`none` | `gateway` | `all`); anything else narrows to `none`. The wrapper intersects this
  with the descriptor request and gives each call a private TMPDIR outside the workspace.
- `write_env` {VAR: [roots]}: write targets named by variables are confined to the host's state
  root, `~/.contextstack` (launcher default `CS_BROWSER_STATE` = `~/.contextstack/state/browser-state`).
- `profiles` {browser: {read}}: listing `browser` lets a descriptor's `"profile": "browser"` run headless
  Chromium (gstack, Playwright cache); the wrapper then turns Chromium's own sandbox off and is the
  browser's only sandbox (`sandbox/README.md`). Unlisted: narrowed, the tool fails.
- `sockets` {name: path}: unix sockets a descriptor's `"sockets": [name]` may reach, the only endpoint
  added (`CS_SOCKET_<NAME>` names it). `browser-public` is the browser broker's `public` set.

`browser.json` — `ops/bin/browser-broker`: the Chrome binary per OS and the origin `sets`;
each set is served on `~/.contextstack/browser/sockets/<set>.sock`, which `sandbox.json` grants by name.
- The `$HOME/Library/Application Support/Herd/.../node/v22.21.1` entry is host-specific (this
  Mac's node, for `defuddle_extract`); a node image's install path replaces it.
- The launcher (`ops/bin/tick`, `ops/bin/run-spec`, via `ops/bin/_sandbox_policy.py`)
  validates it and sets `CS_TOOL_PREFIX` = `<cs-sandbox> --policy <this file> --` for the
  runner, which prepends it to every tool argv. A preset `CS_TOOL_PREFIX` passes through;
  no built wrapper (`CS_SANDBOX_BIN`, else `target/{release,debug}/cs-sandbox` beside
  `cs/`) refuses unless `CS_SANDBOX_DEV_UNCONFINED=1` explicitly enables unconfined
  development tools (said on stderr). `CS_CHECK_SKIP_ESCAPE_SUITE=1` only permits check to
  skip tool escape tests when the release wrapper is missing; it never disables runtime confinement. Malformed = config void
  `sandbox_policy`; `tasks/regressions/sandbox-policy` holds the rules.

`spool.json` — the spool (Plan §9, phase 4b S2–S3). `receiver`: null until phase 6, so
`ops/bin/spool-send` sends nothing and only counts `<state>/outbox`; `{kind: "dir", path}` copies each
envelope and its files there, acked only after the receiver re-verifies every sha256. `min_free_bytes`:
below it on the state root, `tick` admits nothing (a dispatch `yield` row, class `spool_full`). Absent file = no receiver, no floor.
