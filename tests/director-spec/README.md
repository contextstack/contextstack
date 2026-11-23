# director-spec — the Director's black-box contract suites

Director.Plan step 1 (vault `runtime-modernization/`). [`NODE-PROTOCOL.md`](NODE-PROTOCOL.md) is the written
contract between a Director and its nodes; this suite checks both sides of it, from outside, the way
`tests/gateway-spec/` checks the gateways (ADR 0010). [`CLIENT-API.md`](CLIENT-API.md) is the contract between a
Director and its clients (plan step 4); the client side checks it against the Rust Director.

    tests/director-spec/run                                    # both sides: Director cases against director-rs, node cases
    tests/director-spec/run --side director                    # the Rust Director (director/), the reference
    tests/director-spec/run --side director --impl fake        # the suite's own stand-in, held to the same cases
    tests/director-spec/run --side node                        # the node client against the stand-in
    tests/director-spec/run --side node --node-client DIR      # another node client (a dir holding job-pull and spool-send)
    tests/director-spec/run --side client                      # the Client API against the Rust Director
    tests/director-spec/run --director-bin PATH | --no-build   # another binary, or target/release/cs-director as it is
    tests/director-spec/run --list | --area protocol | --case job-terminal | --json out.json | --keep | -v

Exit 0 when every selected case passes. There are no dispositions yet: both sides pass every case.

## The two sides

| Side | Under test | Driven by | Cases |
| --- | --- | --- | --- |
| director | a Director implementation | HTTP only, as a node would call it | 31 protocol-1 (auth, protocol, jobs, accept, objects, envelopes) and 17 protocol-2 (control, work, events, status, schedules), Rust only |
| node | the node client (`job-pull`, `spool-send`, `node-listen`, `_director_client.py`) | the stand-in Director (`harness/fake_director.py`, protocol 2 in `fake_director2.py`) with fault knobs | 18 protocol-1 (protocol, jobs, envelopes) and 14 protocol-2 (protocol2, work, control, events, schedules, envelopes) |
| client | the Rust Director's Client API | HTTP and its event stream, as a client would; a node registered alongside reports runs | 44: work, writes, evidence, decisions, events, access, live, control, fleet, conversation, budgets |

**Protocol 2** (NODE-PROTOCOL.md §9) is spoken by the Rust Director and the node client. Its Director cases are marked Rust-only
(`@case(..., impls=("rust",))`, shown as `[rust]` by `--list`); against `fake` they are skipped and the
run says how many. They drive `harness/node2.py`, a protocol-2 stand-in node. `work-held-30s` waits out the full
30-second hold, so `--side director --impl rust` takes about 35 s. The stand-in speaks protocol 2 for the node side
(`supported = [1, 2]`); its Director cases stay Rust-only because they act through the Client API, which the stand-in
lacks. The node client asks in 2 and falls back to 1, so the protocol-1 node cases see one refused probe per op
(`rig.calls` leaves it out). The protocol-2 node cases run `node-listen` with `"dispatch": false` (it never starts
tick). End to end against the real `director-rs`, runner and gateway mock: `docs/dev/scripts/node-protocol2-e2e.py`.

**Conversations** (CLIENT-API.md §11; plan step 6) are seven client cases in `cases/conversations.py`. A
protocol-2 stand-in node takes each turn's job, reads the generated `conversation/context.md` and
`tasks/turn/goal.md` through `GET objects`, and reports a run whose `messages.jsonl` ends with a canned reply. The
Rust target serves `--context-window dspec-tiny=600`, so a conversation on a `dspec-tiny` spec compacts after three
short turns.

The client side is Rust only: `--side client` with any other `--impl` stops with a message. The default run
(`run` with no flags) is Director cases against director-rs and node cases against the stand-in. The Laravel app's
node API, the first protocol-1 reference, was retired with Director.Plan step 8, and its target with it.

The stand-in is itself a Director target (`--impl fake`), so the double the node side trusts passes the same 31
cases as the reference. When the two disagree, the reference wins and the stand-in or the spec is corrected (that
is how `envelope-repeat-not-rejudged` came to be written down).

**Locating the node client.** `run` takes `--node-client`, else `$DIRECTOR_SPEC_NODE_CLIENT`, else the constant
`NODE_CLIENT` at the top of `run` (`cs/ops/bin`, since the node tooling moved there in plan step 0b).

**Adding a Director.** A target in `harness/targets.py` starts the Director on a temp database on a free port, and
maps the two administrative acts the cases need onto its own commands:
- register a node, printing its token and installation;
- offer that node the fixture job.

The client side needs two more acts: the owner's local credential (`local_token`), and a device paired with a
code issued on the same machine (`pair`).

