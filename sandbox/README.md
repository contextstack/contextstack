# cs-sandbox

OS confinement for one CS tool call (Plan.CS.v2.1 §6; phase 3a macOS, phase 3b Linux). Both runners exec it in front of every tool through `CS_TOOL_PREFIX`; it reads the host policy and the tool's descriptor, intersects them, and runs the tool confined: on macOS a deny-default Seatbelt profile through `/usr/bin/sandbox-exec -p`; on Linux `bwrap` with an empty root built from the policy plus a seccomp denylist. Stdin, stdout, stderr and the environment pass through unchanged (plus `TMPDIR`, `TMP`, `TEMP` naming the call's private tmp directory). On success it prints nothing.

## Contract

    cs-sandbox --policy FILE [--cwd DIR] [--report-dir DIR] -- TOOL [ARGS...]
    cs-sandbox --probe --policy FILE
    cs-sandbox --policy FILE --profile cli-executor --request FILE
    cs-sandbox --profile land-check --request FILE

- `--cwd` is the workspace (default: the current directory). `TOOL` is an absolute path, a path with a slash resolved against the wrapper's directory, or a bare name found on `PATH`.
- `--report-dir`: after the tool exits, `DIR/<unix-ms>-<pid>.json` with `backend`, `tool`, `args`, `cwd`, `effective_policy` (its `tmp` is the call's tmp dir), `profile_sha256`, `narrowed`, `exit`, `signal`, `duration_ms`. A report that cannot be written is one stderr line, never a failed call.
- Tmp: each call gets a fresh `<temp_dir>/cs-sandbox-<pid>-<nanos>` (mode 0700, canonical, outside the workspace, readable and writable), removed after the tool exits on every path the wrapper survives; a failed removal is one stderr line, never a changed exit. A wrapper SIGKILLed by its runner (a timed-out call) cannot remove its dir, so every start first sweeps `<temp_dir>/cs-sandbox-<pid>-<nanos>` dirs (exactly that shape, real directories owned by the current user) whose pid no longer exists (`src/tmp.rs` `sweep_orphans`).
- `--probe` prints `{"backend","platform","fs","net":{"none","gateway","all"},"limits":{"memory": false,"processes":false,"wall_clock":"caller"}}`. Seatbelt enforces no memory or process limit; the runner's `timeout_seconds` stays the wall clock. On Linux it adds `bwrap` (path or null), `userns` (a trivial `bwrap --unshare-user --ro-bind / / true` actually ran), `nested_userns: false` and `apparmor_restrict_unprivileged_userns` (Ubuntu's knob, null where absent); backend `bwrap`, or `none` with exit 101 when bwrap is missing or cannot create a user namespace. Limits stay false: `RLIMIT_AS` is enforceable on Linux but no policy field asks for it, and `RLIMIT_NPROC` is per user.

## Exit codes

| Code | Meaning |
| --- | --- |
| tool's | The tool ran; its exit code is returned byte for byte with its output. A signal death is 128+n. |
| 2 | Usage: bad arguments, missing `--policy` or `-- TOOL`, `--cwd` or `TOOL` not found. |
| 101 | The policy cannot be enforced here; the tool did not run. Neither macOS nor Linux, no `sandbox-exec`, no `bwrap` (`/usr/bin`, `/usr/local/bin`), bwrap could not build the sandbox (no `child-pid` on its `--json-status-fd`: a blocked user namespace, a failed mount), invalid policy or descriptor, `runtime_read` with no list for this OS, a requested path escaping the workspace, `net: gateway` without `CS_GATEWAY_SOCKET`, or the Linux `.env` mask walk exceeding its depth/entry cap on one root. |

Every failure is one stderr line prefixed `cs-sandbox:`.

## Policy shapes

