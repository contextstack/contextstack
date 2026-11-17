# Node protocol, versions 1 and 2

The contract between a ContextStack Director and its nodes. It is language-independent: anything that answers these
requests this way is a Director, and anything that makes them this way is a node. `tests/director-spec/run` checks both
sides (README). The reference Director is the Rust Director (`director/`),
which speaks protocols 1 and 2; the Laravel app's node API, the first protocol-1 reference, was retired with
Director.Plan step 8, so nodes talk only to cs-director. The reference node client is
`cs/ops/bin/_director_client.py` with the ops `job-pull`, `spool-send` and `node-listen`; it asks in protocol 2
and falls back to 1 against a Director that speaks only 1.

Version 1 is the five endpoints as they stood on 2026-11-10 (Phase6.EstateSplit, ADR 0009) plus three additions
from Director.Plan step 1: a protocol version on every request, an installation ID, and a run's usage. Version 2
(§9, Director.Plan step 5) adds six calls to version 1 and changes none of its rules: control messages, a held-open
request for work, live run events, a status report and schedule reports. §8 says how the protocol grows.

"MUST" and "MAY" are used in their ordinary sense. JSON means UTF-8 JSON. A sha256 is 64 lowercase hex characters.

## 1. Transport and authentication

- Every call is `<base>/api/v1/nodes/me/<path>`. `<base>` is the Director's URL as the node is configured with it.
- Pull only: the node opens every connection; a Director never calls a node.
- Every request carries `Authorization: Bearer <node token>`. The token is issued once at registration; a Director
  stores only a hash of it. A token is 1–512 non-space characters.
- A missing, malformed or unknown token is `401`. Authentication is checked before anything else, so a probe without
  a token always gets `401` (operators' health checks rely on this).
- A Director records when it last heard from each node (any authenticated request) and MAY show it.

## 2. Protocol version and installation ID

**Request headers, on every request:**

| Header | Value |
| --- | --- |
| `ContextStack-Protocol` | the node's protocol version, a positive decimal integer: `1` or `2` |
| `ContextStack-Installation` | the installation ID of the Director this node belongs to |

**Response headers, on every answer from a node route, errors included:**

| Header | Value |
| --- | --- |
| `ContextStack-Installation` | this Director's installation ID |
| `ContextStack-Protocol-Supported` | the versions this Director speaks, comma-separated ascending: `1,2` from cs-director; a Director speaking only `1` is still valid |
| `ContextStack-Protocol` | the version this answer follows; present exactly when the request's version was accepted |

**Installation ID.** A string matching `[A-Za-z0-9][A-Za-z0-9._-]{0,127}`, fixed for the life of an installation
(the reference creates `inst_<ulid>` once and keeps it). Registering a node returns it beside the token; the node
keeps both in its configuration (`policy/spool.json` receiver `installation`).

**Order of checks, after authentication:**

1. `ContextStack-Protocol` absent, not a positive integer, or not one this Director speaks → `426`
   `protocol_unsupported`. Body adds `protocol: {node: <int or null>, supported: [<int>, ...]}`.
2. `ContextStack-Installation` absent or not this Director's ID → `421` `installation_mismatch`. Body adds
   `installation: <this Director's ID>`.

**The node's side.** A node acts on an answer only when:

- the answer's `ContextStack-Installation`, whenever present, equals its configured installation (else: wrong
  Director, nothing is changed, and the node says so), and
- a `2xx` answer names `ContextStack-Protocol: <its version>` (else: the Director does not speak its protocol).

A `426` is reported with the Director's supported versions; a `421` tells the operator to set the installation from
the registration output. In every one of these cases the node changes nothing and tries again on its next pass, as
for an unreachable Director.

## 3. Error answers

Every refusal is JSON `{error, code, detail}`, where `error` is the HTTP reason phrase, `code` is machine-readable
and `detail` is free text for people. Nodes MUST branch only on the status and `code`.