**The Rust Director** (`--impl rust`, the default) runs the binary named by `--director-bin`, else `$DIRECTOR_BIN`,
else `target/release/cs-director`, which `run` builds first with `cargo build --release -p cs-director` (a no-op
when it is current; `--no-build` skips it). It serves a temp data dir on
a free loopback port; `director-rs node register` and `director-rs job create --bundle` (the fixture written to a
temp dir) are its two administrative acts.

## Safety

- **Rust target:** a temp `--data-dir` (its own SQLite, objects and local credential), a temp HOME, an environment
  of PATH alone, `127.0.0.1` only. The server is stopped when the suite ends.
- **Node side:** each case gets its own `CS_CONFIG_ROOT`, `CS_STATE_ROOT` and `CS_WORKSPACES_ROOT` under the
  suite's temp dir and a fixture token; the caller's environment (including a `CS_STATE_ROOT` pointing at the
  real ledger) is not passed on.
- No real tokens, keys or databases; no model calls; nothing listens beyond loopback. `--keep` leaves the temp
  dir (server log, node roots) for inspection; otherwise it is removed.

## Layout

| Path | What |
| --- | --- |
| `NODE-PROTOCOL.md` | the node contract, protocols 1 and 2 |
| `CLIENT-API.md` | the client contract, version 1 (with the step-5 and step-6 additions) |
| `run` | runner (`--side`, `--impl`, `--director-bin`, `--no-build`, `--node-client`) |
| `harness/wire.py` | HTTP calls, the protocol's hashes, the fixture job and run |
| `harness/fake_director.py` | the stand-in Director and its fault knobs |
| `harness/targets.py` | Director targets: `rust`, `fake` |
| `harness/node.py` | the node rig: temp roots, policy, the ops as subprocesses |
| `harness/node2.py` | the protocol-2 stand-in node the protocol-2 Director cases drive |
| `harness/fake_director2.py` | the stand-in Director's protocol 2 (control, work, events, status, schedules) |
| `harness/client.py` | client calls, the fixture bundle as objects, the event-stream reader |
| `harness/run.py` | case registry and runner |
| `cases/director.py`, `cases/node.py`, `cases/client.py` | the cases |
| `cases/node2.py` | the protocol-2 node cases (`node-listen`, cancel, events, status, schedules) |
| `cases/protocol2.py`, `cases/client_p2.py` | the protocol-2 Director cases and the step-5 client cases (Rust only) |
| `cases/conversations.py` | the step-6 client cases: conversations (Rust only) |

## What the reference changed for protocol 1

The five endpoints were characterized from the Laravel app (then `director/`, now `clients/web/`) as they stood, then three additions landed in both the
reference and the node client: the `ContextStack-Protocol` version (426 when unsupported), the installation ID
(`ContextStack-Installation`, 421 on mismatch; printed by `director:node-register`), and the run's `usage` in
every `run_finalized` envelope (filled by `finalize-run`). See NODE-PROTOCOL.md §2, §7.2.

## Not covered (and why)

- What a Director records internally (its ledger, restored run directories, last-seen times): not observable over
  the node API. The Rust Director's own tests (`director/src`, `director/tests/`) cover those.
- Grants in a bundle and their enforcement on the node: the node side is covered by `cs/tasks/regressions/estate-split`;
  the Director cases offer only the plain fixture job.
- `finalize-run` producing `usage`: an op run, not a client call; covered by `cs/tasks/regressions/finalize-run-recovery`.
- Running a pulled job end to end (tick, runner, gateway): `estate-split`.

## Open questions (for the main session)

1. **Tokens are not recorded on the runner path.** The gateway returns `usage.input_tokens/output_tokens` on every
   call (WIRE.md), but the Rust runner keeps only `est_usd`/`charged_usd` and writes no tokens to `terminal.json`,
   so `usage.input_tokens/output_tokens` are `null` for runner runs. Summing them in `harness/src/runner.rs` (and
   writing them to `terminal.json`) fills it; `finalize-run` already picks them up. Left alone here: a runner change
   under its line cap, for the runner's own lane.
2. **Subscription coding-agent runs send no envelope.** `_executor.py` writes its ledger row (with tokens) directly
   and never goes through `finalize-run`, so on a node those runs never reach the Director's outbox at all.
3. **`source` is a class and a model, not an account.** The row has no provider endpoint or account, so a Director
   cannot yet tell two pay-as-you-go sources apart (outline §11 wants per-source budgets). The gateway would have
   to name the endpoint it used.
4. **Existence oracle.** `POST envelopes` answering `missing` tells any authenticated node whether an object is
   stored, and `run_finalized` is not scoped to the node's jobs (any run id). Fine with one owner; worth closing
   before Teams.
5. **Installation ID on install:** `node-install install … --installation ID` writes it to `policy/spool.json`
   (closed; a later install without the option keeps it).