Host policy (`cs/policy/sandbox.json`; `$CODE_ROOT` = `CS_CODE_ROOT` or the policy file's grandparent dir, `$REPO_ROOT` = its parent, `$HOME`):

    {"version": 1, "runtime_read": {"macos": ["$CODE_ROOT/tools/bin", "$REPO_ROOT/target/release",...],
     "linux": ["$CODE_ROOT/tools/bin", "$REPO_ROOT/target/release",...]},
     "allow_net": ["none"], "write_env": {"CS_BROWSER_STATE": ["$HOME/.contextstack"]},
     "profiles": {"browser": {"read": {"macos": ["$HOME/.claude/skills/gstack",...], "linux": [...]}}},
     "sockets": {"browser-public": "$HOME/.contextstack/public-browser/sockets/public.sock"}}

`runtime_read` is one list for every OS or one list per OS (`macos`, `linux`); an OS without a list is 101. In a read list, `$TOOL_PYTHON` is the base install of `$CODE_ROOT/.venv` (`pyvenv.cfg` `home`, canonical, less a final `bin`), read-only; no venv or `home`, or a base of `/`, drops the entry (`policy.rs` `tool_python`).

Descriptor request, the optional `sandbox` object in `<TOOL>.json` beside the tool; with no such file, in the one `$CODE_ROOT/tools/bin/*.json` whose `runtime: exe` `bin` (relative to the code root's parent, else absolute) and `args` are this call's TOOL and ARGS (two such = 101). Absent = `net: none`, nothing extra; paths are workspace-relative:

    {"sandbox": {"net": "none" | "gateway" | "all", "read": [...], "write": [...], "write_env": [VAR],
                 "profile": "browser", "sockets": [NAME]}}

Effective policy: reads = `runtime_read` ∪ workspace ∪ tmp ∪ requested reads ∪ the tool binary (and its parent when that binary was outside every runtime path, for co-located siblings); writes = workspace ∪ tmp ∪ requested writes; both ∪ granted `write_env` dirs; net = the request when `allow_net` lists it, else `none`. Denial wins: a request the host does not allow is narrowed and the narrowing is written to the report; a requested path that leaves the workspace lexically or through a symlink is exit 101. Symlinks are resolved by the kernel, so a link planted in the workspace to a file outside is denied by path. Secret env files (`.env`, `.envrc`, `*.env`, `.env.*` except `.env.example`, which stays readable only inside the policy's read roots) are denied on macOS by Seatbelt pathname regexes covering `file-read*`, `file-write-unlink` (which also governs `rename(2)` sources), `file-write-create` (new files and `rename(2)` destinations), and `file-link` (hard links), preventing a tool from reading a workspace `.env` by renaming or hard-linking it to an allowed name (residual Seatbelt limit: rules match by path rather than inode, though renaming an ancestor directory preserves the `.env` basename inside it). On Linux (`src/env_deny.rs`), bwrap masks secret env files with `--ro-bind /dev/null` by walking only the writable binds (workspace, tmp, `write_env` dirs, requested writes) and the repo view (the innermost read-only bind holding the workspace, never `/`), wherever they live (a workspace under `/var` or `/opt` is walked), without following symlinks (`lstat`) and without descending into `target`, `node_modules`, `.git` or `vendor`, capped per root at 32 directory levels and 50,000 entries (exit 101 if exceeded). Runtime trees bound read-only elsewhere (`/usr`, a Python install, a browser cache) are not walked (`src/bwrap.rs` `mask_roots`). `gateway` denies all network and allows `network-outbound` to the one unix socket named by `CS_GATEWAY_SOCKET`.

`sockets`: named unix sockets, the general form of `gateway`. A requested name is granted when the host lists it (name `[a-z0-9][a-z0-9-]*`, path absolute after expansion, else 101) and the path is a live socket now; otherwise it is narrowed (`capability: "socket"`, reported) and the tool fails on its own terms. The tool finds a granted path in `CS_SOCKET_<NAME>` (`browser-public` -> `CS_SOCKET_BROWSER_PUBLIC`); the report's `effective_policy.sockets` lists them. `net` is unchanged: Seatbelt adds `network-outbound` to each socket's literal and canonical path under every `net` mode (`all` opens IP networking only, never other unix sockets); bwrap `--ro-bind`s each canonical socket at the listed path and, under `none`, uses the `gateway` seccomp list (no socket but `AF_UNIX`; the network namespace holds no other endpoint). A socket the host lists but the descriptor did not request stays unreachable. The browser broker (`cs/ops/bin/browser-broker`) serves one socket per origin set this way.

`write_env`: write targets named by variables are confined to the host's state root, `~/.contextstack`. A requested variable's directory (mkdir -p, 0700, canonical) is read- and writable only when the host lists the variable, it is set and absolute, and it lies strictly inside an allowed root and outside the workspace and code root. A target that equals an allowed root, or equals or contains the workspace, code root, home, state root, or a credential store, causes exit 101. Other unavailable grants are narrowed and reported.

On macOS, native agent profiles grant writes to the run's own TMPDIR and allow
data writes to `/dev/null` and `/dev/tty`. Claude Code also gets
`/private/tmp/claude-<uid>` for its Bash session files, and Cursor gets
`/private/tmp/.cursor` for its workspace scratch. Each grant applies to that
CLI's subtree. At render time, `getconf DARWIN_USER_TEMP_DIR` identifies the
current user's Darwin temp directory. Its parent under
`/private/var/folders/<xx>/<hash>` covers that user's `T`, `C`, and `0`
directories.

## Approved localhost actions

List each port in `request_action.needs`. For example, `["network:loopback:3000", "network:loopback:3001"]`
asks to open localhost ports 3000 and 3001 for one invocation. The approval displays these exact entries.
The action runner passes the approved ports to `cs-sandbox` in `--action-widening` as `loopback_ports`.
A request with a reserved port is refused in full. Ports 8780 (Director) and 9200 (gateway) are
always reserved. The node also reserves ports from its configured Director listeners and receiver URL,
its web client listener, and the gateway and Director environment settings. In `node.env`, set
`CS_RESERVED_LOOPBACK_PORTS` to a comma-separated list of any additional ContextStack service ports.

On macOS, Seatbelt allows connections to each listed host localhost port and lets the action bind and
accept traffic on those ports. On Linux, bubblewrap creates a fresh network namespace per invocation
with only its own loopback interface and no host routes. Every host TCP and UDP listener is unreachable,
including a dev server started by another invocation on a listed port. A server and client started inside
the same invocation can communicate on private loopback. Linux does not filter the private namespace by port.

## Named profiles

A descriptor's `"profile"` asks for a named profile; the only one is `browser`. The host grants it only by listing it under `profiles`, with the host's own read paths in the `runtime_read` shapes; an unlisted request is narrowed (`capability: "profile"`, reported) and the tool runs without it, so it fails on its own terms. An unknown name in either file is exit 101. The report's `effective_policy.profile` names the granted one. What a profile adds besides the host's paths is fixed in `src/profile.rs`, found from the failures of a real fetch, not configurable:

- `browser`, macOS: `mach-register` and `mach-lookup` for `org.chromium.Chromium.MachPortRendezvousServer.*` only (Chromium's parent and children rendezvous through it), and `file-read-data` on each ancestor directory of the call's tmp dir, nothing under them (Bun's realpath opens every directory, and browse resolves its `--out` paths). Host reads: the gstack checkout (browse runs `browse/src/server.ts` from it with its `node_modules`; Bun must list the root, or it auto-installs packages from npm into the workspace) and the Playwright browser cache.
- `browser`, Linux: `XDG_CONFIG_HOME` is set to `<tmp>/xdg-config` (the home directory is not mounted, and Chromium traps without a crashpad database); no mount beyond the host's reads (gstack, `~/.cache/ms-playwright`, `/etc/fonts`, `/var/cache/fontconfig`). bwrap's `--dev` already supplies a writable `/dev/shm`, and Playwright passes `--disable-dev-shm-usage`. D-Bus is absent; Chromium logs that and carries on.
- Network stays the descriptor's (`browser_fetch` asks for `all`).

**Chromium's own sandbox is off under this profile.** The profile sets `GSTACK_CHROMIUM_NO_SANDBOX=1`, so gstack browse launches Chromium with `--no-sandbox`. Measured: on macOS, Seatbelt refuses a second profile under ours, so every Chromium helper exits with `sandbox initialization failed: Operation not permitted` even with every operation allowed; on Linux the wrapper refuses nested user namespaces (`--disable-userns`, and Ubuntu's AppArmor refuses them too), which Chromium's namespace sandbox needs. The wrapper is then the browser's only sandbox: renderers that parse untrusted pages run with the tool's full grant (read the runtime and profile paths, write the workspace, the call's tmp and the browser state dir, `net: all`). A renderer exploit is contained by the wrapper, not by Chromium's per-renderer sandbox. Granting `browser` accepts that trade.

Seatbelt does not nest under a deny-default profile (`forbidden-sandbox-reinit`), so a tool that runs its own `sandbox-exec` must skip it when `sandbox_check(getpid, NULL, 0)` returns 1.

## Whole executor runs on Linux

Executor launches project the ordered Seatbelt file rules into bubblewrap mounts. Secret files
are mounted read-only from `/dev/null`, and denied directories use a read-only empty tmpfs.
A literal or subpath deny under a writable mount must be enforceable at launch. An absent
target that the executor could create causes exit 101. Regex file denies apply at launch only.

Executors use private IP loopback. The host forwards only explicitly declared TCP endpoints,
including the lane approval bridge and configured adapter endpoints. Hosted lead MCP uses
its declared Unix socket. Both mechanisms require Landlock ABI 9. The Director and gateway
defaults (8780 and 9200) and configured reserved listeners cannot be forwarded. Direct provider
connections require a host-side bridge with this backend. Sharing the host network would
require enforcing reserved-port restrictions across the supported kernels and transports.

## Linux (phase 3b)

`bwrap --unshare-user --disable-userns --unshare-ipc --unshare-pid --unshare-uts --unshare-cgroup-try [--unshare-net] --die-with-parent --new-session --cap-drop ALL --dev /dev --proc /proc --dir /tmp`, then the mount plan (`src/bwrap.rs`): every readable path outside the writable ones `--ro-bind` (canonical; an absent one is skipped and reported as a `read` narrowing), the workspace, tmp and granted `write_env` dirs `--bind`, parents before children, each symlink on the way to a runtime path (`/lib64`, `/bin`, Ubuntu's `/etc/resolv.conf`) recreated with `--symlink`, and the tool binary bound read-only when no read path covers it (`target/release/cs-tools`). Then `--remount-ro /`, so a write anywhere else is `EROFS`; `--chdir` the workspace; `--seccomp FD` and `--json-status-fd FD`, both inherited fds (a memfd and a pipe) set up with rustix, no `unsafe`. Net: `none`, `gateway` and `loopback` unshare the network; `gateway` also `--ro-bind`s the canonical `CS_GATEWAY_SOCKET` at the path the tool sees. An outside path is not denied but absent: a planted symlink dangles (`ENOENT`).

The seccomp denylist (`src/seccomp.rs`, compiled with `seccompiler`, `EPERM` on match) is adapted from Codex: always `ptrace`, `process_vm_*`, `io_uring_*`; `none` adds connect, bind, listen, accept, send/recv*mmsg, sockopts and any socket but `AF_UNIX`; `gateway` refuses every socket but `AF_UNIX`; `all` refuses `AF_VSOCK`. Nested user namespaces are refused (`--disable-userns`; on Ubuntu AppArmor's `bwrap-userns-restrict` refuses them too), so a tool that runs its own bwrap (`exec`) skips it when its process already carries a seccomp filter and `no_new_privs` and cannot create a user namespace. bwrap is looked up at fixed paths only: Ubuntu's AppArmor profile that admits it is attached by path.

## Attribution

The profile's allow rules derive from the Codex CLI's `seatbelt_base_policy.sbpl` and `seatbelt_read_only_platform_defaults.sbpl` (OpenAI, Apache-2.0), and the Linux seccomp list from its `linux-sandbox/src/seccomp.rs`, all vendored unchanged with their LICENSE and NOTICE under `vendor/codex/`. `scripts/check` runs fmt, clippy, release build and tests.

## Known check limitations

On macOS, a shell running under the lane sandbox failed to launch the setuid `/bin/ps -ax` with
`Operation not permitted` (errno 1, shell exit 126). The failure occurs at executable launch.
The generated native check profile's explicit `process-exec` deny targets `/bin/launchctl`.
Seatbelt refused to apply that profile from inside the lane sandbox and blocked access to the
unified log, so the specific rule behind the `ps` failure could not be confirmed.

## Land-check profile

On macOS, `cs-sandbox --profile land-check --request FILE` prints a Seatbelt
profile. Use `--request -` to read the JSON from stdin. The trusted host supplies
the paths in [the version 1 schema](land-check-request.schema.json). TMPDIR,
scratch and cache directories must resolve beneath `run_tmp`. Set
`darwin_user_temp` from `getconf DARWIN_USER_TEMP_DIR`. The renderer grants
the enclosing Darwin per-user tree. Writable roots may not overlap the main
checkout or a declared toolchain root.

`_estate_io.land_check_request` builds the payload and collects configured
reserved ports through the same helper used for approved localhost actions.
The renderer always denies outbound localhost access to ports 8780 and 9200
as well. `_estate_io.render_land_check_request` returns the emitted bytes.
Linux reports this profile as unsupported.