| Status | `code` | When |
| --- | --- | --- |
| 401 | `unauthenticated` | no valid node token (§1) |
| 404 | `not_found` | unknown route, malformed sha256 in a path, a job or object this node may not see |
| 409 | `conflict` | `accept` naming a `bundle_sha256` that is not the job's |
| 421 | `installation_mismatch` | §2 |
| 422 | `invalid_request` | a body that does not match the call (missing `bundle_sha256`; object bytes that do not hash to the name) |
| 426 | `protocol_unsupported` | §2 |

A path that matches no node route MAY be answered without the §2 headers. `POST envelopes` never uses these
statuses for an envelope's content: its answers are all `200` (§7).

## 4. `GET jobs`

This node's jobs that are **offered** or **accepted**, oldest first. Done and failed jobs are not listed.

```json
{"jobs": [{
  "job_id": "01JC…",            // 1-64 chars [A-Za-z0-9_-]
  "spec": "spec.yaml",           // bundle path of the frozen spec
  "task": "tasks/<name>",        // bundle path of the task directory (holds goal.md)
  "trials": 1,                   // positive integer
  "bundle_sha256": "…",
  "files": [{"path": "…", "sha256": "…", "bytes": 12, "mode": 420}],
  "grant": null,                 // or a grant id; then grants/<id>.yaml is in files
  "grant_sha256": null,          // sha256 of that grant file, or null
  "meta": {}                     // always an object; schedule_id, slot, … when the job came from a schedule
}]}
```

