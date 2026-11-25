# director-rs — the standalone Director

The ContextStack Director as its own Rust service (ADR 0012; vault `runtime-modernization/Director.Plan.md`
steps 3 to 7). It has three parts:
- the node side: node protocols 1 and 2 (`tests/director-spec/NODE-PROTOCOL.md`). Protocol 2 adds control messages, a
  held-open request for work (answered within 30 s), numbered live run events, a status report and schedule
  reports; a protocol-1 node is served exactly as before;
- access without login;
- the Client API, version 1 (`tests/director-spec/CLIENT-API.md`): submit intents, see their jobs and runs with usage,
  fetch evidence, approve or reject a run's result, request cancel and see its outcome, follow live updates and
  live run output over one event stream, see each node's status, and see and change every node's schedules; and
  hold conversations (§11): a thread of turns, each an ordinary job; and usage and budgets (§12): usage added up per
  source, project, conversation and intent, project and conversation budgets that reserve before a job is offered,
  and the gateway's source budgets seen and (by the owner) set.

It lives in `director/` (until 2026-11-11 `director-rs/`, when the Laravel app moved to `clients/web/`); the crate,
binary and launchd label keep the name `director-rs`.

## Run it

    cargo build --release -p cs-director
    target/release/cs-director serve                          # http://127.0.0.1:8780, data in ~/.contextstack/state/director
    target/release/cs-director node register laptop               # {"node", "token", "installation"}, printed once
    target/release/cs-director job create --node laptop --bundle DIR [--task tasks/x] [--trials N] [--grant ID] [--meta k=v]
    target/release/cs-director pair phone                     # a one-time pairing code for a device
    target/release/cs-director credential list | credential revoke ID
    target/release/cs-director node revoke laptop
    target/release/cs-director installation

Every command takes `--data-dir DIR` (or `DIRECTOR_DATA_DIR`). The administrative commands work on the data
directory directly, with the service running or stopped. `serve` also takes `--listen ADDR` (repeatable;
`DIRECTOR_LISTEN`), `--tls-cert PEM --tls-key PEM`, `--allow-origin ORIGIN` (repeatable;
`DIRECTOR_ALLOWED_ORIGINS`), `--stale-after SECONDS` (default 300), `--offer-grace SECONDS` (default 30; how long
an offered job may go unpulled before its job and turn show `waiting`), and the conversation compaction settings
`--context-window MODEL=TOKENS` (repeatable; `DIRECTOR_CONTEXT_WINDOWS`) and `--compact-at FRACTION` (default 0.6;
`DIRECTOR_COMPACT_AT`), and for source budgets `--gateway URL` (default `http://127.0.0.1:9200`, loopback only;
`DIRECTOR_GATEWAY`) and `--gateway-owner-token FILE` (default `~/.contextstack/config/gateway/owner-token`;
`DIRECTOR_GATEWAY_OWNER_TOKEN`), and for reservations that never settle `--reservation-stale-after SECONDS` (default
3600; `DIRECTOR_RESERVATION_STALE_AFTER`) and `--reservation-timeout SECONDS` (default 86400;
`DIRECTOR_RESERVATION_TIMEOUT`). It stops cleanly on SIGTERM or SIGINT.

A hand-made job is a bundle directory: `spec.yaml` at its root, one `tasks/<name>/goal.md` (or name the task with
`--task`), and whatever the spec refers to (`prompts/…`, `grants/<id>.yaml`). Regular files only; executables are
offered as mode 0755, everything else 0644. The node gets it on its next `job-pull`, configured with the token
and installation that `node register` printed (`policy/spool.json` receiver `url`, `token_env`, `installation`).

## Open the built-in UI

Run `cs-director ui-link` on the Director host and open the printed URL within five minutes.
On an installed host, `cs ui` opens it (`cs ui --print` prints the URL).
The embedded UI at `/ui/` shows nodes, recent jobs, conversations and their turns, and can send
messages. It needs no PHP, frontend build, or external assets. client-web remains the richer client.

Use the same `--data-dir DIR` as the service, and `--listen 127.0.0.1:PORT` when using a custom
loopback port (`DIRECTOR_LISTEN` also works). The browser stays paired for eight hours. The owner
credential stays inside the Director. UI access requires an explicitly loopback-bound listener;
remote API listeners do not serve the UI. See [ADR 0022](../docs/adr/0022-director-embedded-loopback-ui.md).

## Install it as a service

