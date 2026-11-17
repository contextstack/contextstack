# E2: frozen Seatbelt baseline and native v1 parity

Pin 876 byte-exact profiles: 24 executor templates across six route variants,
plus two CLI adapter profiles, each in six fixed environment states. The seven
routes are native task, frozen check, auth refresh, Muse (including its identity
probe), Muse frozen check, CLI adapter (including its version probe), and lead
host. The cross product includes combinations dispatch normally does not select.

States: no configured toolchains, configured cargo, PHP, uv, explicit SSH agent
socket, and repository lane. HOME includes regex metacharacters and a space.
The owner filesystem, launcher shebang/interpreter, credentials, git layout,
PATH, temp directories, and toolchain configuration are fake. No credentials or
programs are executed. Lane git discovery and validation use fixed metadata.
Template staging models resolved login sources.

`profiles.sha256.json` preserves the frozen Python bytes from E2 steps 1–6.
The 48 retained `.sb` representatives cover every state for native-task/pi,
frozen-check/goose, auth-refresh/pi, muse/pi, muse-check/pi, lead-host/antigravity,
and both CLI adapters. No Python SBPL renderer remains.

Both baseline checks capture typed requests through the production Python
builders, materialize the fake filesystem under target/, and render using
`cs-sandbox --profile cli-executor --request -`. Every result must match its
frozen SHA-256; representatives must also match their complete fixture bytes.
Only Rust's temporary path prefix is mapped back to the virtual owner path.
Protected paths remain virtual so scratch ancestors cannot leak into rules.

Drift saves `rendered.sb` and `representative.diff` beneath target/ and prints
a unified diff against the route's representative for that environment state.
The manifest pins templates, adapters, routes, states and production dispatch
call sites. Changes require explicit baseline review; there is no freeze command.

Python retains discovery, login staging and writeback, template parsing, and
lane Git validation. Rust alone builds rules. Raw `extra_rules` is rejected for
both v1 and unversioned requests; there are no remaining callers requiring it.
Typed caller/toolchain fixtures and variant suffix fixtures independently cover
rule order and cases beyond the matrix. Historical known-mismatches.json is empty.

Run `cs/tasks/regressions/e2-sbpl-baseline/check`,
`cs/tasks/regressions/e2-native-switch/check`, and
`cargo test -p cs-sandbox --test e2_baseline` on macOS.
`CS_SANDBOX_BIN` selects the test binary (the repo check exports it).
Partial estates use `_lib/native_runtime.stage_native_runtime` to stage the
binary and policy. Transport failures fail closed before returning a command.

Review a single case with:

```
python3 cs/tasks/regressions/e2-sbpl-baseline/reference.py \
  --rust-report "$PWD/target/debug/cs-sandbox" \
  --case native-task/pi/cargo
```

`no-python-sbpl/check` rejects production Python rule literals, SBPL imports,
and profile-text appends under cs/ops/bin.