- `files` is sorted by `path`; every path is plain and relative (no `/` prefix, no `.`/`..`/empty segment, no `\`).
  `mode` is `420` (0644) or `493` (0755).
- `bundle_sha256` = sha256 of the concatenated lines `"%s:%s:%o\n" % (path, sha256, mode)` over `files` sorted by
  path (mode in octal, so `644`/`755`). The node MUST recompute it and refuse a job that does not match.
- The job's frozen spec, task and prompts are the bundle. **The dollar budget travels in the spec**: `spec.yaml`'s
  `budget: {turns, tool_calls, usd, seconds}` is part of the bundle and so of `bundle_sha256`; there is no separate
  field. `budget.usd` is the runner's per-job stop on *estimated* cost (0 turns it off). It is a safety stop, not the
  source or project budget of the Director outline §11.
- A job under a Director's project or conversation budget (CLIENT-API.md §12.3) is offered with the amount the
  Director reserved for each run written into its spec as `budget.usd`, so the bundle and its `bundle_sha256` are the
  job's own. Nothing changes for the node: it reads `budget.usd` as it always does. For the gateway to hold a run to
  it, the runner is to send what remains of it as each call's `budget.max_usd`; the Rust runner (`harness/`) does not
  yet, so today only its own estimated-cost stop applies to that amount.

## 5. `POST jobs/{job_id}/accept`

Body `{"bundle_sha256": "<the job's>"}`.

| Case | Answer |
| --- | --- |
| this node's job, matching sha | `200 {"job_id", "state"}`; `state` is `accepted` (the first time it moves offered → accepted) or the job's current state |
| repeat of an accepted job | the same `200`; nothing changes (the accept time is the first one) |
| `bundle_sha256` differs | `409 conflict`; the job is unchanged |
| `bundle_sha256` missing or not a string | `422 invalid_request` |
| unknown job, or another node's | `404 not_found` |

The node accepts only after it has the bundle, verified, and has queued the job exactly once; an accepted job is
still listed by `GET jobs` until its `job_terminal` is acked.

## 6. Objects

Content-addressed bytes, named by their sha256.

**`GET objects/{sha256}`** → `200`, `Content-Type: application/octet-stream`, the exact bytes, only when the object
is a file of one of **this node's** job bundles (in any job state). Anything else, including an object that exists
but belongs only to another node's bundles or was only uploaded, is `404`. The node MUST check the sha256 and size
against the job's file list.

**`PUT objects/{sha256}`** with the raw bytes as the body (`Content-Type: application/octet-stream`):

| Case | Answer |
| --- | --- |
| bytes hash to the name, not stored before | `200 {"sha256", "stored": true}` |
| already stored | `200 {"sha256", "stored": false}`; the stored bytes are unchanged |
| bytes do not hash to the name | `422 invalid_request`; nothing is stored |

Uploading is not scoped to a job: any authenticated node may store any object that hashes to its name. Storing
grants no read access (above).

## 7. `POST envelopes`

Body `{"envelope": {…}, "row_line": "<string>" | null}`. The answer is **always `200`** with a `status`:

| `status` | Body | Meaning | Node does |
| --- | --- | --- | --- |
| `acked` | `ack: {event_id, envelope_sha256, verified_at, restored: true}` | verified and recorded | moves the entry to `acked/`; only an ack naming its own `event_id` and `envelope_sha256` counts |
| `missing` | `missing: [<sha256>, …]` | some listed objects are not stored yet | `PUT`s exactly those (each must be one its envelope lists) and posts again |
| `rejected` | `error` | the envelope does not verify | keeps the entry; a later resend is judged afresh |
| `conflict` | `error` | the event is already acked with another `envelope_sha256` | keeps the entry; never overwritten |

`verified_at` is UTC `YYYY-MM-DDTHH:MM:SSZ`. A repeat of an acked event with the same `envelope_sha256` from the
same node returns **the identical answer** and records nothing twice. The repeat is judged on shape (step 1 below)
and `envelope_sha256` only: `row_line` and the objects are judged once, when the event settles, so a repeat with a
different `row_line` still gets the original ack. The same `event_id` acked for another node is `conflict`.

### 7.1 Common envelope fields

| Field | Rule |
| --- | --- |
| `envelope_version` | `1` |
| `event_id` | `run_finalized:<run_id>` or `job_terminal:<job_id>` |
| `files` | list of `{path, sha256, bytes}`; plain relative paths (§4), each once |
| `envelope_sha256` | sha256 of the lines `"%s:%s\n" % (path, sha256)` over `files` sorted by path (empty list → sha256 of `""`) |
| `row_sha256` | null, or the sha256 of `row_line`'s UTF-8 bytes; `row_line` must then be a JSON object |

Anything else wrong with the body (not an object, a field of the wrong type) is `rejected`. Unknown fields are kept
and ignored.

### 7.2 `run_finalized:<run_id>`

A finished run's evidence and its ledger row. `run_id` must equal the event suffix and be a plain name
(`[A-Za-z0-9][A-Za-z0-9._-]*`, no `..`). Checked in this order:

1. shape (§7.1, and `usage` below) → `rejected`;
2. every listed object stored → else `missing`;
3. every stored object still hashes to its sha256 and has its `bytes`; `envelope_sha256` matches; `row_line` hashes
   to `row_sha256` (`row_sha256` may be null: the row is then not recorded) → else `rejected`;
4. the run is restorable: `terminal.json` (a JSON object whose `run_id` is this run), `transcript.md` and
   `check.json` are listed, and `stimulus/manifest.json` is listed when any `stimulus/` file is → else `rejected`;
   a run already restored with different bytes for a listed file → `rejected`;
5. `acked`. The row is appended to the Director's run ledger once (by its `transcript`).

`row_line` is the node's ledger row, byte for byte. Today (row_version 2) it carries `exp, spec, spec_sha, task_sha,
env_sha, stimulus_sha, model, task, trial, verdict, stop, cost_usd, est_usd, est_basis, cost_class, turns,
tool_calls, seconds, transcript, ts, estate, row_version` and, when classified, `error`, `call_errors`, `sandbox`.

**Usage (required in protocol 1).** The envelope names what the run used, so a Director need not parse the row:

```json
"usage": {
  "input_tokens": 1200,          // integer >= 0, or null: not reported
  "output_tokens": 300,          // integer >= 0, or null
  "est_usd": 0.0045,             // number >= 0, or null: estimated cost (API-equivalent price of the tokens)
  "est_basis": "rate",           // how est_usd was reached (rate | metered | chars | not_invoked | …), or null
  "charged_usd": 0,              // number >= 0, or null: the actual charge; 0 for subscription and local sources
  "source": {"class": "free_local", "model": "local-coder"}   // class: api | subscription | free_local | null
}
```

All six keys are required; `source` needs `class` and `model` (a string or null). A missing or malformed `usage` is
`rejected`. The reference node fills it from the row (`est_usd`, `est_basis`, `cost_usd` → `charged_usd`,
`cost_class` → `source.class`, `model` → `source.model`) and the tokens from `terminal.json` when the runner records
`input_tokens`/`output_tokens` there (today it does not: see the README's open questions).

**Optional usage fields (additive, §8).** A node MAY add, when the agent reported them: `cache_read_tokens`,
`cache_creation_tokens`, `reasoning_tokens` (integer >= 0 or null; `input_tokens` then excludes cache reads),
`source.reported_model` (the model that actually ran; `source.model` stays the configured alias), and `allowance`
`{via, rejected, windows: [{window, used_pct, resets_at, status}]}` for a subscription that reports its allowance
(`rejected: true` = allowance reached). A Director ignores what it does not use. `est_basis: "api_list"` is the
agent's own API list price of the tokens: an estimate for comparison, never a charge.

### 7.3 `job_terminal:<job_id>`

A job's last dispatch row: the job is over on the node (done, failed, refused before it ran, or closed by a restart).
`job_id` in the envelope, when present, equals the suffix. `files` is empty, so `envelope_sha256` is the sha256 of
`""`. `row_sha256` and `row_line` are required; `row_line` is a JSON object whose `state` is `done` for success.
On a non-`done` end the node SHOULD set a short `reason` (redacted, at most 2000 characters: the executor's or
gateway's refusal, or the last lines of a traceback); a Director that speaks Client API stores it on the job as
`error`. Absent `reason`, it MAY take `stop`, then `error.detail`, then `note`.

- another node's job, or unknown → `rejected`; any `files` → `rejected`;
- `acked`: the job leaves `GET jobs` (state `done` when the row's `state` is `done`, else `error`) and the row is
  recorded once.

No `usage` here: a job's usage is the sum of its runs' `run_finalized` envelopes.

## 8. How the protocol grows

- **Every addition raises the version.** A new call, a new required field, a new answer a node must understand, or
  a changed rule is version N+1. Version 2 (§9) is the Director plan's step 5 additions, taken as one version: a
  status report, control messages (outline §4), schedule reports, the held-open request for work with its stated
  30-second answer, and the run events call.
- **A Director lists every version it speaks** in `ContextStack-Protocol-Supported` and answers each request in the
  version it names. It MAY drop old versions; a node outside its list gets `426` and the list, never a partial or
  guessed answer. A node newer than its Director gets the same `426`.
- **A node speaks one version** and checks the Director's answer names it (§2).
- **Additive, ignorable data does not need a version**: a Director ignores unknown envelope fields, a node ignores
  unknown response fields. Anything one side must act on does.
- The suite (`tests/director-spec/`) states each version's behaviour; a version is added as cases in the same change that
  implements it, on both sides.

## 9. Protocol 2

Protocol 2 is protocol 1 plus the seven calls below (Director.Plan step 5; contract outline v1.5 §3–5). A node
speaking 2 sends `ContextStack-Protocol: 2` on every request, including the five protocol-1 calls, which behave
exactly as in §4–7. A Director that speaks 2 also speaks 1 and lists `1,2`. The six calls exist only for a request
that names version 2: the same path under version 1 is `404 not_found`, so a protocol-1 node sees no change.

| Call | What |
| --- | --- |
| `GET work` | the held-open request for work: jobs and control messages, answered when they change or after 30 s (§9.2) |
| `GET control` | this node's unsettled control messages (§9.1) |
| `POST control/{control_id}` | report a control message received, applied or refused (§9.1) |
| `POST runs/{run_id}/events` | a run's live events, numbered (§9.3) |
| `POST status` | the node's status: its active runs (§9.4) |
| `POST schedules` | the node's schedules and their firings (§9.5) |
| `POST runs/{run_id}/land` | report landing a task lane (§9.6) |
| `POST conversations/{id}/lead-credential` | mint a hosted-lead credential for this conversation (§9.7) |

Refusals are as in §3, plus `409 conflict` for a control report that contradicts the recorded outcome and for run
events under a run another job or node owns. A Director records the protocol version each node last spoke and MAY
show it.

Names: a `control_id` is `ctl_` and 26 lowercase base32 characters; a `schedule_id`, a `firing_id` and a run event's
`kind` follow the rules given with them. Timestamps are UTC `YYYY-MM-DDTHH:MM:SSZ`.

### 9.1 Control messages

Anything a Director wants a node to do travels as a control message the node picks up; a Director never calls a
node. Each message has a stable `control_id` and moves through these states, which the Director records:

    requested ──> received ──> applied
         │            └──────> refused
         └──────────────────> applied | refused     (a node may report the outcome without reporting received first)

**`GET control`** → `200 {"control": [<message>, …]}`: this node's messages that are `requested` or `received`,
oldest first. A message leaves the list once it is applied or refused.

```json
{"control_id": "ctl_…", "kind": "cancel", "state": "requested", "issued_at": "…",
 "body": {"job_id": "01JC…"}}
```

| `kind` | `body` | Applied outcomes | Refused outcomes |
| --- | --- | --- | --- |
| `cancel` | `{job_id}` | `stopped`: it was running or queued and is now stopped, or was never started and never will be; `already_finished`: it was over before the message arrived | `refused` |
| `schedule_change` | `{schedule_id, expect_revision, set}`, §9.5 | `applied` | `revision_mismatch`, `unknown_schedule`, `refused` |

A node that receives a `kind` it does not know refuses it with outcome `refused`. New kinds are added with a protocol
version.

**`POST control/{control_id}`**, body one of:

```json
{"state": "received"}
{"state": "applied", "outcome": "stopped", "detail": "SIGTERM to the runner, exited 143", "result": {}}
{"state": "refused", "outcome": "refused", "detail": "the job is past the point it can be stopped"}
```

`detail` is optional text up to 4000 bytes; `result` an optional object (for `schedule_change`, `{"revision": N}`, the
schedule's revision after the change). The answer is `200 {"control": <message as recorded>}`, the message now carrying
`outcome`, `detail`, `result`, `received_at` and `settled_at`.

| Case | Answer |
| --- | --- |
| `received` on a `requested` message | `200`; the state is `received` |
| `received` on a message already received or settled | `200` with the record unchanged |
| an outcome on an unsettled message | `200`; the state is `applied` or `refused`, settled now |
| the same outcome (`state` and `outcome`) again | `200` with the identical record; nothing changes |
| a different outcome on a settled message | `409 conflict`, body adds `control: <the recorded message>`; the first outcome stands |
| an `outcome` not allowed for the kind and state, a missing `outcome`, a bad `state`, `detail` or `result` | `422 invalid_request` |
| unknown `control_id`, or another node's | `404 not_found` |

**The node's side.**
- The node records each message under its `control_id` before acting on it, and records the outcome it reached. A
  message it meets again (listed again after a lost answer, or after a restart) is never applied twice: the node
  reports the outcome it recorded.
- It checks for control messages while a run is in progress: its held-open `GET work` (§9.2), or `GET control`,
  runs beside the work, never queued behind it.
- **Cancel.** The node stops the job's run if it is running, removes it from its queue if it is queued, and never
  starts it afterwards, including a job it has not pulled yet (it keeps the cancelled `job_id`, so a later
  `GET jobs` listing it is not run). It reports `stopped`, then reports the job over as usual with `job_terminal`
  (`row_line.state` `cancelled`). A job that was already over is `already_finished`. A run that cannot be stopped is
  `refused`, with the reason in `detail`. Stopping covers both execution paths: the runner process, and a
  subscription coding agent the gateway is running for the job.

**The Director's side.** A cancel for a job whose `job_terminal` is acked while the message is still `requested`
is settled by the Director: `applied`, outcome `already_finished`, detail saying the job ended first. It leaves the
list. A message to a node that never picks it up stays `requested`; clients show it as pending.

### 9.2 `GET work`: the held-open request for work

`GET work?after=<cursor>&wait=<seconds>`, both optional. The answer:

```json
{"jobs": [ …exactly as GET jobs… ], "control": [ …exactly as GET control… ],
 "cursor": "c1f0…", "changed": true}
```

- `cursor` is an opaque string of 1–128 characters `[A-Za-z0-9._-]`, naming what this answer holds: it changes
  whenever a listed job or control message is added, removed or changed. Nodes only echo it back.
- **Without `after`**, or with an `after` that differs from the current cursor, the Director answers at once with
  `changed: true`.
- **With `after` equal to the current cursor**, it holds the request open and answers as soon as the cursor
  changes (`changed: true`), or **30 seconds** after the request arrived with `changed: false` and the same lists and
  cursor. `wait` asks for a shorter hold: an integer from 0 to 30 seconds; a larger value is treated as 30, anything
  else is `422 invalid_request`. The Director MAY answer a held request early with `changed: false` (when it shuts
  down, for example).
- The node then asks again at once with the cursor it was given. It gives the request a timeout above 30 seconds
  (45 is enough) and treats a timed-out or dropped request as an unreachable Director (§2): it waits a little and asks
  again, without `after`.
- The node handles the lists as it handles `GET jobs` and `GET control`: a job it already queued is not queued again,
  and a control message it already settled is reported again with its recorded outcome (§9.1).

This is still the node pulling: the Director never opens a connection. It lets a job start within about a second of
being offered. `GET jobs` and `GET control` stay, for a node that does not hold requests open.

### 9.3 `POST runs/{run_id}/events`: live output

A run's progress while it runs, as numbered events. `run_id` follows the rule in §7.2. Body:

```json
{"job_id": "01JC…",
 "events": [{"seq": 1, "kind": "delta", "partial": true, "at": "…", "payload": {"text": "Hel"}},
            {"seq": 2, "kind": "delta", "partial": true, "at": "…", "payload": {"text": "lo"}},
            {"seq": 3, "kind": "stop", "partial": true, "at": "…", "payload": {"reason": "end_turn"}}]}
```

| Field | Rule |
| --- | --- |
| `job_id` | the job the run belongs to: one of this node's jobs, in any state |
| `seq` | integer ≥ 1; a run's events are numbered 1, 2, 3, … with no gaps, in the order the node recorded them |
| `kind` | `[a-z][a-z0-9_.]{0,63}`; the protocol does not interpret it (the runner's are `delta`, then `stop`, as the gateway's stream chunks) |
| `partial` | boolean. `true`: output in progress, which the run's finished result replaces |
| `payload` | a JSON object of at most 64 KiB serialised |
| `at` | optional: when the node recorded it |

At most 500 events per call, sorted by `seq`. The answer is `200`:

```json
{"run_id": "…", "confirmed": 3, "accepted": 3, "duplicates": 0, "gap": null}
```

- `confirmed` is the highest `seq` such that every event 1…`confirmed` of this run is recorded.
- An event with `seq` ≤ `confirmed` is a duplicate: it is dropped (the first one recorded stands) and counted.
- An event with `seq` > `confirmed + 1` is past a gap: it and every later event in the call are not recorded, and
  `gap` is `{"expected": confirmed + 1}`.
- The node resends from `confirmed + 1`: after a gap, after a lost answer and after a reconnect. It keeps an event
  until a `confirmed` covers it. A call with `"events": []` asks for `confirmed` and records nothing.
- A body that does not match is `422 invalid_request` and records nothing. A `job_id` that is not this node's is
  `404 not_found`. A `run_id` already used by another job, or by a finished run of another node, is `409 conflict`.

A node speaking 2 adds `job_id` to a run's `run_finalized` envelope (§7.2) when the run belongs to one of its Director
jobs: an additive field (§8) a protocol-1 Director ignores, so every trial's run, not only the last, links to its job and
its charge settles there. The run's `run_finalized` envelope (§7.2) is its result: once it is acked, it replaces the partial output, which a
Director keeps as the run's live record. Events still in the node's queue when the envelope is acked MAY be sent or
dropped.

### 9.4 `POST status`

```json
{"active": [{"job_id": "01JC…", "run_id": "…", "state": "running", "started_at": "…"},
            {"job_id": "01JD…", "run_id": null, "state": "queued", "started_at": null}],
 "node_version": "cs 2026.11.10",
 "vitals": {"memory_total_bytes": 128849018880, "memory_available_bytes": 8589934592,
            "memory_pressure": "normal", "swap_total_bytes": 0, "swap_used_bytes": 0,
            "disk_free_bytes": 50000000000},
 "held": {"reason": "memory_low"},
 "provider_limits": [{"template": "cursor-sol", "message": "You've hit your usage limit",
                      "resets_at": "2026-11-17T00:00:00Z", "recorded_at": "2026-11-14T21:40:00Z"}]}
```

`repos` optionally lists `{name, path, branch}` checkouts (at most 1000); each status replaces the inventory.
The Director exposes these on `GET nodes` and `GET repos` in the Client API.

`active` is every job the node holds that has not ended: `state` `queued` or `running`, at most 1000. `run_id` and
`started_at` may be null. `node_version` is optional text up to 200 bytes.

**`vitals` (optional).** A snapshot of machine health: each field is a non-negative integer or null, and
`memory_pressure` is optional text up to 40 bytes (`normal`, `warn`, `critical`, or the node's own label). Absent
fields are omitted. The node includes `vitals` whenever it can read them.

**`held` (optional).** When present, the node is refusing new jobs (`reason` is required text up to 80 bytes, e.g.
`memory_low`, `swap_high`, `disk_low`). Running and already-queued jobs continue; the Director and clients show
`held` on `GET nodes`. When pressure clears, the node omits `held` on the next report.

**`provider_limits` (optional).** The executor templates this node has seen hit a provider's usage limit, until they
reset: a list (at most 1000) of `{template, message, resets_at, recorded_at}`. `template` is required text up to 200
bytes; `message` (up to 500), `resets_at` and `recorded_at` (up to 40) are optional text or null. Other fields are
dropped. Clients show these templates as out of quota; the node omits the field when nothing is limited.

Answer `200 {"recorded_at": "…"}`; each report replaces the last. `422 invalid_request` for a body that does not match.

The node reports when its active set changes, when `held` or `provider_limits` changes, and at least every 60 seconds while it is non-empty
or while `held` is set. A Director shows the last report with its time and when the node was last heard from (any
request, §1). A node not heard from lately is shown as **stale**, its last report as it was; never as stopped.

### 9.5 `POST schedules`

Each node holds and fires its own schedules (outline §5). It reports them, and what fired:

```json
{"schedules": [{"schedule_id": "nightly-eval", "revision": 4, "enabled": true, "missed": "skip",
                "cron": "0 2 * * *", "timezone": "America/Chicago", "spec": "specs/eval.yaml",
                "task": "tasks/nightly", "next_at": "…", "definition": {}}],
 "firings": [{"firing_id": "nightly-eval@2026-11-10T02:00", "schedule_id": "nightly-eval", "slot": "2026-11-10T02:00",
              "schedule_revision": 4, "state": "started", "recorded_at": "…", "job_id": "d-1731204000-…",
              "run_id": null, "detail": null}]}
```

**Schedules.** `schedules` is the node's complete set: a schedule the Director holds that is missing from a report is
shown as removed. At most 1000.

| Field | Rule |
| --- | --- |
| `schedule_id` | `[A-Za-z0-9][A-Za-z0-9._-]{0,127}`, unique on the node |
| `revision` | integer ≥ 1, the node's own; it rises with every change to the schedule, on the node or by a control message |
| `enabled` | boolean |
| `missed` | what happens to a firing missed while the node was off: `skip` (the default when absent) or `catch_up` |
| `cron`, `timezone`, `spec`, `task`, `next_at` | optional strings, shown as given |
| `definition` | optional object: the rest of the node's definition, shown as given |

**Firings.** Each firing has a stable `firing_id` (`[A-Za-z0-9][A-Za-z0-9._:@-]{0,199}`; the reference shape is
`<schedule_id>@<slot>`), recorded on the node before its job starts. `firings` carries firings that are new or changed
since the last report; at most 1000 per report, and a Director keeps the latest report of each `firing_id`.

| Field | Rule |
| --- | --- |
| `schedule_id`, `slot` | the schedule and the slot it fired for (a string, the node's) |
| `schedule_revision` | optional integer: the schedule's revision when it fired |
| `state` | `recorded` (identity written, job not started), `started`, `done`, `error`, `missed` (not run: skipped by `missed: skip`), `uncertain` (after a crash the node cannot tell whether the job ran; never run again blindly) |
| `recorded_at` | when the identity was recorded |
| `job_id`, `run_id`, `detail` | optional: the node's dispatch job id, its run, free text |
| `catch_up` | optional boolean: fired late under `missed: catch_up` |

Answer `200 {"schedules": <count>, "firings": <count>, "recorded_at": "…"}`, or `422 invalid_request` with nothing
recorded.

**Changing a schedule.** A client's change reaches the node as a `schedule_change` control message (§9.1):

```json
{"control_id": "ctl_…", "kind": "schedule_change", "state": "requested", "issued_at": "…",
 "body": {"schedule_id": "nightly-eval", "expect_revision": 4, "set": {"enabled": false}}}
```

`set` holds one or both of `enabled` (boolean) and `missed` (`skip` | `catch_up`). The node applies it only when the
schedule's current revision is `expect_revision`: it makes the change, raises the revision, reports `applied` with
`result: {"revision": <new>}` and sends a `POST schedules` report. A schedule at another revision is `refused` with
outcome `revision_mismatch` (so an older, delayed change never overwrites a newer one); an unknown schedule is
`unknown_schedule`. Until the node confirms, the change is pending and the old schedule may still fire.




### 9.7 `POST conversations/{conversation_id}/lead-credential`

The conversation's home node fetches a **lead credential** for a hosted-lead conversation. The token is returned only
in this answer; clients never see it. Requires protocol 2.

The Director answers only when the conversation exists, `lead` is true, and its `node` is the calling node. It mints a
lead credential bound to that conversation (the same kind and binding as `POST credentials/lead`), revokes any earlier
lead credential for that conversation, and returns the token once in 201 `{credential_id, kind: "lead", name, token,
conversation_id, intent_id, node}`.

| Status | `code` | When |
| --- | --- | --- |
| 404 | `not_found` | unknown conversation |
| 403 | `not_this_node` | the conversation belongs to another node |
| 409 | `not_a_lead` | the conversation is not a hosted lead |

### 9.6 `POST runs/{run_id}/land`: land record

After `cs land` on the machine, the node reports what happened. **Requires `Idempotency-Key`** (same rules as the
Client API, §7). Body:

```json
{"state": "landed", "repo": "contextstack", "lane": "lane/foo", "main": "main", "commit": "abc…",
 "reason": null, "landed_by": "the owner"}
```

| Field | Rule |
| --- | --- |
| `state` | `landed` \| `failed` \| `refused` |
| `repo`, `lane`, `main`, `landed_by` | non-empty strings |
| `commit` | required when `state` is `landed`; must be null otherwise |
| `reason` | required when `state` is `failed` or `refused`; must be null when `landed` |

**Answer:** `201` with the land record (CLIENT-API.md §6.4); the same key and body answers `200` with
`Idempotent-Replayed: true`. The run must belong to this node; otherwise `404 not_found`. While the run's record
is not `landed`, another report under a new key replaces it and keeps the earlier attempt on the record in
`attempts` (`state`, `reason`, `land_id`, `time`). A `landed` record refuses a further report under a different
key with `409 already_landed` and includes the existing record in `land`. A missing or invalid `Idempotency-Key`
follows the Client API refusal codes (`428`, `422`). Each recorded change emits `run.land`.

## 10. Not in this version

Registration and job creation are administrative, not node calls (the reference: `director:node-register`,
`director:node-dispatch`; `cs-director node register`, `job create`). Moving a schedule between nodes, budgets and
reservations, conversations, and replay of a run's events to a node are not in protocol 2 (§8).
