# ContextStack known issues

**Install.** Follow [Getting started](getting-started.md) for the stable and release candidate install commands. Before downloading it checks your platform, free disk space and that curl is present; then it shows named stages with a progress bar and retries, picks the bundle for your CPU, checks the published SHA-256, unpacks into `~/.contextstack`, and runs setup. Re-running upgrades or repairs; it does not use `sudo` or phone home for telemetry (`cs/scripts/install.sh`).

**`cs init`.** After install, init warns if disk encryption is off, lists which coding-agent CLIs it can see and whether they look signed in, starts the local Director and model gateway as your user agents, and runs a tiny "hello" job through the first agent that passes its checks (it will try the next signed-in agent if one fails). It never asks about paid API keys; if no agent is signed in it prints one hint (`cs init --api-key PROVIDER`) (`cs/scripts/cs_init.py`).

**Conversations in the web UI.** Two browser surfaces exist:
- **`cs ui`** opens the Director's built-in UI on loopback (pairing link, then Status and Conversations). You can start a conversation, pick a model, and read replies rendered as Markdown that update live. `cs init` opens it for you on a welcome conversation (`director/src/ui_assets/app.js`, `cs/scripts/cs_init.py`).
- **client-web** (Laravel) is richer: New conversation, model picker, streaming replies. It is not started by default; the release must include it and activation must pass `--serve-client-web` plus a local PHP 8.3+ (`cs/scripts/cs_init.py`, `clients/web/`).

**Hosted lead (`cs lead create`).** Creates a Director conversation on your machine with the shipped lead bundle, mints a scoped credential, checks the chosen model is signed in (and can prompt you through that CLI's login), sends a welcome message, prints the reply, and usually opens the conversation in your browser (`cs/scripts/cs_lead.py`).

**Task lanes.** A task with its lane: an isolated git branch and worktree. Jobs name `repo` and `lane`; your machine runs the agent in that worktree; **`cs land REPO LANE`** lands the lane's result on main after the repo's check passes (`cs/ops/bin/_lanes.py`, `cs/scripts/cs`). The hosted lead starts task lanes through **`cs mcp`** (`start_task`, etc.) when you use it from a configured lead session.

**Supported for first-time setup today: Apple Silicon Mac (macOS on `arm64`).**

- **Intel Mac:** the installer stops before downloading: "ContextStack's setup requires an Apple Silicon Mac (this Mac has an Intel processor)."
- **Linux:** the installer stops before downloading: "ContextStack's setup currently supports Apple Silicon Macs. Linux support is coming." Release tarballs exist for Linux, and `--force-platform` exists for developer test hosts only (`cs/scripts/install.sh`, `cs/scripts/_install_setup.py`).

## Security note

On macOS, agent tool calls run inside **Seatbelt** via `cs-sandbox`: deny-by-default file access with allow-lists rooted in your home directory, the workspace, policy-defined read paths, and named Unix sockets (for example the model gateway socket) (`sandbox/README.md`, `cs/policy/sandbox.json`).

That is **sandboxing on your Mac**, not a VM. A determined agent or a malicious repo may still reach data the policy allows, hit gaps we know about, or abuse your own login sessions.

Main open classes (from internal tracking; not exhaustive):

- **Secrets in git** (stash, tags, reflog) and **incomplete log redaction** for API keys and tokens.
- **Config files** agents were not meant to touch (many dotfiles and tool folders still readable).
- **Writable system temp areas** (`/private/tmp`, `/var/folders`) and some git internals.
- **Process and supervisor escapes** (detached children, some macOS spawn patterns) — accounting helps but does not fully confine a hostile process.
- **Land and review checks** still run tooling you did not write in contexts that assume you trust the lane output (`cs/ops/bin/_lanes.py`).
- **Credentials** for providers live as **plain files**, not the macOS Keychain (see below).

**Use ContextStack only on machines and repositories you trust**, with agent accounts you are willing to expose to automated tooling. Turn on **FileVault** (init checks it). Keep shared and production secrets out of the environments you use with ContextStack.

## Known rough edges

- **Two browser products** and two pairing flows (`cs ui` vs client-web Settings); easy to land in the minimal UI first.
- **client-web** is optional in the bundle; PHP and extra activation flags are on you.
- **The browser link from `cs init`** works once and expires in five minutes; run `cs ui` for a new one.
- **Task lanes and `cs land`** assume you review agent-written code; landing runs your repo's check (`cs/ops/bin/_lanes.py`).
- **Releases may be unsigned** when signing secrets are absent (see below).
- **No hosted account signup**; pairing or a local owner credential is the access model.

## Where keys live

Provider API keys and similar values go in **`~/.contextstack/credentials/<provider>`**, mode **0600**, when the gateway uses the file store (`CSGATEWAY_CREDENTIALS` empty or `file`). Init defaults activation to **`--credentials file`**, not Keychain (`gateway/src/credentials.rs`, `cs/scripts/cs_init.py`).

The machine's Director token and other install secrets are also **plain 0600 files** under `~/.contextstack` (init's FileVault warning). Encrypt the disk.

## Unsigned builds

CI signs and notarizes macOS bundles when Apple signing secrets are set. If they are missing, the bundle is marked **`signing.status: unsigned`** (`.github/scripts/sign-macos-bundle.py`).

- **`curl | sh` installs** normally do not set quarantine; Gatekeeper often stays out of the way.
- **Browser-downloaded** copies may be quarantined; macOS may show that the developer cannot be verified and block launch until you allow it in **Privacy & Security** (or remove quarantine yourself if you trust the file).
- Local check: `spctl -a -vvv -t install` on a binary may report rejection for unsigned builds.

## Landing checks

By default, **`cs land`** runs your repository's gate in a detached checkout under Seatbelt on macOS or bubblewrap on Linux. Landing stops if the sandbox cannot run. Unattended lands that touch paths in the owner probe list also stop and request owner host probes.

The owner kill switch is `"land_check": "host"` in the repo's entry in `~/.contextstack/config/repos.json` (or the file selected by `CS_REPOS_FILE`). This setting runs the gate with the owner's user privileges. An executable custom pre-commit hook requires host mode because only the ContextStack hook can skip a gate already run in confinement. `cs doctor` shows sandbox readiness, the probe list location, and each repo's effective mode.

For now, review a task's changes before landing (`git diff main...lane/<name>` in the repository), especially in repositories whose checks run arbitrary scripts.

Report a problem with logs and what you expected: [GitHub Issues](https://github.com/contextstack/contextstack/issues)

Include `cs doctor` output, relevant `cs logs` lines, and your release id (`cs init` prints it).

## Sources

- `cs/scripts/install.sh`, `cs/scripts/cs_init.py`, `cs/scripts/cs` — installation and platform gate
- `cs/scripts/cs_lead.py`, `cs/scripts/cs_mcp.py`, `cs/ops/bin/_lanes.py` — lead, MCP and task lanes
- `sandbox/README.md`, `gateway/src/credentials.rs` — confinement and credential storage
- `.github/scripts/sign-macos-bundle.py` — signing and unsigned releases
- `director/src/ui_assets/app.js`, `clients/web/` — web UIs
