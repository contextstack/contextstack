# release-tool-runtime

The first Director job on the installed node workstation (2026-11-11,
release `20261111-07abba6b5`) ran `specs/hello-smoke.yaml`, and every Python
tool call died inside `cs-sandbox`:

    dyld: Library not loaded: @executable_path/../lib/libpython3.13.dylib
      Referenced from: ~/.contextstack/releases/<id>/python/cpython-3.13.9-macos-aarch64-none/bin/python3.13
      Reason: tried: '…/lib/libpython3.13.dylib' (file system sandbox blocked open)

Tools run `#!/usr/bin/env python3` through `<code root>/.venv/bin`, a link to
the release's own CPython at `releases/<id>/python/cpython-…`. That install is
outside the code root (`$CODE_ROOT` = `releases/<id>/cs`), and
`policy/sandbox.json` granted only `$HOME/.local/share/uv/python`, where a
checkout's interpreter lives. The dev smoke passed; the release's failed.

The fix: `$TOOL_PYTHON` in a read list is the base install of
`$CODE_ROOT/.venv` (its `pyvenv.cfg` `home`, canonical, less `bin`; cs-sandbox
`policy.rs` `tool_python`), read-only. Both OS lists name it.

In a fake HOME, offline, with this checkout's built `cs` (as `cs-harness`),
`cs-sandbox` and `cs-tools`, the check must show:

1. `node-install install BUNDLE … --no-load` lays out the release with the
   interpreter at `releases/<id>/python/<cpython>` (seeded as
   node-install-layout seeds it) and `cs/.venv/bin/python` linking there.
2. The node plist's own environment (PATH starting at `current/cs/.venv/bin`,
   the roots, `CS_RUNNER_BIN`) runs the release's `ops/bin/run-spec` on a
   hello-smoke-shaped spec (fs_read, fs_write, exec) against a scripted gateway:
   fs_write writes `hello.txt`, exec runs `cat` and `python3 -c` inside the
   sandbox, the task check passes, exit 0.
3. Every wrapped call's report: seatbelt (or bwrap), exit 0, no signal; reads
   include the release's `python/<cpython>` and not `python/` or the release
   root; writes are only the workspace and the call's tmp.
4. `python3` inside the sandbox reports `sys.base_prefix` = that install.
5. Control: the same release's wrapper with `$TOOL_PYTHON` removed from the
   policy aborts fs_write the way workstation did (signal 6, `libpython` in stderr), so
   the check fails on the pre-fix policy.

Never the real launchd (the launchctl shim, `--no-load`), the real HOME or
`~/.contextstack`; no model or network call (the gateway is a local script).
Needs uv, the pinned uv-managed interpreter and the built binaries
(`cargo build --release -p cs-harness -p cs-sandbox -p cs-tools`, rule 0 of
scripts/check); else SKIP and why. Linux runs it under bwrap when present.
Run: `tasks/regressions/release-tool-runtime/check`, about 10 s.
