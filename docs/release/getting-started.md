# Getting started with ContextStack

This page walks you from zero to your first chat on an Apple Silicon Mac. For caveats and security notes, read [known issues](alpha-known-issues.md) first.

## Before you start

- **Mac:** Apple silicon (M1 or later). Intel Macs are refused by the installer.
- **Coding agent:** Install at least one CLI and sign in before or during setup. `cs init` looks for:
  - **Claude Code** (`claude`) — sign in with `claude`, then `/login`
  - **Codex** (`codex`) — `codex login`
  - **Gemini** (`gemini`) — run `gemini`, then sign in
  - **Cursor** (`cursor-agent`) — `cursor-agent login`
  - **OpenCode**, **Pi**, **Goose**, and **Grok** are reported too; the first job uses Claude, Codex, Gemini, Cursor, Pi, Goose, or Grok when signed in
- **Disk:** At least about **2 GB** free where `~/.contextstack` will live (the installer checks this).
- **Tools:** `curl` and `shasum` (or `sha256sum`) on your PATH.

## Install

Install the latest stable release:

```sh
curl -fsSL https://github.com/contextstack/contextstack/releases/latest/download/install.sh | sh
```

To install a pre-release, including a 0.3.0 release candidate, use the `latest` channel:

```sh
curl -fsSL https://github.com/contextstack/contextstack/releases/download/channel-latest/install.sh | sh -s -- --channel latest
```

Releases are fetched from GitHub. The installer does not use `sudo` and does not send telemetry.

You should see named stages: **Checking release…**, **Downloading ContextStack** *version*, **Verifying…**, **Installing…**, then `cs init` runs automatically. The installer reports the activated release, any missing optional runtimes, and the services configured by setup. Pass `--json` to see the full activation record.

If you need a coding agent, the installer prints the next step and exits successfully. Standalone `cs init` still returns a nonzero exit status when it cannot complete the first job.

`cs` is linked under `~/.local/bin`; new shells pick it up if that directory is on your PATH.

## `cs init`

Run `cs init` yourself if you installed without the full pipe, or to retry after a partial run. Each stage prints its name and how long it took:

1. **Disk encryption** — if FileVault is off, one warning line and how to turn it on (API keys and tokens are plain files under `~/.contextstack`; `--verbose` explains).
2. **Agents** — lists each CLI as found, signed in, or not; in a terminal it can offer to start login for Claude, Codex, Cursor, or Gemini. Otherwise it prints `To sign in later, run: …` using each tool’s real command.
3. **Services** — registers this Mac as a machine of your Director, and starts the local Director, gateway and machine services.
4. **First job** — sends a one-line hello through the first signed-in agent the gateway can run; on success it saves that agent as your default.
5. **Browser** — creates a “Welcome to ContextStack” conversation and opens the built-in UI when it can.

A successful run ends with:

```text
ContextStack is ready.
Opened your first conversation in the browser.
Later: cs ui opens it again · cs lead create sets up the hosted lead · cs doctor checks for problems
```

If it can't open a browser, the middle line is instead a one-time link: `Open your first conversation: <link> (works once, expires in 5 minutes)`. Add `--verbose` to see service details, the Director's address and your release id.

No signed-in agent? Use a provider API key instead: `cs init --api-key PROVIDER` (anthropic, openai, groq, together, lambda, openrouter, gemini).

Register your project before starting a task lane: `cs repo add ~/path/to/project`. This uses its current branch; `--gate CMD` adds a landing check (default: repo commit hooks only). `cs repo list` shows local repos; the lead sees machine reports through `list_repos`.

## Your first conversation

`cs init` should leave you in the **Conversations** page of the built-in UI (loopback only). Use **New conversation**, pick a **Model**, add an optional first message, and send. Your welcome thread from init appears in the list too.

To open the UI again later:

```sh
cs ui
```

That mints a fresh pairing link (five minutes, one use) and opens your browser.

## Optional next steps

- **Desktop app** — run client-web in a native window (local Director, no pairing when already initialized). See [desktop app (development)](desktop-app.md).
- **`cs lead create`** — starts the hosted lead: a Director conversation with the shipped lead bundle, a scoped credential on disk, and usually a welcome message and reply in the browser.
- **Task lanes** — a task together with its lane (an isolated git branch and worktree); **land** the result with `cs land REPO LANE` after the check passes.
- **`cs doctor`** — problems seen so far, each with a one-line fix.
- **`cs update`** — install a newer release from the same channel as `install.sh` (stable by default).

## If something goes wrong

Run **`cs doctor`**, then **`cs logs`** on a service name (`director`, `gateway`, `node`, …) for recent output. `cs release list` shows your release id.
Read [known issues](alpha-known-issues.md) for current limitations and where to report problems (include doctor output and relevant logs).
