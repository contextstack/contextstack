# sandbox-policy

The escape probes are in `../tool-escapes-workspace`. Every shipped tool runs under `cs-sandbox`.

The host policy `policy/sandbox.json` is validated by the launcher, never by the runner.
`ops/bin/_sandbox_policy.py` `tool_prefix_value` decides `CS_TOOL_PREFIX` for the
runner, and `run.go` `Tool.Run` only prepends its whitespace-split words to the tool argv.

The check must show, against isolated estates only:

1. `CS_TOOL_PREFIX`, `CS_SANDBOX_BIN` and `CS_BROWSER_STATE` are in the runner's `defaultChildEnv`,
   `policy/env.json` `always` and `_env_policy.DEFAULT_ENV`, so hooks and nested
   children see the same switch. `../env-policy` keeps the three lists equal.
2. The committed policy loads, and every manifest tool's descriptor carries a
   `"sandbox"` request whose `net` is one of `none`, `gateway` or `all`.
3. A malformed policy is a named `SandboxPolicyError`. That covers a wrong version,
   a missing or extra key, a relative `runtime_read` or one that is neither a list
   nor `{macos, linux}` lists (phase 3b; the committed file carries both), an unknown
   `allow_net`, and the removed `tmp_in_workspace` (the wrapper owns a per-call tmp).
   An optional `write_env` must be `{VAR: [absolute roots]}`. Only `browser_fetch`
   requests one (`CS_BROWSER_STATE`), and the host confines it to `$HOME/.contextstack`.
   An optional `profiles` is `{browser: {"read": paths}}`; only `browser_fetch`
   requests a profile, and the committed host lists `browser`. An optional `sockets` is
   `{name: absolute path}`, names `[a-z0-9][a-z0-9-]*`; only `browser_session` requests
   one (`browser-public`), and the committed host lists it.
4. Resolution. A present policy is validated first. A preset `CS_TOOL_PREFIX`,
   even an empty one, passes through. With no policy, nothing is set. With a policy
   and a wrapper (`CS_SANDBOX_BIN`, else `target/release`, else `target/debug`
   beside the code_root), the prefix is `<wrapper> --policy <code-root>/policy/sandbox.json --`.
   With no wrapper, `SandboxPolicyError` (unless `CS_SANDBOX_DEV_UNCONFINED=1`: then
   nothing is set and stderr says tools run unconfined). A wrapper
   path containing whitespace is refused. When the policy's `write_env` lists
   `CS_BROWSER_STATE`, it is passed through when set, else `$HOME/.contextstack/code_root/
   browser-state`, and run-spec and tick hand it on; otherwise nothing is set.
5. run-spec hands the runner the prefix, passes a preset through, and exits 2 on a
   malformed policy before any cell runs. tick's `run_spec` does the same and turns
   a malformed policy into a config-void row `sandbox_policy:...`.
6. End to end, a real runner and the fake gateway run a cell through run-spec with
   `CS_SANDBOX_BIN` set to `fake-cs-sandbox`. The cell passes, and the fake's log
   shows each of the three tool calls as `--policy <estate policy> -- <tool path>`.
7. End to end with the real wrapper, found as run-spec finds it (`<repo>/target/release/
   cs-sandbox` beside the estate, `CS_SANDBOX_BIN` unset): the fake model calls
   `fs_write`, `fs_read`, `exec echo`, plants a symlink to a file outside the estate and
   `fs_read`s it. The cell passes, the read is `Operation not permitted` without the
   sentinel, the outside file is untouched, and no per-call tmp dir is left in `TMPDIR`.
   Skipped, with the reason printed, when `target/release/cs-sandbox` is not built.
8. Reports. A prefix the launcher built gains `--report-dir <state>/sandbox-reports/<cell>`
   before its `--`, a fresh dir per cell (run-spec and tick); a preset passes unchanged. With the real
   wrapper, every wrapped call's report lands in the run dir's `sandbox/`, the cell dir is removed, and
   the denied symlink read is `sandbox_denied` naming `exec`. finalize-run with no prefix (tick's sweep)
   claims only reports whose `cwd` is its workspace. Two concurrent cells sharing one report dir each
   claim exactly their own calls' reports.
