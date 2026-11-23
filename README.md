# ContextStack

ContextStack runs coding work through AI agents on your own machines. You can use Claude Code, Codex, Cursor, OpenCode, Antigravity, or ContextStack's harness for API and local models. Each task gets its own git branch and sandbox. A frozen check grades the result. You choose what lands.

## Status

The current release line is **0.2.0**, now in release candidates. First-time setup targets **macOS on Apple Silicon**. **Linux** support is next. Read [known issues](docs/release/alpha-known-issues.md) before you install.

## Install

Install the latest stable release:

```sh
curl -fsSL https://github.com/contextstack/contextstack/releases/latest/download/install.sh | sh
```

To install release candidates, use the `latest` channel:

```sh
curl -fsSL https://github.com/contextstack/contextstack/releases/latest/download/install.sh | sh -s -- --channel latest
```

Then run `cs init` if the installer did not already. Step-by-step instructions are in [Getting started](docs/release/getting-started.md). For a native window on macOS, see the [desktop app](docs/release/desktop-app.md).

## How it fits together

| Directory | Role |
| --- | --- |
| [`director/`](director/) | The Director: the shared record of tasks, jobs, runs, conversations and budgets, and the Client API. Machines pull work from it. |
| [`gateway/`](gateway/) | The model gateway (`cs-gateway`): one boundary for agent model calls, credentials, and spend limits. |
| [`harness/`](harness/) | The Rust runner that executes one spec/task cell and records the outcome. |
| [`sandbox/`](sandbox/) | OS confinement around each tool call (Seatbelt on macOS, bubblewrap and seccomp on Linux). |
| [`tools/`](tools/) | Core tools as one Rust binary: file read/edit, okf, and a YAML/JSON helper. |
| [`cs/`](cs/) | The `cs` command and everything that runs on each machine: the installer, the dispatcher, finishing, landing, policy. |
| [`clients/web/`](clients/web/) | The Laravel web UI for the Director: pages and rendering through the Client API only. |
| [`tests/`](tests/) | Black-box contract suites for the gateway and the Director (machine and client sides). |
| [`docs/`](docs/) | Release documentation and architecture decision records. |

Terms such as task, job, run, task lane, and land are defined in [GLOSSARY.md](GLOSSARY.md).

## Building from source

From the repository root, build the Rust workspace:

```sh
cargo build --release
```

Run the full project checks:

```sh
cs/scripts/check
```

## License

MIT. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