A release carries it when built with `node-bundle build --director-rs`. `node-install install BUNDLE
--serve-director-rs [--director-rs-listen ADDR]...` runs it as the launchd agent `com.contextstack.director-rs` from
`~/.contextstack/current/bin`, with its data in `~/.contextstack/state/director` (0700). That is state, not the release,
so `node-install switch` and `rollback` keep it, and `uninstall --purge` never removes it. The listen address defaults to
`127.0.0.1:8780`. Name a Tailscale address as well to serve other nodes; only loopback and Tailscale addresses are
accepted. The installer refuses `--serve-director` (the Laravel director, retired 2026-11-11). `cs status` shows the Director's
URL, installation ID and data dir modes, `cs doctor` checks them, and `cs start|stop|restart|logs director` manage it.

Registering a node takes two steps, and the token never goes on a command line:

    umask 077; ~/.contextstack/current/bin/cs-director (symlink  on older layouts) node register laptop \
        --data-dir ~/.contextstack/state/director > laptop.json                  # on the Director host
    node-install install BUNDLE --node laptop --director http://HOST:8780 --director-token-from laptop.json   # on the node
    rm laptop.json

The built-in default data directory is `~/.contextstack/state/director`, the same one the installed service uses,
so administrative commands reach the installed service without `--data-dir`. (`~/.contextstack/director` is the old
Laravel director's state directory; director-rs never uses it by default.)

`--director-token-from` reads that JSON line, or a bare token (`-` reads stdin). It checks the node name. It writes the
token to `config/node.env` (0600) and the installation ID to `config/policy/spool.json`.

## Where it keeps data

Everything is under the data directory, default `~/.contextstack/state/director`, created 0700. It refuses to start on
a directory other users can read or write.

| Path | What |
| --- | --- |
| `director.sqlite` | the database (0600, WAL): nodes with their protocol and last status, intents, jobs and their bundle files, runs with usage and files, live run events, decision receipts, cancel requests, control messages, reported schedules and firings, schedule changes, conversations with their spec bundles, turns and summaries, projects, budgets, each job's reservation (`job_budgets`), acked envelopes, client credentials, pairing codes, idempotency keys, the change log that numbers revisions, the installation ID |
| `objects/ab/<sha256>` | the content-addressed object store: bundle files and run evidence, named by their sha256, written once |
| `local-credential` | the local client token (0600) |

It starts empty. Nothing is read from or written to the Laravel app's database or the cs ledger. Only SHA-256
hashes of tokens and pairing codes are stored. The installation ID (`inst_<ulid>`) is created on first start and
never changes.

## Access

No accounts and no login (contract outline §8). Every request needs a credential, including from this machine.

| Who | Credential | Can |
| --- | --- | --- |
| a node | its node token (`csn_…`) from `node register` | the `/api/v1/nodes/me/` calls: five in protocol 1, eleven in protocol 2 |
| a client on this machine | the local credential (`csl_…`), read from `<data>/local-credential` | everything below; it is the owner |
| a paired device | a device token (`csd_…`) | everything except pairing and revoking: read and write work (submit, approve, reject, cancel), fetch evidence, follow events |

**Pairing a device.** On a trusted machine the owner issues a code, with `director-rs pair NAME` or
`POST /api/v1/client/pairings {"name"}`. The device sends it to `POST /api/v1/pair {"code"}` within 10 minutes
and gets its token once. A code works once. Without a live code the pairing endpoint answers 403 and creates
nothing; after five wrong codes every outstanding code is voided.

**Revoking.** `credential revoke ID` (or `DELETE /api/v1/client/credentials/{id}` as owner) and `node revoke NAME`.
Revoking the local credential writes a fresh one to the file at once.

**Network.** It listens on `127.0.0.1` by default. A connection from another machine is answered only when it
arrives over Tailscale (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), whose own encryption carries it, or when TLS is
configured; any other plain-HTTP connection gets 403 `transport_refused`, credentials or not. Listening on a
Tailscale address is a choice made with `--listen`; nothing is published to the internet.

**Browsers.** A request naming an `Origin` (or a cross-site `Sec-Fetch-Site`) that is not in `--allow-origin` is
refused with 403 `origin_refused`, so a web page cannot use a local credential it does not hold, nor probe the
service. No CORS headers are sent yet.

## Client API

The full contract is in `tests/director-spec/CLIENT-API.md`. Every path below is under `/api/v1/client/`.

    GET  me | nodes
    POST intents                     Idempotency-Key; {node, spec, task, files | from_job, trials?, grant?, meta?, title?} -> 201 {intent, jobs, runs}
    GET  intents[?node=]             {intents, jobs};  GET intents/{id} -> {intent, jobs, runs}
    GET  jobs[?node=] | jobs/{id} | jobs/{id}/files/{path}
    POST jobs/{id}/cancel            Idempotency-Key -> 202; a control message: requested -> received -> applied | refused, with outcome
    GET  runs[?node=] | runs/{id} | runs/{id}/files/{path}      runs carry usage, verdict, files, job_id, intent_id
    POST runs/{id}/decision          Idempotency-Key; {decision: approve|reject, note?} -> 201 receipt (authorises nothing further)
    GET  runs/{id}/decision | decisions/{id} | cancels/{id}
    PUT  objects/{sha256}            bundle bytes for an intent; GET objects/{sha256} for a listed object
    GET  runs/{id}/events[?after=]   a run's live output, numbered (also run_event on the stream)
    GET  nodes/{name}                protocol, listening, last status report; stale, never stopped
    GET  schedules[?node=] | schedules/{node}/{id} | schedule-changes/{id}
    POST schedules/{node}/{id}/changes   Idempotency-Key; {expect_revision, set: {enabled?, missed?}} -> 202, pending until the node confirms
    POST conversations               Idempotency-Key; {node, spec, files | from_job, title?, intent_id?} -> 201 {conversation}
    GET  conversations[?node=&intent=] | conversations/{id} | conversations/{id}/thread   (the whole thread, always)
    POST conversations/{id}/turns    Idempotency-Key; {message} -> 201 {turn, conversation}; queued behind the active turn
    POST conversations/{id}/stop | resume                 Idempotency-Key; stop holds the queue and requests cancel
    POST conversations/{id}/summaries/{sid}/redo          Idempotency-Key -> 202 {summary}, a new summary job
    GET  turns/{id} | summaries/{id}
    POST projects                    Idempotency-Key; {name, budget?: {usd, period?}} -> 201 {project}
    GET  projects | projects/{id}    each with its budget: none | active | reserved | exhausted, and what it shows
    PUT  projects/{id}/budget | conversations/{id}/budget     {usd, period: total|day|month}; DELETE removes it
    GET  usage?by=source|project|conversation|intent|node&period=day|month|all&date=   (each source for what it is)
    GET  source-budgets              the gateway's sources, labelled: off, set a budget to use | active | used up
    PUT  source-budgets              owner only: the gateway's limits.json, sent with its owner token
    PUT  source-budgets/{source}     owner only: {usd, period: day|month} for one source (* is shared); DELETE turns it off
    GET  events[?conversation=]      server-sent events: hello, then change {kind, id, revision, record}, and run_event
    GET  credentials | POST pairings | DELETE credentials/{id}  owner only

Write rules:
- A write is answered only once it is recorded.
- The same `Idempotency-Key` with the same content answers 200 with the same record. With different content it
  answers 409.
- Every record has a `revision` taken from one sequence for the whole installation.

Live updates: the client opens the stream and waits for `hello`, holds the events that follow, refetches its
state, then applies the held events by revision. Older events are not replayed yet.

How a client supplies the spec and task: it uploads the bundle's files as objects, then lists them
(`{path, sha256, mode}`) in the intent. Alternatively it names an earlier job (`from_job`) to offer the same
bundle again.

## Conversations

A conversation is a thread of turns, and each turn is an ordinary job (CLIENT-API.md §11; outline §7). The person
chooses the spec (a bundle, as for an intent) and the node when creating it. The spec lists
`conversation/context.md` in its `context` and `tasks/turn` in its `tasks`; for each turn the Director adds those
files to the bundle: the thread so far as `conversation/context.md` (it joins the system prompt), the message as
`tasks/turn/goal.md`, and an `exit 0` check. The node sees an ordinary job. The reply is the last model reply with
text in the run's `messages.jsonl`.

One turn runs at a time; messages from any device queue, and a queued turn's context is assembled when it becomes
eligible. Stop holds the queue and requests cancel of the active turn's job (a control message); resume releases
it. The active turn's live events reach the stream tagged with `conversation_id` and `turn_id`
(`GET events?conversation=ID` filters to one conversation).

Compaction: the whole thread is kept and readable. A turn sends the whole thread while it fits in
`window × 0.6 − instructions − 500 × tools` tokens (estimated as bytes / 4); the window comes from
`--context-window`, else a table by model prefix (`thread.rs`), else 32,768. Past that it sends a summary plus the
recent turns whole. A summary is made by an ordinary job through the normal path, recorded with the job and run that
made it, and can be redone. The modules: `conversations.rs` (the routes), `turns.rs` (the queue and starting jobs),
`settle.rs` (how turns and summaries end), `thread.rs` (context, compaction and the reply; no database).

## Usage and budgets

Every run's usage (tokens, actual charge, source) is added up per source, project, conversation, intent and node, over
a day, a month or all time (`GET usage`). Days and months, here and for budgets, are local calendar days and months on
the Director's host, the gateway's convention, so a project's day and a source's day agree; times on the wire stay UTC. Pay-as-you-go sources show dollars charged; subscriptions show the
allowance the agent last reported and whether it was reached; local sources show usage only. An estimated API price
is never shown.

A project (intents and conversations name it with `project_id`) and a conversation may each have a budget in dollars
of actual pay-as-you-go spending, on top of the gateway's source budgets. The rule (`budget.rs`): a job is given
`min(budget.usd × trials from its spec, what remains)`, or what remains when its spec states no amount; the amount
goes into the job's spec as `budget.usd` per run. With nothing left the job is created `waiting`, with a reason
(`budget_exhausted`, `budget_reserved`) and words to show, and is offered when a run settles or a budget changes. A
job settles on its runs' `charged_usd` when it ends; the difference is released. Reservations are rows
(`job_budgets`) written with the change that caused them, so a restart keeps them; at start the Director settles
what finished and offers what fits. A reservation whose run never reports does not stay held: it closes on the job's
end, a cancel outcome, its node being silent past `--reservation-stale-after`, or as a last resort
`--reservation-timeout`, counted in full (`presumed`) so nothing is freed on a guess; a late report recomputes the
charge whole and releases the difference once (CLIENT-API.md §12.3). A run whose `run_finalized` names its job
(`job_id`) is linked to it, so every trial counts. The modules: `budget.rs` (money, periods, the rule; no database),
`reservations.rs` (reserve, wait, settle, release), `scopes.rs` (budgets as set and shown), `usage.rs`, `spend.rs`
(the routes) and `gateway.rs` (the loopback-only gateway client).

Source budgets live in the gateway. Any client reads them through `GET source-budgets`; only the owner sets them,
with `PUT source-budgets` (or one source with `PUT`/`DELETE source-budgets/{source}`, a read-modify-write against the
gateway's report, compared and retried; a writer outside the Director between its last read and its write is
overwritten, since the gateway offers no revision), which the Director forwards to the gateway on this machine with the gateway's owner token,
read from its file at that moment (it must be readable by its owner only) and never logged or answered.

For example: `curl -H "Authorization: Bearer $(cat ~/.contextstack/state/director/local-credential)"
http://127.0.0.1:8780/api/v1/client/nodes`. A node that has never been heard from, or not within `--stale-after`,
is shown as stale, never as stopped. Refusals are `{error, code, detail}`, as in the node API.

## Action approval hard lines

A lane may ask for a command outside its sandbox (network, extra write paths, delete). A second model reviews the
request, except when a hard line applies: then the request goes straight to the owner (`needs_owner`, verdict
`no_go`) with the reason below, and the reviewer is never asked. The rules live in `action_approvals.rs`
(`owner_only`) and `action_approvals/hard_lines.rs`. The Git check looks at command positions in argv and inline
shell code, including commands passed to wrappers such as `find -exec`. Inline interpreter code gets a conservative
check for embedded commands.

| hard line | reason shown |
|---|---|
| Writing to a Git remote. A `git` or `gh` invocation outside the permitted list, or a push helper such as `git-send-pack`, `git-http-push` or `git-remote-*` in command position | Git push requires an owner decision (naming the form when it is not plain `push`) |
| The workspace cannot be shown to lie inside the user's home | The workspace cannot be verified as lying inside the user's home. |
| A credential, key, token or protected configuration path: a path part named like one (`*credential*`, `*token*`, `*secret*`, `.ssh`, `.aws`, `.gnupg`, `.kube`, `Keychains`, `.env*`, `*.pem`, …), or a credential store under the home | The action references a credential, key, token, or protected configuration path. |
| A path outside the user's home (system executables aside), or with `..` | The action references a path outside the user's home. |

**Git and gh are a closed list.** A `git` command skips the owner only as a listed subcommand that neither writes to
a remote nor runs a caller-chosen command (`fetch`, `pull`, `clone`, `ls-remote`, `log`, `diff`, `status`, `commit`,
`rebase`, …), without the options that would run one (`--upload-pack`, `rebase --exec`, `bisect run`,
`submodule foreach`, `grep -O`, `clone -c`/`--template`, `archive --exec`). The permitted `config` forms cover
supported reads and setting `user.name` or `user.email`. Credential config keys still require the owner. Global
`-c` settings are limited to harmless keys; `-c alias.*` is resolved (git prefers a builtin to an alias of the same
name), and a `!` shell alias, `--config-env` or `--exec-path=` waits on the owner. `gh` skips the owner only as
`pr view|list|diff|checks|status`, `issue view|list|status`, `repo view|list|clone`, `run view|list|watch|download`,
`workflow view|list`, `release view|list|download`, `label|cache list`, `search …`, `status`, and `api` as a GET
without fields (`-X`/`--method` other than GET, `-f`/`-F`/`--field`/`--raw-field`/`--input`, and `graphql` wait on
the owner). A closed list was chosen over naming bypasses: git and gh have many ways to write to a remote
(`push`, `send-pack`, `http-push`, remote helpers, aliases, `gh pr merge`, `gh repo sync`, `gh api -X POST`, …),
and a new one would pass a deny list silently, while it fails closed here (to an owner decision, not a refusal).

**Credential stores** (home-relative): the paths every agent sandbox already denies (`sandbox/src/cli_executor.rs`:
`.contextstack/credentials` and the Director, gateway, node and lead secrets, `.ssh`, `.aws`, `.gnupg`, `.config/gh`,
`.netrc`, `.zshrc.secrets`, `Library/Keychains`, `.claude/.credentials.json`, `.codex/auth.json`, the Gemini, Pi,
Goose and Grok login files), OpenCode's `.local/share/opencode/opencode.db`, `.docker/config.json`, `.kube`,
`.config/gcloud`, `.git-credentials`, `.config/git/credentials`, and the 1Password and Secretive agent directories.
Paths are compared after expanding `~`, `~user`, `$HOME` and `${HOME}`, taking relative paths from the workspace,
removing `..`, and ignoring case; symlinks are resolved by the node (`resolved_operands`) and again by the Director
for paths under the home that exist on its host. A path that contains a store (`~`, `~/.config`) counts as the
store for actions that can access its contents. Recognized direct shallow listings, such as `ls -la ~/.config`
and `find ~ -maxdepth 1 -name '*.md'`, can list containing directories. A path with a part only known at run time
(`$name`, a glob) is judged by the directory before that part.
The reviewer also checks scripts and commands assembled at run time. The sandbox still denies the stores.

## Checks

    cargo fmt --check -p cs-director && cargo clippy -p cs-director --all-targets -- -D warnings && cargo test -p cs-director
    cargo build --release -p cs-director && tests/director-spec/run --side director --impl rust
    tests/director-spec/run --side client --impl rust               # the Client API contract (CLIENT-API.md)

`tests/director-spec/cases/conversations.py` holds the conversation cases and `tests/director-spec/cases/budgets.py` the budget
cases (a stand-in gateway, `harness/fake_gateway.py`, plays the gateway; one case restarts the Director). `tests/service.rs` runs the binary on temp data dirs and loopback ports: the access model, pairing, TLS (with a
throwaway certificate when `openssl` and `curl` are present) and a hand-made job pulled and reported.

## Not built yet

A node client that speaks protocol 2 (the node tooling in `cs/`, moving to `cs/`, speaks protocol 1, so
cancels to it stay requested), a library of specs and tasks to choose from, event replay, Teams and accounts, CORS for browser clients, garbage collection of uploaded objects that no
intent used, a limit on how many live events a run may keep, a conversation's reply from the coding-agent path
(its runs carry no `messages.jsonl` turn events yet), changing a conversation's spec, a summary job whose own
input is over its model's window, the runner sending a job's reserved amount to the gateway as `budget.max_usd`
(today the runner holds a run to `budget.usd` by its own estimate only), and a node that names a run's job: the
Director reads `job_id` on `run_finalized` when present (the field name is not yet in NODE-PROTOCOL.md), and until
nodes send it, only runs the job names or that sent live events are linked to it. Each node-protocol addition raises the
protocol version (NODE-PROTOCOL.md §8).
