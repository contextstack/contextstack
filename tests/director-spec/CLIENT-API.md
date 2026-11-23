# Client API, version 1

The contract between a Director and its clients (`client-web` first, then `client-tui`, `client-mobile`,
`client-desktop`). It covers Director.Plan step 4, "first journey through a client" (vault
`runtime-modernization/Director.Plan.md`; contract outline v1.5 §1 decisions 3 and 12, §6, §8, §10), and the
additions of step 5: cancel outcomes, live run output, node status and schedules (§4.3, §4.4, §5.1, §6.2, §6.3,
§8.1), of step 6: conversations (§11), and of step 7: usage and budgets (§12). None changed existing behaviour, so this
is still version 1 (§10). It sits
alongside [NODE-PROTOCOL.md](NODE-PROTOCOL.md), which covers the node side. `tests/director-spec/run --side client --impl rust`
checks it from outside. The Rust Director (`director/`) is the only
implementation.

Paths are under `/api/v1/client/` unless shown whole. Bodies are JSON unless stated. Timestamps are UTC
`YYYY-MM-DDTHH:MM:SSZ`.

## 1. Vocabulary

An **intent** is what was asked. A **job** is one assignment of work to a node. A **run** is one execution
attempt, with evidence and a verdict. An intent has jobs and a job has runs. In version 1 an intent makes exactly
one job, on the node it names. A **decision** is a receipt about one run's result. A **cancel** is a request to
stop one job. A **schedule** is held and fired by its node; each **firing** has a stable identity. A **schedule
change** is a request to change one schedule. Cancels and schedule changes reach the node as **control messages**
(NODE-PROTOCOL.md §9.1), which the node reports back on. A run's **live events** are its output while it runs.
A **conversation** is a thread of **turns**; each turn is one job, and a **summary** of older turns is one job too
(§11). A **project** groups intents and conversations and may carry a **budget**: dollars of actual pay-as-you-go
spending (§12).

## 2. Access

Every request carries `Authorization: Bearer <token>`, including requests from the same machine.

| Credential | How it is obtained | May |
| --- | --- | --- |
| local (`csl_…`) | created at install, in `<data>/local-credential` (0600) | owner access |
| device (`csd_…`) | pairing: the owner issues a one-time code, and the device sends it to `POST /api/v1/pair {"code"}` | owner access, with self-revocation refused |
| lead (`cslead_…`) | owner mints with `POST credentials/lead`, bound to a conversation and optionally an intent | the scoped lane operations below |
| node (`csn_…`) | `director-rs node register` | nothing here (401) |

The Director owns the installation and its records. The local credential and every paired device act for the
owner, including when the Director runs on a separate host. `GET me` returns `owner: true` for these credentials.
Leads receive 403 `owner_only` on owner routes. A missing, unknown or revoked token gets 401 `unauthenticated`.

- `POST pairings {"name"}` (local or device) returns 200 `{code, name, expires_at}`. The code works once, for 10 minutes.
- `GET credentials` (local or device) returns `{credentials, pairings}`. Credential rows contain
  `id, kind, name, created_at, created_by, last_used_at, revoked_at, revoked_by, conversation_id, intent_id, node`.
  A lead mint records its actor in `created_by`. Pairing rows contain
  `name, issued_by, issued_at, expires_at, used_at, credential_id`, including codes awaiting exchange.
  Revocations record the actor in `revoked_by`. Actor fields store credential IDs, or `node:<id>` for automatic
  hosted-lead rotation. Historical actor fields may be null. The list exposes no codes, tokens or hashes.
- `DELETE credentials/{id}` (local or device) returns 200 `{id, revoked: true}`. Revocation takes effect on the
  next request and ends open event streams (§8). A device deleting its own credential gets 409 `self_revoke`.
  Clients offer **Forget this device** to remove the saved credential locally.
- On the Director host, `cs device list`, `cs device pair NAME` and `cs device revoke ID` provide recovery
  using the installed Director's data directory.

### 2.1 Mint a hosted lead: `POST credentials/lead` (owner only)

```json
{"name": "hosted-lead", "conversation_id": "conv_…", "intent_id": "01…"}
```

`name` matches `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`. The conversation must exist. `intent_id` is optional
(or null); when present it must name an existing intent on the conversation's node. Invalid names or bindings
answer 422 `invalid_request`. The binding is stored with the credential and survives restart.

**Answer:** 201 `{credential_id, kind: "lead", name, token, conversation_id, intent_id, node}`.
The token is returned only by this mint response; only its hash is stored. This route takes no
`Idempotency-Key`: another successful mint creates another credential. The owner can inspect the binding in
`GET credentials` and revoke it through `DELETE credentials/{id}`. Both local and paired credentials may mint leads.


### 2.3 Lead home node: `GET lead-node`, `PUT lead-node` (local or device)

Hosted-lead conversations run on one **home machine** for task execution. Local and paired credentials may
read and change it. Leads receive 403 `owner_only` for both calls.

`GET lead-node` answers `{"node": "<name>", "source": "default" | "set"}`. The default is the first node registered on
this Director (`local_node` in `meta`); after `PUT`, `source` is `set`.

`PUT lead-node` `{"node": "<name>"}` (owner only) pins the home machine. An unknown or revoked node answers 422
`unknown_node`.

### 2.2 Lead access

A lead's estate node is the bound conversation's node. **With `intent_id`, work is limited to that intent plus
intents submitted by that particular lead credential. Without it, all jobs on that node are visible**, including
jobs created administratively. Neither form permits submitting work on another node. Lists, usage rollups and
SSE changes/live output are filtered to this scope. Reads and writes naming work outside it answer 403
`owner_only` with the existing `{error, code, detail}` shape. Bundle reuse through `from_job`, decisions, cancels,
and object reads are checked against the same scope; a shared object is readable if an allowed job/run lists it.
An explicit `files` bundle may reference only objects uploaded by this lead credential or listed by work
already in its scope. Knowing a foreign object's hash cannot copy it into a newly submitted intent. Uploading
the actual bytes through `PUT objects/{sha256}` records possession for that credential, including a deduplicated
upload whose answer is `stored: false`; that upload may then be referenced in future bundles. This permission
survives restart. Idempotency replay cannot reveal an out-of-scope record. For a lead credential, the same key with different content for an out-of-scope write answers 403 `owner_only` and names no record.

| Operation | Local owner | Device | Lead |
| --- | --- | --- | --- |
| `GET me`, `GET models`, `GET repos`, `GET nodes`, `GET nodes/{name}` | yes | yes | yes |
| Submit/list/read intents, jobs, runs; bundle/evidence files and live run events | yes | yes | scoped to node and optional intent, plus own submissions |
| Decide and read decision receipts; cancel and read cancel receipts | yes | yes | same work scope |
| `PUT objects/{sha256}` for bundles | yes | yes | yes |
| `GET objects/{sha256}` | listed objects | listed objects | listed by allowed work |
| `GET usage`, `GET events` | yes | yes | filtered to allowed work; bound conversation events also allowed |
| `GET conversations/{id}` and `/thread`; read turns/summaries | yes | yes | bound conversation only |
| `GET events?conversation=ID` | yes | yes | bound conversation only |
| Pair, list/revoke credentials, mint lead credentials | yes | yes, except self-revocation | no |
| Read/write lead node and chief of staff, rename conversations | yes | yes | no |
| Decide an action approval | yes | yes | no |
| Add job attempts | yes | yes | scoped to own intents and descendants |
| Mark owner messages or override an owner mark | yes | yes | scoped marks only, with local marks protected |
| Create/list conversations; post turns; stop/resume; redo summaries | yes | yes | no |
| Create/read projects, read source budgets, read/change schedules | yes | yes | no |
| Write project/conversation budgets | yes | yes | no |
| Set/clear conversation `attention` (§11.6) | yes | yes | bound conversation only |
| Write source budgets | yes | yes | no |
| Land lanes (`cs land`) | local owner operation; **recorded** on the node API, readable here | no write | no |

A lead cannot exchange a pairing code at `POST /api/v1/pair` using its bearer credential, even with a live code.
All refused lead operations use 403 `owner_only`. The lead ACL denies operations not explicitly allowed here.

The rules for transport and origin come before the credential check, and they apply to every route, including
the event stream:
- A plain-HTTP connection from another machine is refused with 403 `transport_refused`, unless it arrives over
  Tailscale or TLS is configured.
- A request whose `Origin` is not listed (`--allow-origin`), or which carries `Sec-Fetch-Site: cross-site`, is
  refused with 403 `origin_refused`.

Nothing is recorded for a refused request.

**Refusals** take the form `{error, code, detail}`, sometimes with extra fields. The codes are listed in §9.

## 3. Records and revisions

Every record is a flat JSON object with an integer `revision`. Revisions come from a single sequence for the
whole installation:
- Each change to any record takes the next value as that record's revision.
- A record's revision rises with each change to it.
- A record that has not changed keeps its revision.
- Revisions from different records can be compared, which is how events are ordered (§8).

| Kind | Id field | Changes when |
| --- | --- | --- |
| `intent` | `intent_id` | it is submitted |
| `job` | `job_id` | it is offered, the node accepts it, the node reports it over, a cancel is requested or moves on, or a run of it first sends live events |
| `run` | `run_id` | the node reports it, a job names it, it is decided, or its lane is landed |
| `decision` | `decision_id` | it is made (decisions do not change after that) |
| `land` | `land_id` | the node records what happened when a lane was landed (§6.4) |
| `cancel` | `cancel_id` | it is requested, and each time its control message moves on (§6.2) |
| `node` | `name` | it is registered or revoked, it first speaks another protocol version, or it sends a status report |
| `schedule` | `schedule_key` | its node reports it changed, removed, or fired; a change to it is requested or settled |
| `schedule_change` | `change_id` | it is requested, and each time its control message moves on (§6.3) |
| `conversation` | `conversation_id` | it is created, a turn is posted or moves on, a summary is made or settles, it is stopped or resumed |
| `turn` | `turn_id` | it is posted, it starts or waits for a summary, it ends |
| `summary` | `summary_id` | it is requested, it ends |
| `project` | `project_id` | it is created, its budget is set or removed, a job under it is reserved, waits, starts or settles |

Live events are not records: they carry a sequence number per run, not a revision (§5.1, §8.1).

The fields of each record:
- **intent:** `intent_id, title, node, spec, task, trials, grant, meta, bundle_sha256, created_by, created_at,
  revision, job_ids, conversation_ids` (conversations attached to it, §11), `project_id` (§12, or null),
  `lead_conversation_id` (the hosted-lead conversation that submitted this intent through `POST intents`, or null).
- **job:** `job_id, intent_id` (null for a job made by the administrative command), `node, state`
  (`waiting | offered | accepted | done | error`; `waiting`: created but held by a budget, not offered, §12.3), `spec, task, trials, grant, meta, bundle_sha256, files, created_at,
  accepted_at, finished_at, run_id, run_ids, terminal_row, error, failure` (`error` is the raw reason text; `failure` is the object below when `state` is `error`, else null), `cancel` (`{cancel_id, state, outcome}` or null),
  `live_run_ids` (runs of this job that have sent live events), `conversation_id` (the conversation whose turn or
  summary it carries, else null), `lead_conversation_id` (from its intent, §4.1), `budget` (its reservation, §12.3; null for a job made before step 7),
  `land` (full land record for this job's terminal run, or null; §6.4), `land_state` (`landed` | `failed` |
  `refused`, or null), `revision`.
- **run:** `run_id, job_id, intent_id, node, usage` (NODE-PROTOCOL.md §7.2: tokens, `est_usd`, `charged_usd`,
  `source`), `verdict, transcript, row, files, received_at, decision` (`{decision_id, decision, decided_at}` or
  null), `land` (the run's land record or null, §6.4), `events` (how many live events it sent; 0 for none),
  `conversation_id` (as for its job), `revision`.
- **decision:** see §6.1. **cancel:** see §6.2. **node:** see §4.3. **schedule:** see §4.4. **schedule_change:** see
  §6.3. **conversation, turn, summary:** see §11. **project:** see §12.1.

A job's runs are the ones its node named when it reported the job over. With `trials > 1`, only the run named in
that report is linked.

## 4. Work

### 4.1 Submit an intent: `POST intents` (Idempotency-Key)

The intent names its spec, task and node. The Director creates the job exactly as told; it chooses nothing.

```json
{"node": "laptop", "spec": "spec.yaml", "task": "tasks/hello",
 "files": [{"path": "spec.yaml", "sha256": "…", "mode": 420}, …],
 "trials": 1, "grant": null, "meta": {}, "title": "say hello"}
```

**How the client supplies the spec and task.** In version 1 the client supplies the bundle itself. A bundle is a
set of content-addressed files, the same thing a node receives (NODE-PROTOCOL.md §4). There are two ways to give
it:

1. **`files`:** a list of `{path, sha256, mode}`. `mode` is 420 (0644, the default) or 493 (0755). Paths are
   plain and relative, and each appears once. First upload each file's bytes with `PUT objects/{sha256}` (§5). An
   intent that names an object not yet stored is refused with 422 `objects_missing`, and the refusal lists the
   missing objects in `missing: [sha256…]`. Nothing is recorded, so the client uploads those objects and sends the
   same request again with the same key.
2. **`from_job`:** the id of an earlier job, whose bundle is offered again ("run it again").
   For Run again, clients send `from_job` alone as the bundle source (alongside the required node, spec and task),
   without downloading, patching or resending `files`. On both intent submission and additional-job submission,
   if `{task}/check` is absent, the Director adds it with mode 493 (0755) and these exact bytes:

   ```sh
   #!/bin/sh
   # no check: the form's check was empty, so every run passes
   exit 0
   ```

   The bytes include a final newline. The new job records `meta.check_added: "none"`, overriding that key in
   submitted job metadata. Existing files remain unchanged except for the usual spec stamp and reservation;
   an existing check is retained regardless of its content or mode, and no marker is added. The source job is
   never changed. On `POST intents` using uploaded `files`, the Director applies the same default when the
   spec's `tasks` list names the selected task. Both the intent and job record `meta.check_added: "none"`
   when a check is added (also for `from_job` intents). A spec naming no task is left alone. Uploaded checks
   retain their exact bytes and mode; only the usual spec stamp and reservation change the bundle.

Give exactly one of the two. `spec` is the bundle path of the frozen spec, and it must be in the bundle. `task` is
the bundle path of a task directory, and `{task}/goal.md` must be in the bundle. `grant`, when given, needs
`grants/{grant}.yaml` in the bundle. `trials` is a whole number from 1 to 100, default 1. `meta` is an object of
strings. `title` is at most 200 bytes. The node must be registered and not revoked.

**The spec names the intent.** The Director writes the new intent's id into the job's spec as `intent:` (the
runner refuses a spec that names no intent, ADR 0015). A client leaves the field out: it cannot know the id before
the answer. A spec that names any other value is refused with 422 `invalid_request` and `field: "intent"`; with
`from_job`, the earlier job's own stamp is replaced. The job's `bundle_sha256` is that of the bundle as offered, with
the stamp (and any reservation, §12.3); the intent's is the bundle before stamping, including any added missing check. A job made by the administrative command
(`director-rs job create`) belongs to no intent, and its spec is offered as it is.

Both edits are made in place: the stamp appends one `intent:` line (or replaces the earlier stamp's), and a
reservation sets `budget.usd` inside the budget mapping in its own style (`{…}` on one line, or block), or adds a
`budget: {usd: X}` line when there is none. Every other byte of the spec is offered as the client sent it: comments,
key order, quoting. A spec the Director cannot edit that way (a key not written as one plain `key:` line, an anchor,
alias or tag on `intent` or `budget`, a budget flow mapping over several lines or nested, a second document) is
refused with 422 `invalid_request` naming why.

The dollar budget travels in the spec (`budget.usd`), as in the node protocol. A library of named specs and tasks
that clients can choose from by reference comes later; it is not part of version 1.

**Answer:** 201 `{intent, jobs, runs}`, with the job `offered`. The node picks it up on its next `GET jobs`. A
malformed or unusable intent is refused with 422 `invalid_request`.

When the submitting credential is a hosted lead (§2), the Director sets the intent's `lead_conversation_id` to that
credential's bound `conversation_id`. The value is taken from the credential only, never from request metadata or
from provenance on `from_job`. An owner or device submission leaves `lead_conversation_id` null. Each job's
`lead_conversation_id` is the value on its intent.

Local and paired credentials can add an attempt with `POST intents/{id}/jobs`. Leads can add attempts on
their own intents and descendants within their scope.

Local and paired credentials can override a mark made with the local credential using
`PUT intents/{id}/owner-messages/{message_id}/mark`. Leads can mark messages on their own intents
and descendants within scope. A lead overriding a local mark receives 403 `owner_only`.

### 4.2 Read work

| Call | Answer |
| --- | --- |
| `GET intents[?node=NAME][&conversation=ID][&project=ID][&lanes=1][&limit=N]` | `{intents, jobs}`: intents newest first (at most 500), and the jobs of those intents. With `conversation`, only intents whose `lead_conversation_id` is that id. With `project`, only intents in that project. With `lanes=1`, only task lanes: `meta.source` is `client-web-lane` or `cs-mcp`, or `meta.lane` is set and not null. `limit` (1..500, applied after the filters) caps the intents; missing or out of range means 500. An unknown `project` is refused with 422 `invalid_request`. |
| `GET intents/{id}` | `{intent, jobs, runs}`: the intent, its jobs, and their runs with usage |
| `GET jobs[?node=NAME]`, `GET jobs/{id}` | `{jobs}`, or one job |
| `GET runs[?node=NAME]`, `GET runs/{id}` | `{runs}`, or one run, with its `usage` |
| `GET nodes`, `GET nodes/{name}` | `{nodes, stale_after_seconds}`, or one node record (§4.3) |
| `GET models` | Usable executor templates (gateway), labelled, with `default_model` and `lead` on each row (§4.5) | all |
| `GET repos` | `{repos}`: machine-reported `{node, name, path, branch}` rows; latest inventory per machine, excluding revoked machines | all |
| `GET schedules[?node=NAME]` | `{schedules}`: every node's schedules in one list (§4.4) |
| `GET schedules/{node}/{schedule_id}` | `{schedule, firings, changes}`: the schedule, its latest 100 firings, its changes |
| `GET me` | `{credential_id, owner}` |

### 4.3 Node status

A node record:

```json
{"name": "laptop", "created_at": "…", "revoked_at": null, "last_seen_at": "…", "seconds_since_seen": 4, "stale": false,
 "open_jobs": 1, "protocol": 2, "listening": true,
 "status": {"reported_at": "…", "seconds_since_report": 12, "node_version": "cs …",
            "active": [{"job_id": "…", "run_id": "…", "state": "running", "started_at": "…"}]},
 "revision": 57}
```

- `last_seen_at` is the node's last request of any kind. `stale` is true when it has never been seen, or not within
  the Director's `--stale-after` (`stale_after_seconds` in `GET nodes`). **A stale node is shown as stale, never as
  stopped**: its last status report is shown as it was, with its time.
- `protocol` is the node protocol version it last spoke (null before its first request). A node on protocol 1 cannot
  receive control messages, so cancels and schedule changes to it stay pending.
- `listening` is true while the node holds a request for work open (NODE-PROTOCOL.md §9.2).
- `status` is its last status report (NODE-PROTOCOL.md §9.4), or null. It carries `vitals`, `held` and
  `provider_limits` (`[{template, message, resets_at, recorded_at}]`: templates out of quota until `resets_at`) when
  the node sent them. `seconds_since_seen`, `stale`,
  `listening` and `seconds_since_report` are worked out when the record is read, so they can move without a new
  revision; a client showing them refetches the node now and then.


### 4.4 Schedules

Each node holds and fires its own schedules and reports them (outline §5). A schedule record:

```json
{"schedule_key": "laptop/nightly-eval", "node": "laptop", "schedule_id": "nightly-eval", "schedule_revision": 4,
 "enabled": true, "missed": "skip", "cron": "0 2 * * *", "timezone": "America/Chicago", "spec": "specs/eval.yaml",
 "task": "tasks/nightly", "next_at": "…", "definition": {}, "reported_at": "…", "removed_at": null,
 "last_firing": {"firing_id": "nightly-eval@2026-11-10T02:00", "state": "done", …}, "pending_change_ids": [],
 "revision": 61}
```

- `schedule_revision` is the node's own revision of the schedule; `revision` is the record's, as for every record.
- `missed` says what happens to a firing missed while the node was off: `skip` (the default) or `catch_up`.
- `reported_at` is when its node last reported a change to it; a report that changes nothing leaves the record
  as it was. `removed_at` is set when the node's report no longer lists it.
- A firing: `{firing_id, schedule_id, slot, schedule_revision, state, recorded_at, job_id, run_id, detail,
  catch_up, reported_at}`. `state` is `recorded | started | done | error | missed | uncertain`
  (NODE-PROTOCOL.md §9.5). Each firing has a stable `firing_id`.
- `pending_change_ids` lists the changes not yet confirmed by the node (§6.3).

### 4.5 Models

`GET models` answers `{models, default_model, gateway}`. Each model row includes `"lead": true` or `"lead": false`,
whether that gateway id can host a hosted-lead conversation (today `claude-code-sonnet`, `claude-code` and
`claude-code-opus`; see §11.1a). Each model name appears once: when the gateway has several deployments of one
name, the list keeps an available one.

## 5. Evidence

| Call | Answer |
| --- | --- |
| `GET runs/{id}/files/{path}` | a file from the run's evidence, by its listed path |
| `GET jobs/{id}/files/{path}` | a file from the job's bundle |
| `GET objects/{sha256}` | an object that some run's evidence or some job's bundle lists |
| `PUT objects/{sha256}` | stores the request's raw bytes when they hash to the name; answers `{sha256, stored}`, where `stored` is false for a repeat |

How evidence is served:
- Path segments in the URL are percent-decoded, and the decoded path must be plain.
- Evidence is never served as a page. `.json` files go out as `application/json`; text files (`.md`, `.txt`,
  `.log`, `.yaml`, `.jsonl`, `.diff`, `.patch`, `.csv`) go out as `text/plain; charset=utf-8`; anything else goes
  out as `application/octet-stream`.
- Every evidence answer carries `X-Content-Type-Options: nosniff`, `Content-Security-Policy: sandbox` and
  `ContextStack-Sha256`.
- An object that no record lists answers 404, even when it is stored.

An object upload needs no idempotency key, because its name is its content. A repeat changes nothing.

### 5.1 Live output: `GET runs/{id}/events[?after=SEQ]`

While a run is in progress its node sends its output as numbered events (NODE-PROTOCOL.md §9.3). A run that has
sent any is listed in its job's `live_run_ids` before the run itself is reported.

```json
{"run_id": "…", "job_id": "…", "node": "laptop", "confirmed": 3, "finished": false,
 "events": [{"seq": 1, "kind": "delta", "partial": true, "payload": {"text": "Hel"}, "at": "…", "received_at": "…"}, …]}
```

- `events` are those with `seq` above `after` (default 0), in order, at most 1000; ask again from the last `seq`
  for more. `confirmed` is the highest `seq` recorded; the Director records a run's events without gaps.
- `kind` and `payload` are the node's. The runner's are `delta` events (`payload.text`), then a `stop`.
- **Partial output is replaced by the result.** An event with `partial: true` is output in progress. Once the run is
  reported (its `run` record appears, `finished` is true), its evidence is the result, and a client shows that
  instead of the partial text.
- A run id with no events and no report is 404.

## 6. Decisions

### 6.1 Approve or reject a run's result: `POST runs/{id}/decision` (Idempotency-Key)

Body: `{"decision": "approve" | "reject", "note": "…"}`. The note is optional and at most 4000 bytes.

**Answer:** 201 with the receipt:

```json
{"decision_id": "dec_…", "run_id": "…", "job_id": "…", "intent_id": "…", "decision": "approve",
 "note": "…", "decided_by": "cred_…", "decided_by_name": "phone", "decided_at": "…", "revision": 42,
 "scope": "run_result", "authorises": []}
```

A decision records a judgement about one run's result, and nothing more (outline decision 12). It authorises
nothing further, which is why `authorises` is always the empty list. Any later outside action, such as applying a
patch, needs its own authorisation. Approving offers no job and changes no node.

Each run is decided once. A second decision, under a different key, is refused with 409 `already_decided`, and the
refusal includes the existing receipt in `decision`. An unknown run is 404. A word other than `approve` or
`reject` is 422.

Reading decisions: `GET runs/{id}/decision` returns the run's receipt, or 404 when the run has not been decided.
`GET decisions/{id}` returns a receipt by its own id.

### 6.4 Land records (read; written on the node API)

Landing still runs on the machine (`cs land`), but the node **reports** the outcome afterward through
`POST /api/v1/nodes/me/runs/{id}/land` (NODE-PROTOCOL.md §9.6). Clients read it on runs and jobs; list views carry
`land_state` on each job row so task-lane pages need no extra call per row.

```json
{"land_id": "lnd_…", "run_id": "…", "job_id": "…", "intent_id": "…", "state": "landed", "repo": "contextstack",
 "lane": "lane/foo", "main": "main", "commit": "abc…", "reason": null, "landed_by": "the owner", "landed_at": "…",
 "revision": 44, "attempts": []}
```

- `state` is `landed`, `failed`, or `refused`. `commit` is required when `state` is `landed` and null otherwise.
  `reason` is required when `state` is `failed` or `refused` (for example a failing check line, `conflict`, or
  `dirty main`).
- `attempts` lists earlier records for this run, oldest first. Each entry has `state`, `reason`, `land_id`, and
  `time` (when that attempt was recorded). A first record has `"attempts": []`.
- A run has one current land record. While it is `failed` or `refused`, another report under a new idempotency
  key replaces it: the new record is current, and the replaced one is appended to `attempts`. `landed` is final.
  A further report under a different key is refused with 409 `already_landed`; the refusal includes the existing
  record in `land`. The same idempotency key and body still answers the original response.
- `GET runs/{id}` and `GET jobs/{id}` include `land: null | {…}` (jobs also expose `land_state`).
- Each recorded change emits `event: run.land` with `run_id`, `job_id`, `intent_id`, and the record (§8.2).
  A `change` for the `land`, `run`, and `job` records is published too.

### 6.2 Request cancel: `POST jobs/{id}/cancel` (Idempotency-Key)

Body: `{"reason": "…"}`. The reason is optional.

**Answer:** 202 with the cancel record:

```json
{"cancel_id": "can_…", "job_id": "…", "intent_id": "…", "node": "laptop", "control_id": "ctl_…",
 "state": "requested", "outcome": null, "pending": true, "delivered": false,
 "reason": "…", "requested_by": "cred_…", "requested_by_name": "…", "requested_at": "…",
 "received_at": null, "settled_at": null, "detail": "waiting for the node to pick it up", "revision": 43}
```

The cancel travels to the node as a control message (NODE-PROTOCOL.md §9.1), and the record follows it:

| `state` | `outcome` | Meaning |
| --- | --- | --- |
| `requested` | null | recorded; the node has not picked it up. `pending` is true |
| `received` | null | the node has it (`delivered` is true from here); `pending` is still true |
| `applied` | `stopped` | the job was stopped, or will never start |
| `applied` | `already_finished` | the job was over before the cancel reached it; also set by the Director when the node reports the job over before picking the cancel up |
| `refused` | `refused` | the node could not stop it; `detail` says why |

- **Cancel is requested until the node reports an outcome.** A client shows "stopping" for `requested` and
  `received`, and the outcome once there is one. The job ends as usual when the node reports it over.
- `detail` is the node's own words once it reports, else the Director's explanation of the wait. A node that speaks
  protocol 1 cannot receive control messages: its cancels stay `requested`, and `detail` says so.
- Each change of state is a new revision of the cancel and of its job, published on the stream.

A job has at most one cancel request. Asking again, under any key, answers 200 with the existing request. A job
that is already `done` or `error` is refused with 409 `job_finished`. An unknown job is 404. `GET cancels/{id}`
reads a cancel.

### 6.3 Change a schedule: `POST schedules/{node}/{schedule_id}/changes` (Idempotency-Key)

Body: `{"expect_revision": 4, "set": {"enabled": false}}`. `set` holds one or both of `enabled` (boolean) and
`missed` (`skip` | `catch_up`). `expect_revision` is the `schedule_revision` the client saw.

**Answer:** 202 with the change record:

```json
{"change_id": "sch_…", "schedule_key": "laptop/nightly-eval", "node": "laptop", "schedule_id": "nightly-eval",
 "expect_revision": 4, "set": {"enabled": false}, "control_id": "ctl_…",
 "state": "requested", "outcome": null, "pending": true, "result_revision": null, "detail": "…",
 "requested_by": "cred_…", "requested_by_name": "…", "requested_at": "…", "received_at": null, "settled_at": null,
 "revision": 62}
```

- The change travels as a `schedule_change` control message. It **counts once the node confirms it**: `state`
  `applied` (outcome `applied`, `result_revision` the schedule's new revision), or `refused` with outcome
  `revision_mismatch`, `unknown_schedule` or `refused`.
- **A change sent to an offline node shows pending** (`pending` true, in the schedule's `pending_change_ids`) until
  the node picks it up and reports. Until then the old schedule may still fire.
- **An older change cannot overwrite a newer one.** The Director refuses at once, with 409
  `schedule_revision_mismatch` and the current `schedule`, a change whose `expect_revision` is not the schedule's
  reported revision. The node checks again when it applies it, so of two changes made from the same revision, the
  second is refused as `revision_mismatch`.
- An unknown or removed schedule is 404. A `set` with nothing in it, an unknown field, or a bad value is 422.
- `GET schedule-changes/{id}` reads a change; `GET schedules/{node}/{schedule_id}` lists them all.

## 7. Write rules

These apply to every write except an object upload and the budget writes, which replace what they name (§12): submit,
decide, cancel, schedule change, the conversation writes (create, post a turn, stop, resume, redo a summary) and
creating a project.

1. **Accepted means recorded.** A write is answered only after it has been committed. A client shows work as
   accepted only once it has that answer. If the answer is lost, the client retries with the same key.
2. **Every new write carries a client-supplied `Idempotency-Key` header.** A key is 1 to 128 characters from
   `A-Z a-z 0-9 . _ : -`; a random UUID per user action is the intended use. A missing key is refused with 428
   `idempotency_key_required`, and a malformed one with 422 `idempotency_key_invalid`.
   - **Same key and same content:** 200 with the record the key made, as it is now, and the header
     `Idempotent-Replayed: true`.
   - **Same key and different content:** 409 `idempotency_conflict`, naming that record as `{kind, id}`. This
     includes using the key for a different kind of write. For a hosted lead, when that record is outside the
     lead's scope, the answer is 403 `owner_only` with no `record` field.
   - "Content" means the operation, its target (the run or the job) and the body after normalising: defaults are
     filled in, and unknown fields are ignored. So `trials` left out and `trials: 1` are the same request.
   - Keys are shared across the whole installation, so a key used by one device and then by another names the
     same write.
   - A refused write records nothing and does not use up its key.
3. **Every record has a revision** (§3).

## 8. Live updates: `GET events`

There is one server-sent-events stream per client, and it carries changes to every kind of record. Writes go
through the ordinary requests above.

```text
retry: 2000

event: hello
data: {"revision": 41, "replay": false, "credential_id": "cred_…"}

id: 42
event: change
data: {"kind": "job", "id": "01J…", "revision": 42, "record": {…the job as it now is…}}
```

- `hello` comes first. Its `revision` is the newest revision published when the stream opened.
- Each `change` event carries one record as it is when published, or a conversation deletion signal (§11.8). The event's `id` equals the record's
  `revision`. Events arrive in rising revision order. Several changes to one record that are published together
  arrive as a single event, carrying the newest record.
- Changes made through the API are published when the request that made them is answered. Changes made by the
  Director's administrative commands are published within about a second.
- A comment line (`: ping`) arrives every 15 seconds while the stream is idle.
- `event: run_event` carries a run's live output (§8.1). It has no `id:` line.
- `event: run.land` carries a run's land record (§6.4). It has no `id:` line.
- `event: revoked` ends the stream when the credential is revoked.
- `event: resync` ends the stream when it fell too far behind.
- A hosted lead's global stream carries `change` events only for records in its work scope; `change` events for other nodes' status never arrive.
- `GET events?conversation=ID` carries only that conversation's events: `change` events whose record has that
  `conversation_id` (the conversation, its turns and summaries, their jobs and runs), `change` events for intents and
  jobs whose `lead_conversation_id` is that id, and its runs' `run_event`s. `hello` then names the `conversation`.
- **No replay yet.** A stream starts at the present. `Last-Event-ID` is ignored. Replay of missed events will be
  added when a client needs it.

**The reconnect order**, and also the order at first load:

1. Open the stream, and wait for `hello`.
2. Hold the `change` events that arrive from then on.
3. Refetch the state you show (`GET intents/{id}`, `GET jobs`, and so on). Every record in the answers carries
   its revision.
4. Apply the held events, then the live ones, by revision. Apply an event only when its `revision` is higher than
   the revision of the record you already hold with that kind and id. A record you do not hold yet is applied.

Opening the stream first closes the gap in which a change could be missed. A change committed after `hello` is
always delivered. Any change committed before `hello` is already in the refetch.

When the stream drops, ends with `resync`, or ends in any other way, start again at step 1.

### 8.1 Live output on the stream

Each live event a node sends is published once it is recorded, as:

```text
event: run_event
data: {"run_id": "…", "job_id": "…", "node": "laptop", "seq": 4, "kind": "delta", "partial": true, "payload": {"text": "lo"}, "at": "…"}
```

A run of a conversation's turn also carries `conversation_id` and `turn_id`; a run of a summary carries
`conversation_id` and `summary_id` (§11).

They arrive in `seq` order for each run. A client showing a run follows the same order as above: open the stream,
hold its `run_event`s, fetch `GET runs/{id}/events`, then apply the held and live events whose `seq` is above the
last one shown. When the run's `run` change arrives, the result replaces the partial output. A client that does not
show live output ignores `run_event` (an `EventSource` without a listener for it does).

### 8.2 Land on the stream

```text
event: run.land
data: {"run_id": "…", "job_id": "…", "intent_id": "…", "land": {…the record…}}
```

A client that shows land status listens for `run.land` the same way it listens for `run_event`: hold events
from the stream, refetch `GET runs/{id}` or `GET jobs/{id}`, then apply held and live events.

Browsers: `EventSource` cannot send an `Authorization` header, so a browser client reads the stream with
`fetch()` and a streaming body reader. Tokens never travel in URLs.

## 9. Refusal codes

| Status | Code | When |
| --- | --- | --- |
| 401 | `unauthenticated` | no client credential, or an unknown, revoked or node token |
| 403 | `owner_only` | a lead calling an owner-only or out-of-scope route |
| 403 | `origin_refused`, `transport_refused` | the browser-origin and transport rules (§2) |
| 403 | `pairing_refused` | `POST /api/v1/pair` with no matching live code |
| 404 | `not_found` | no such route or record |
| 409 | `self_revoke` | a device attempts to revoke its own credential |
| 409 | `idempotency_conflict` | the key was used for a different request |
| 409 | `already_decided` | the run's result is already decided |
| 409 | `already_landed` | the run's land record is already `landed` |
| 409 | `job_finished` | cancel of a job that is `done` or `error` |
| 409 | `summary_running` | redo of a summary that is still being made (§11.5) |
| 409 | `budgets_changed` | a per-source budget edit whose gateway budgets kept changing under it (§12.5) |
| 502 | `gateway_unavailable`, `gateway_refused`, `gateway_owner_token` | source budgets: the gateway could not be reached, refused, or its owner token could not be read (§12.5) |
| 409 | `schedule_revision_mismatch` | a schedule change whose `expect_revision` is not the schedule's reported revision (`schedule`) |
| 422 | `invalid_request` | a malformed or unusable body (`field: "intent"` when the spec names another intent, §4.1) |
| 422 | `objects_missing` | an intent names objects not stored yet (`missing`) |
| 422 | `idempotency_key_invalid` | a malformed `Idempotency-Key` |
| 428 | `idempotency_key_required` | a write without an `Idempotency-Key` |

## 10. How this API grows

Additions that change no existing behaviour stay in version 1: new routes, new fields, new kinds of record on the
stream, new event names on the stream. Clients ignore fields, record kinds and event names they do not know.

Step 5 was added this way: the cancel record's `outcome`, `node`, `control_id`, `pending`, `received_at`,
`settled_at` and its later states; the job's `live_run_ids` and the run's `events`; the `node`, `schedule` and
`schedule_change` records; `GET nodes/{name}`, `GET schedules…`, `POST schedules/{node}/{id}/changes`,
`GET schedule-changes/{id}` and `GET runs/{id}/events`; and `run_event` on the stream. One forecast in the first
text of §6.2 was corrected rather than kept: a cancel's `state` follows its control message (`received`, `applied`,
`refused`) and what happened is its `outcome` (`stopped`, `already_finished`, `refused`), where the first text
said the outcomes would be states. Only `requested` was ever served before.

Step 6 was added the same way: the `conversation`, `turn` and `summary` records and their routes (§11), the
`conversation_id` of jobs and runs, the intent's `conversation_ids`, `GET events?conversation=`, the tags on
`run_event`, and the refusal `summary_running`.

Step 7 was added the same way: the `project` record and its routes, `project_id` on intents and conversations, a
conversation's and a job's `budget`, the job state `waiting`, `GET usage`, the budget routes and source budgets (§12),
and the 502 refusals. A job is `waiting` only under a budget, which no earlier client could set.

Expected additions:
- event replay;
- a library of specs and tasks.

A change that breaks a client is published as version 2 under `/api/v2/client/`.

## 11. Conversations

A conversation is a thread of turns, where each turn builds on the ones before it (outline §7). The Director keeps
the thread and decides what earlier context goes into each turn. **Each turn is an ordinary job**: the node gets a
bundle, runs it and reports, exactly as for an intent, and does not know it is part of a conversation.

| Call | Answer |
| --- | --- |
| `POST conversations` (key) | 201 `{conversation}`; hosted-lead may omit `node` (uses `lead_node`) |
| `GET conversations[?node=NAME][&intent=ID][&lead=1][&project=ID][&limit=N]` | `{conversations}`, newest first by `created_at` (at most 500). With `lead=1`, only hosted-lead conversations (`lead` is true). With `project`, only conversations in that project. With `node` or `intent`, only that node or attached intent. `limit` (1..500, applied after the filters) caps the list; missing means 500. Invalid `limit`, `lead`, or unknown `project` is refused with 422 `invalid_request`. With no query parameters, the answer matches the previous behaviour (newest first, at most 500). |
| `GET conversations/{id}` | `{conversation}` |
| `GET conversations/{id}/thread` | `{conversation, turns, summaries}`: the whole thread, every turn and summary, always |
| `POST conversations/{id}/turns` (key) `{message}` | 201 `{turn, conversation}` |
| `POST conversations/{id}/stop` (key) | 202 `{conversation, cancel}` |
| `POST conversations/{id}/resume` (key) | 200 `{conversation, cancel}` |
| `POST conversations/{id}/summaries/{summary_id}/redo` (key) | 202 `{summary}` |
| `PUT conversations/{id}/attention` `{question, job_id?}` | 200 `{conversation}` |
| `DELETE conversations/{id}/attention` | 200 `{conversation}` |
| `GET turns/{id}`, `GET summaries/{id}` | one record |

### 11.1 Create: choose the spec

```json
{"node": "laptop", "spec": "spec.yaml", "files": [{"path": "spec.yaml", "sha256": "…", "mode": 420}, …],
 "title": "planning chat", "intent_id": null}
```

What a conversation talks to is a **spec**: instructions, tools and a model. The client supplies it as a bundle,
the same way as for an intent (§4.1): `files` (uploaded first; 422 `objects_missing` otherwise) or `from_job`.
`node` is the node its turns go to. `title` is optional, at most 200 bytes. `intent_id`, when given, **attaches** the
conversation to that intent (the intent lists it in `conversation_ids`); without it the conversation stands alone.

The spec must name the two things the Director writes for each turn, so the runner reads them the way it reads any
spec:

```yaml
model: claude-sonnet-4-5
system: prompts/system.md
context: [conversation/context.md]   # the thread so far; it joins the system prompt
tasks: [tasks/turn]                  # the turn: goal.md is the person's message
```

The bundle must not hold `conversation/context.md` or anything under `tasks/turn/`; the prompts the spec names
(`system`, the rest of `context`) must be in it. Otherwise 422 `invalid_request`. A conversation keeps its spec:
`spec_version` is the bundle's `bundle_sha256`.

**How context reaches a turn's job.** The turn's bundle is the conversation's bundle plus these generated files:

| Path | What |
| --- | --- |
| `conversation/context.md` | the thread before this turn: a summary of an older portion if one is used, then earlier turns whole, each as the person's message and the reply (a turn that was stopped or failed says so instead of a reply). On the first turn it says so |
| `conversation/decision-log.md` | a frozen snapshot of the active decisions for a hosted lead’s project, when there are any. If the Markdown exceeds 12,000 characters, the snapshot keeps whole entries in descending update order and reports how many were omitted. |
| `tasks/turn/goal.md` | the person's message, byte for byte |
| `tasks/turn/check` | `exit 0`, mode 0755: a turn has no check; its reply is its result |

The job's `meta` is empty and its `task` is `tasks/turn`. A summary's job is built the same way (§11.5).


### Built-in chat

`POST conversations` with an `Idempotency-Key` accepts
`{"spec": "chat", "model": "codex", "node": "laptop", "title": "Chat"}` without uploading a bundle.
`model` is required and must name a model in the gateway catalog; an unknown model returns 422
`invalid_request`. `title` is optional. Omitted or null `node` uses the effective lead home node:
the configured `lead_node`, else the recorded local node, else the first live node for older installs.
An explicit node may name any live node. Register a node first if none exists.

The Director supplies `spec.yaml` with `id: chat`, the selected model, `system: prompts/system.md`,
`context: [conversation/context.md]`, empty `tools` and `mcp`,
`budget: {turns: 4, tool_calls: 0, usd: 0, seconds: 300}`, `tasks: [tasks/turn]`, and `trials: 1`.
The system prompt is: “You are a helpful assistant in a conversation. The thread so far is in your context.”
It ends with a newline and provides the default conversational role; the Director supplies the thread
through the context file. The result is an ordinary conversation (`lead: false`), with the same turns,
threads and summaries as an uploaded bundle. Repeating the request with the same key replays the result.
`files` and `from_job` cannot accompany this built-in request. Custom uploaded bundles and existing
model-only requests remain supported as before.

### 11.1a Hosted lead

A client starts a hosted lead without uploading files:

```json
{"node": "laptop", "spec": "hosted-lead", "model": "claude-code-sonnet", "title": "Hosted lead"}
```

The Director builds the bundle from the installed release's `cs/lead/` folder, substituting the chosen model.
Only `claude-code-sonnet`, `claude-code` and `claude-code-opus` can lead (the Claude CLI with the lead's MCP
wired; the restricted planner, judge and patch templates cannot); any other `model` returns 422
`lead_model_unsupported` with a detail listing the models that can lead.
`CS_LEAD_BUNDLE_DIR` overrides that folder for tests. A missing folder returns 422 `lead_bundle_missing`. An uploaded bundle (`files` or `from_job`) whose spec says `id: hosted-lead` returns 422 `lead_bundle_reserved`:
only the Director builds a lead bundle.
The answer is `201` with `"lead": true` and a complete `spec_version` hash. The first turn dispatches normally.
Every conversation read includes `"lead": true` or `"lead": false`. New rows set it from the spec's
`id: hosted-lead`; older rows stored before the column are backfilled once at startup from the same rule.

### 11.2 Records

```json
{"conversation_id": "con_…", "title": "…", "node": "laptop", "lead": false, "spec": "spec.yaml", "spec_version": "…",
 "model": "claude-sonnet-4-5", "intent_id": null, "held": false, "state": "running",
 "active_turn_id": "trn_…", "queued_turn_ids": ["trn_…"], "turns": 7, "summaries": 1,
 "compaction": {"context_window": 200000, "window_source": "table", "threshold_tokens": 119000, "keep_tokens": 59500},
 "attention": null, "created_by": "cred_…", "created_at": "…", "revision": 90}
```

- `state`: `idle` (nothing to do), `running` (a turn's job is out), `summarising` (the next turn waits for a
  summary), `held` (stopped; turns queue and do not start).
- `attention` is `null` or `{"question", "job_id"?, "set_at", "set_by"}`: the hosted lead (or owner) asks for one
  decision while child lanes keep running. Unlike `held`, the queue is not stopped.
- `compaction` is the budget as it stands now (§11.4).

`GET me` includes `conversation_id` for a lead credential (the conversation it may set attention on).

A turn:

```json
{"turn_id": "trn_…", "conversation_id": "con_…", "seq": 3, "message": "…", "state": "done",
 "posted_by": "cred_…", "posted_by_name": "phone", "posted_at": "…", "job_id": "…", "run_id": "…",
 "reply": "…", "detail": null, "error": null, "cancel": null, "started_at": "…", "finished_at": "…",
 "context": {"conversation_revision": 81, "spec_version": "…", "model": "…", "context_window": 200000,
             "window_source": "table", "threshold_tokens": 119000, "tokens": 5100, "over_threshold": false,
             "summary_id": null, "summary_through_seq": null, "whole_from_seq": 1, "whole_through_seq": 2},
 "revision": 88}
```

- `state`: `queued`, `summarising` (eligible, waiting for a summary it needs), `running` (its job is offered or
  running), then `done`, `error` or `stopped`. `detail` says why for the last two.
- `failure` is the structured failure object described below when the state is `error`, otherwise null; `detail` keeps the raw reason.
- `context` records what the turn was sent, set when it starts: the **conversation revision** it was assembled from,
  the **spec version** it used, the budget, and which summary and which whole turns went in.
  When the bundle includes a decision log, `context.decision_log` records its `revision` (the latest
  decision record revision in that project) and the `entry_count` included in the bundle.
- `cancel` is its job's cancel (`{cancel_id, state, outcome}`), as on the job.

A summary:

```json
{"summary_id": "sum_…", "conversation_id": "con_…", "through_seq": 12, "based_on": null, "redoes": null,
 "job_id": "…", "run_id": "…", "state": "done", "text": "…", "detail": null, "requested_by": "director",
 "created_at": "…", "finished_at": "…", "revision": 77}
```

### 11.3 One turn at a time

- A conversation has **one active turn**. A message posted while one runs, from any device, is recorded as `queued`
  behind it. Posting is answered once the turn is recorded; when nothing runs and the conversation is not held, the
  answer already shows it `running` with its `job_id`.
- **A queued turn's context is assembled when it becomes eligible**, so it includes the reply before it. Its
  `context.conversation_revision` is later than its predecessor's.
- **The reply** is the run's result: the text of the last model reply with text in the run's `messages.jsonl`
  (the runner's record of each turn, `{"event": "turn", "text": …}`), taken when the node reports the job over and
  its run is recorded. A turn whose run carries none is `error` ("the run reported no reply").
- **Replies appear as they are written.** While a turn runs, its run's live events (§5.1, §8.1) reach the stream
  as `run_event`s tagged with `conversation_id` and `turn_id`. When the turn ends, its `turn` change carries
  `reply`, which **replaces** the partial text.
- **Stop** (`POST …/stop`, body ignored) holds the queue at once (`held` true) and requests cancel of the active
  turn's job (§6.2; a control message). The answer's `cancel` is that request: `requested` until the node reports,
  as for any cancel. When the job ends, the turn is `stopped` if the cancel stopped it; if the job finished first,
  the turn ends as it would have. A turn waiting for a summary goes back to `queued`, and the summary's job is
  cancelled. Stopping again answers the same way and requests nothing new.
- **Resume** (`POST …/resume`) releases the queue: the next queued turn is assembled and starts. Resuming a
  conversation that is not held changes nothing.
- A cancel requested on a turn's job directly (§6.2) stops that turn without holding the queue.
- A turn whose job cannot be offered (its node was revoked, say) ends as `error`, and the queue moves on.

### 11.4 Compaction keeps the whole thread

The whole thread is always kept: `GET conversations/{id}/thread` returns every turn, message and reply, and every
summary, however long the conversation. What compaction changes is only what a turn **sends**.

- **Size.** Tokens are estimated as one per four bytes of text.
- **Threshold.** `threshold_tokens = context_window × 0.6 − instructions − 500 × tools`, at least 256.
  `instructions` is the estimated size of the spec's prompt files; 500 tokens are set aside for each tool's
  definition; the remaining 40% of the window is left for the reply, the tool results and the estimate's error.
  `0.6` is a stake in the ground, set with `serve --compact-at`.
- **Context window, per model.** `serve --context-window MODEL=TOKENS` (repeatable) first; else a built-in table by
  model-name prefix (`claude-` 200,000; `gpt-` 128,000; `codex` 200,000; `gemini` 1,000,000; `z-ai/glm` 128,000;
  the local `local-coder`, `local-reasoner`, `qwen3-coder`, `muse-code` 32,768); else 32,768.
  `window_source` says which (`setting`, `table`, `default`). The gateway's `/v1/models` does not report context
  sizes today; when it does, it can feed the same setting.
- **Until the threshold**, a turn sends the whole thread: `context.md` plus the message are within
  `threshold_tokens`.
- **Past it**, the turn sends the newest done summary (the one covering the most turns, newest first) plus every
  turn after it whole, when that fits.
- **When that does not fit either**, the Director identifies an older portion to summarise: everything but the
  most recent turns that fit within `keep_tokens` (half the threshold). It makes the summary with an ordinary job,
  and the turn waits (`summarising`). The summary covers turns 1 to `through_seq`. It is built from the previous
  summary (`based_on`) plus the turns after it whole, so each summary job sees a bounded portion. When the summary
  is done, the turn is assembled again and starts.
- **Nothing is dropped silently.** When no smaller form exists, because the portion's summary failed or there is
  nothing older left to summarise, the turn sends the newest summary and every later turn whole, over the
  threshold, and says so (`context.over_threshold`). A failed portion is not tried again on its own; redo it
  (§11.5).

### 11.5 Summaries are ordinary jobs

- A summary's job is the conversation's bundle with `conversation/context.md` holding the portion (its base summary
  and turns whole) and `tasks/turn/goal.md` asking for a summary of turns 1 to `through_seq`. It goes to the
  conversation's node through the normal path. The Director calls no model itself.
- Its text is its run's reply (§11.3). The summary records the `job_id` and `run_id` that produced it, so a bad
  summary can be traced, and its run's evidence fetched (§5).
- **Redo** (`POST conversations/{id}/summaries/{summary_id}/redo`) makes the same portion again with a new job:
  a new summary with `redoes` naming the old one. Once it is done it is the newest summary of that portion, so
  later turns use it. Turns already sent keep their record of the summary they used. Redo of a summary still being
  made is 409 `summary_running`; an unknown summary, or one of another conversation, is 404.
- While any summary of a conversation is being made, its next turn waits for it.

### 11.6 Writes

Every conversation write carries an `Idempotency-Key` and follows §7. Content is: for create, the normalised
body; for a turn, the conversation and the message; for stop, resume and redo, the operation and its target. So the
same key used to stop and then to resume is 409 `idempotency_conflict`. An unknown conversation is 404; an empty
message, or one over 200,000 bytes, is 422. A hosted lead may `POST conversations/{id}/wake-turns` with
`{nudge, reply, jobs?}`; every `jobs[]` id must be a job the lead may see, or the answer is 422 `invalid_request`
naming that id. A completed wake reply is ordered after started and terminal turns and ahead of pending
person turns, so it appears in their context when they start. Pending turns retain their relative order and
turn IDs, and their sequence numbers can change when a wake reply is recorded.

### 11.7 Attention (needs-you)

A hosted lead escalates without stopping the queue: `PUT conversations/{id}/attention` with `question` (required,
non-empty, at most 4000 bytes) and optional `job_id` (at most 128 bytes). The answer is `{conversation}` with
`attention` set. `DELETE conversations/{id}/attention` clears it to `null`; both writes touch the conversation and
emit a `conversation` change on the event stream (§8). Any client credential may set or clear attention except a
**lead**, which may do so only on its bound conversation (§2.2). Posting an owner or device turn (`POST …/turns`)
clears attention in the same transaction as the queued turn, so every client sees it was answered. Attention writes
need no `Idempotency-Key`.

### 11.8 Archive and deletion

`POST conversations/{id}/archive` and `POST conversations/{id}/unarchive` require an
`Idempotency-Key` and answer 200 `{conversation}`. `archived_at` is null or the archive
time. A change of archive state bumps the revision and emits the usual change event;
repeating the current state leaves the record unchanged. Archive preserves lead credentials.
Archived conversations and their threads remain readable. Posting turns or wake-turns
while archived returns 409 `archived`: unarchive first.

`GET conversations` excludes archived records by default. `archived=1` selects only
archived records; `archived=all` selects both. Other values return 422 `invalid_request`.
Archive selection combines with node, intent, lead, project and limit in SQL before limiting.

`DELETE conversations/{id}` requires an `Idempotency-Key` and owner authority (lead
credentials receive 403 `owner_only`). It returns 200 `{"deleted":"<id>"}`;
a subsequent delete, including with the same key, returns 404. Any turn outside done,
error or stopped refuses deletion with 409 `busy`: stop it first. Deletion atomically
removes the conversation, files, turns, summaries, attention and conversation budget,
revokes bound lead credentials, and clears conversation links on retained intents and jobs.

Deletion emits a `change` with `kind: "conversation"`, `id`, `revision`, and `deleted: true`,
without a `record` body, including on `events?conversation=ID`. Remove that id from open
pages. Streams still start at the present and do not replay: after a disconnect or resync,
refetch lists (which omit deleted records); a deleted conversation or thread returns 404.

## 12. Usage and budgets

Every run records its usage (§3, NODE-PROTOCOL.md §7.2). The Director adds it up, and holds budgets in dollars of
actual pay-as-you-go spending (outline §11):

- a **source budget** is the gateway's, per pay-as-you-go source; without one the source is off (§12.5);
- a **project** or **conversation budget** is the Director's, optional, a further limit on top (§12.2).

| Call | Answer |
| --- | --- |
| `POST projects` (key) `{name, budget?}` | 201 `{project}` |
| `GET projects`, `GET projects/{id}` | `{projects}`, `{project}` |
| `PUT projects/{id}/budget` `{usd, period?}`, `DELETE projects/{id}/budget` | `{project}` |
| `PUT conversations/{id}/budget` `{usd, period?}`, `DELETE conversations/{id}/budget` | `{conversation}` |
| `GET usage?by=&period=&date=&project=&conversation=&intent=&node=` | §12.4 |
| `GET source-budgets` | `{gateway, budgets}` (§12.5) |
| `PUT source-budgets` (owner only) | `{gateway, budgets}` (§12.5) |
| `PUT source-budgets/{source}` `{usd, period}`, `DELETE source-budgets/{source}` (owner only) | `{gateway, budgets}` (§12.5) |

### 12.1 Projects

```json
{"project_id": "prj_…", "name": "site rebuild", "created_by": "cred_…", "created_at": "…",
 "budget": {"state": "active", "shown": "$3.75 of $5.00 left", "usd": 5.0, "period": "total", "spent_usd": 1.25,
            "reserved_usd": 0.0, "remaining_usd": 3.75, "resets_at": null, "waiting_job_ids": [],
            "set_by": "cred_…", "set_at": "…"},
 "revision": 120}
```

An intent (`POST intents`) and a conversation (`POST conversations`) may name a `project_id`, which must exist (422
otherwise). A lead may submit an intent with a null or omitted project, or with its conversation's project.
Any other project receives 403 `owner_only`. A conversation attached to an intent takes the intent's project.
Naming another project for that conversation is 422. A request that leaves `project_id` out hashes as it did before (§7), so earlier keys still replay.

### 12.2 Budgets, and how they are shown

A budget body is `{"usd": 5, "period": "month"}`. `usd` is dollars, a number or a decimal string, 0 or more (0 holds
all paid work under it). `period` is `total` (the default: everything ever charged under it), `day` or `month`.

**Days and months are local calendar days and months on the Director's host**, from local midnight to local midnight,
as the gateway's source budgets are (WIRE.md, budgets lines; the gateway runs on the same machine, §12.5), so a
project's day and a source's day are the same day. Times on the wire stay UTC: a period's start, end and `resets_at`
are those local midnights written in UTC (in UTC−5, `2026-11-10T05:00:00Z`). Where a clock change skips midnight, the
day starts at the first moment it has. A charge counts in the period its job's reservation was first settled
(`settled_at`), including a later correction (§12.3). A budget write replaces the budget, so it needs no `Idempotency-Key`; any client credential may make it, since a
project budget can only narrow what the source budgets allow. Setting, changing or removing one offers any waiting
job that now fits (§12.3).

The `budget` field of a project or conversation:

| `state` | `shown` | Meaning |
| --- | --- | --- |
| `none` | no budget set | only the source budgets apply; `usd`, `remaining_usd` are null |
| `active` | `$R of $U left` (`this day` / `this month`) | room remains |
| `reserved` | all that remains is reserved for running work | nothing is free until running jobs settle; new work waits |
| `exhausted` | budget used up | the period's charges reached it; new work waits until it resets (`resets_at`) or is raised |

**Amounts in `shown` are honest when small.** Whole cents show as cents (`$5.00`, `$0.02`). Anything finer shows
more digits: under $1, three significant figures (`$0.0184`, `$0.00154`); from $1, four decimals (`$4.9999`). Extra
digits are cut, never rounded up, so once anything is spent `$R` is never shown equal to `$U`: $0.02 with $0.001542
spent is `$0.0184 of $0.02 left`. The same rule shapes every dollar amount the Director writes as text (§12.4's
`shown`, §12.5's labels, a waiting job's `detail`); the `_usd` numbers are exact.

**Absent and exhausted are different states**: "no budget set" is never shown as "budget used up". `spent_usd` is the
period's settled charges, `reserved_usd` what is set aside for jobs not yet settled, `remaining_usd` = `usd −
spent − reserved` (never below 0), `waiting_job_ids` the jobs held by it.

### 12.3 Reserve before dispatch, settle on the actual charge

A job is created under its project's budget and, for a turn or summary, its conversation's. **The rule:** it is
given `min(ceiling, room)`, where `room` is the least `remaining_usd` of those budgets and `ceiling` is what its spec
states, `budget.usd × trials` (when the spec states none, the whole room).

- **Reserved:** the job is `offered`. The amount is written into its spec as `budget.usd`, per run (the amount ÷
  trials, rounded down), so the node's runner and the gateway hold each run to it. The job's `bundle_sha256` is that
  of the bundle as offered; the intent keeps the bundle as given. A job under a budget needs a spec that is a YAML
  mapping (422 otherwise).
- **Nothing left:** the job is created `waiting` and is not offered. Its `budget.reason` is `budget_exhausted` or
  `budget_reserved`, and `budget.detail` says it in words a client can show. Two jobs started together under a project
  with room for one: one is offered, the other waits.
- **Settle:** when the job is over and its run (if it names one) is recorded, its reservation settles on the runs'
  `charged_usd`. A refusal or error costs what was charged, which is $0; a job that ends with no run costs $0. A
  pay-as-you-go run that reports no charge counts at the whole reservation (`charge_basis: "unreported"`), since what
  it spent is unknown. The difference is released, and waiting jobs are offered, oldest first, while room remains.
- **Cancel** of a waiting job (§6.2) ends it at once: the cancel is `applied` with outcome `stopped`, the job `error`,
  nothing charged, and its node is never asked.
- **Restarts.** Reservations are recorded with the change that caused them, so they survive a restart as they were. At
  start the Director settles any job that finished and offers what fits.

**When a run's report never comes.** A reservation is not held forever. It closes on the first of these signals,
before the job is fully known:

| Signal | `settled_by` | When |
| --- | --- | --- |
| the job's end | `terminal` | its node reports the job over (`job_terminal`), naming a run whose report has not arrived |
| a cancel outcome | `cancel` | its node applies a cancel (`stopped` or `already_finished`) |
| a stale node | `node_stale` | its node has not been heard from for `serve --reservation-stale-after` seconds (default 3600; a node is shown stale after 300, §4.3), and the reservation is at least that old |
| the last resort | `timeout` | the reservation is `serve --reservation-timeout` seconds old (default 86400) |

**The rule.** A reservation closed this way counts as charged in full (`charge_basis: "presumed"`), or at what its
runs have reported if that is more: what the job spent is not known, and each run is held to its reservation. Nothing
is released to other work on a guess. Closing ends the hold: the job no longer counts as running work, and a day or
month budget's presumed charge falls in the period it was counted, so it lifts when the period resets. A cancel of a
job its node never accepted and says it will never start (`stopped` while `offered`) is different: the job ends
`error`, with no run, and costs $0 (`no_run`).

**A late report settles it correctly.** Whenever a run linked to the job reports (its end names it, it sent live
events for the job, or its `run_finalized` names the job with `job_id`), or the job's end arrives, the charge is
recomputed whole from what is known: once the job is over and the run its end names has reported, it is the runs'
actual charge (`reported`, or `unreported`/`no_run` as above); until then, still `presumed`. The difference from the
reservation is released then, once, and waiting jobs are offered. Because the charge is recomputed, never added to,
a repeated or late report never releases twice. `corrected_at` records the last correction; the charge stays in the
period of `settled_at`.

A job that ends normally, with its runs reported, has `settled_by: "ended"`.

A job's `budget`:

```json
{"state": "held", "project_id": "prj_…", "conversation_id": null, "ceiling_usd": 5.0, "reserved_usd": 3.75,
 "per_run_usd": 3.75, "charged_usd": null, "released_usd": null, "charge_basis": null, "reason": null, "detail": null,
 "reserved_at": "…", "settled_at": null, "settled_by": null, "corrected_at": null}
```

`state` is `free` (no budget applied when it was made; its charge still counts once settled), `held`, `waiting` or
`settled`. `charge_basis` is `reported`, `unreported`, `no_run`, `never_ran` or `presumed` (closed before its report;
counted in full until it comes). `settled_by` is `ended`, `terminal`, `cancel`, `node_stale` or `timeout`.

A job's `run_ids` lists the run its end names, then every run whose report names the job (`job_id` on
`run_finalized`: each trial's run), so a job with `trials > 1` is charged for every trial that names it.

### 12.4 Usage: `GET usage`

`by` is `source` (the default), `project`, `conversation`, `intent` or `node`. `period` is `day` (the default), `month`
or `all`; `date` (`YYYY-MM-DD` or `YYYY-MM`) picks which, default today. Days and months are local to the Director's
host, as budget periods are (§12.2); `from` and `to` are those local midnights in UTC. `project`, `conversation`, `intent`,
`node` keep only those runs. A run counts in the period its report was received.

```json
{"by": "project_id", "period": "day", "from": "2026-11-10T05:00:00Z", "to": "2026-11-11T05:00:00Z",
 "groups": [{"project_id": "prj_…", "runs": 3, "charged_usd": 0.4,
             "kinds": {"pay_as_you_go": {…}, "subscription": {…}, "local": {…}}}],
 "total": {"runs": 3, "charged_usd": 0.4, "kinds": {…}}}
```

**Each source is shown for what it is.** A kind's figures are `runs`, the token counts (`input_tokens`,
`output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, `reasoning_tokens`; null when no run reported one),
`reported_models`, `shown`, and:

| Kind | `source.class` | Also |
| --- | --- | --- |
| `pay_as_you_go` | `api` | `charged_usd` (actual charges), `charge_unreported_runs` |
| `subscription` | `subscription` | `allowance` (the latest the agent reported, with `reported_at`), `allowance_reached`; no dollars |
| `local` | `free_local` | nothing more: no cost |
| `unknown` | null | nothing more |

With `by=source` each group is one `{class, model}`, its kind's figures flat beside `source` and `kind`. **An
API-price estimate (`est_usd`) is never shown**: it is not a charge.

### 12.5 Source budgets

The gateway holds a budget per pay-as-you-go source; one with none is off (WIRE.md, budgets lines). The Director
reaches the gateway on its own machine only (`serve --gateway`, a loopback address).

- `GET source-budgets` answers the gateway's `GET /v1/budgets` report as `budgets`, each source with a `shown` label:
  `off, set a budget to use` (state `off`), `active: $R left`, `budget used up until …` (state `exhausted`). `$R`
  is written as in §12.2: $0.20 with $0.0017 spent is `active: $0.198 left`.
- `PUT source-budgets` (local or paired credential) sends its body, the gateway's whole `limits.json`
  document (at most 64 KiB), to the gateway's `PUT /v1/budgets`, with the gateway's owner token, which the Director
  reads from its file on the same machine (`serve --gateway-owner-token`, default
  `~/.contextstack/config/gateway/owner-token`; it must be readable by its owner only). The token is never logged,
  stored or answered. A document the gateway refuses is 422 `invalid_request` with `gateway_status` and `gateway`;
  other refusals are 502 `gateway_refused`, an unreachable gateway 502 `gateway_unavailable`, an unreadable token 502
  `gateway_owner_token`. It replaces, so it needs no key.
- `PUT source-budgets/{source}` `{"usd": 5, "period": "day"}` sets one source's budget and `DELETE
  source-budgets/{source}` turns it off, every other entry kept (owner only, as above; no key). `{source}` is the
  gateway's source id, lower-cased, or `*` (`%2A`) for the shared budget. `usd` is 0 or more; `period` is `day` or
  `month` (the gateway's; there is no `total`). Off removes the source's own entry; while a shared budget is active it
  is set to $0 instead, which keeps the source off (the shared budget covers every source without its own entry).
  Off for `*` removes the shared budget. The answer is the new `{gateway, budgets}`.

  It is a read-modify-write against the gateway's `GET /v1/budgets` and `PUT /v1/budgets`. The gateway's report
  carries no revision and not the file, so the Director rebuilds `limits.json` from the report (`total` is `*`; each
  source's own `budget` is its entry; a source shown off under an active shared budget has a $0 entry), changes the
  one entry, reads again, and writes only if the second read means the same as the first; otherwise it starts over,
  up to three times, then answers 409 `budgets_changed`. Writes through the Director (whole or per source) run one at a
  time, so two clients' edits never lose one another. **Limitation:** a writer outside the Director (an edit to
  `limits.json`, or a `PUT /v1/budgets` straight to the gateway) that lands between the Director's last read and its
  write is overwritten; closing that needs a revision from the gateway. The rebuilt document is normalised: entries
  with no effect are dropped (a $0 shared budget; a $0 source with no shared budget), and the legacy
  `api_usd_per_day` is written as `sources."*"` daily. A gateway whose budgets file is invalid answers 502
  `gateway_refused`: replace it whole with `PUT source-budgets`.

### Chief of staff (one slot per Director)

`GET chief-of-staff` is available to local and paired credentials. Leads get 403 `owner_only`.
It returns `{chief_of_staff: {enabled, name, conversation_id, revision}}`. Defaults are `false`,
`"Chief of staff"`, `null`, and `0`. This state lives in the Director's meta table.

`PUT chief-of-staff` takes `{enabled?, name?, conversation_id?}` (`conversation_id` adopts an existing hosted-lead conversation; anything else is 422) and requires a local or paired credential and an
`Idempotency-Key`. It returns the same shape (200). Names are trimmed, 1–200 Unicode characters;
invalid names or nonboolean enabled values get 422 `invalid_request`. Omitted fields retain their values.
Enabling creates a hosted-lead conversation on the effective lead node or unarchives the existing
conversation. Its title follows the slot name. Creation chooses the first model in the Director's
ordered lead list absent from that node's last status `provider_limits` (limits with a past
`resets_at` are ignored). If every model is limited, it uses the first and adds a top-level
`model_note` explaining the fallback. Disabling archives the conversation and retains its ID.
Retries and repeated enabling reuse the same conversation. A deleted, disabled conversation is
replaced on the next enable.

While enabled, archiving or deleting that conversation returns 409 `chief_of_staff`, with a
message to turn the chief of staff off first.

`PATCH conversations/{id}` takes `{title}` and requires a local or paired credential and an
`Idempotency-Key`. Titles are trimmed, 1–200 Unicode characters; otherwise 422 `invalid_request`.
It returns 200 `{conversation}`, bumps the conversation revision, and emits its normal change.
When renaming the chief-of-staff conversation, its slot name follows, including while disabled.
Leads get 403 `owner_only`.

Every slot change emits a normal SSE `change` event on the unfiltered owner/device event stream:
`{kind: "chief_of_staff", id: "chief_of_staff", revision, record: {enabled, name, conversation_id, revision}}`.
The SSE event ID is the revision. Slot revisions share the Director's global change sequence.
Unchanged PUTs emit no slot change. Leads do not receive slot events.


### Failure meaning on jobs and turns

Job and turn records include `failure: {code, message, model_switch_may_help, retryable, detail}` (`detail` is the machine's own words for the specific cause, one line of at most 300 characters with secrets redacted, or null when it said nothing beyond the code) (a job's `error` stays the raw reason text) when their state is
`error`; otherwise `error` is null, including successful and stopped turns. This applies to individual GETs,
thread turns, and records in conversation changes. Clients should rely on this object rather than classify
terminal rows or prose themselves. `message` is one actionable sentence; `retryable` means a retry may succeed
without changing the task or configuration, and does not authorize an automatic retry. Signing in is required
for `auth_expired` even though choosing another model may help.

The Director derives this object from stored evidence without a schema change. A recognized terminal `stop`
takes precedence over the stored job reason, terminal reason/error/note, and turn `detail`, in that order.
Unrecognized evidence falls back to `unknown`. Original terminal evidence and turn detail remain available.
Budget and stuck subtypes become `budget` and `stuck`; native terminal errors, executor exit errors and provider
faults become `model_error`; `check` and `preflight_check` become `check_failed`.

| Code | model_switch_may_help |
|---|---|
| `provider_unavailable` | true |
| `auth_expired` | true |
| `provider_limit` | true |
| `rate_limit` | true |
| `model_error` | true |
| `timeout` | true |
| `check_failed` | false |
| `check_timeout` | false |
| `check_error` | false |
| `budget` | false |
| `stuck` | true |
| `cancelled` | false |
| `memory_cap` | false |
| `workspace` | false |
| `preflight` | false |
| `preflight_timeout` | false |
| `grant_paths` | false |
| `login_file_tampered` | false |
| `publish_evidence` | false |
| `evidence_incomplete` | false |
| `harvest` | false |
| `gateway_unreachable` | false |
| `gateway_config` | false |
| `gateway_protocol` | false |
| `node_config` | false |
| `node_restart` | false |
| `hook_rejected` | false |
| `sandbox_unenforceable` | false |
| `sandbox_denied` | false |
| `spool_full` | false |
| `tool_timeout` | false |
| `unknown` | false |

### Stable dispatch and recovery

`POST intents` accepts optional outer `dispatch_id`: 1–128 ASCII characters
from `[A-Za-z0-9._:-]`. It becomes the idempotency key, overriding the header.
Without it, the existing required `Idempotency-Key` header behavior is unchanged.
Replay of the same normalized request returns 200 with the original intent and
job IDs and their current records; changed content returns 409
`idempotency_conflict` without creating work. Invalid dispatch IDs return 422.
Keys share the Director-wide write namespace; choose a unique ID per dispatch.

Keys and request hashes live in SQLite `writes`, committed atomically with the
intent and job. They have no expiry or pruning: retention lasts for the database
lifetime, covering retries hours or days later. Restoring an older database loses
keys created after that backup. Responses are rendered from stored records,
not cached response bytes.

`GET intents?dispatch_id=ID` returns `{intents, jobs}` for that key, or empty
arrays if absent or outside the caller's lead scope. Lookup uses the key index,
independent of recent-list limits. Use this filter alone.
`cs mcp` exposes optional `start_task.dispatch_id` and `find_task(dispatch_id)`
with this lookup response. Keep the dispatch ID before submission and reuse it
with the same arguments after a lost response. Omission retains fresh keys.

### Built-in coding lane intents

`POST intents` also accepts `{"node":"machine","lane":{"title":"Fix bug",
"goal":"Do the work", "repo":"project","lane":"fix-bug","agent":"coder",
"model":"codex","check":"test command","budget":{"turns":100},"task_type":"bugfix"}}`.
Use the usual `Idempotency-Key` header; the response is the usual intent with jobs.
The submitted form defines idempotency, so replay survives installed profile changes.
Uploaded `files` and `from_job` intents remain supported unchanged. They cannot be
combined with this built-in `lane` form. Outer `project_id`, `grant` and string `meta`
are supported; generated workspace and profile metadata take precedence.

`lane.title` and `lane.goal` are required nonempty strings. `repo` and inner `lane`
are optional together (omit both for scratch work). Each is 1–63 ASCII characters,
starting with a lowercase letter or digit, followed by lowercase letters, digits,
periods, underscores or hyphens; invalid names return 422. The task slug uses the
inner lane name, otherwise the title: lowercase ASCII, replace runs outside a–z/0–9
with a hyphen, trim hyphens, truncate to 40 characters, trim again; empty becomes
`lane`. The task is `tasks/<slug>` and spec id is `lane-<slug>`.

`agent` selects `cs/agents/<agent>.md` from the Director's code root. No profile is
implicit: without `agent`, model defaults to `cursor-sol`, budget to 300 turns,
3600 seconds, zero tool calls and zero USD (zero means unlimited), and the generic
worker prompt is used. With `agent`, its model, prompt body, check, task type and
budget supply defaults; explicit values override them and budget merges per key.
Optional `system` overrides the prompt. Budget accepts nonnegative finite numbers
for `usd` and nonnegative integers for `turns`, `tool_calls`, `seconds`; seconds
cannot exceed 3600. Profile workspace requirements are enforced; a read-only
profile's lane check also guards against worktree changes. The profile's SHA-256
is recorded as `meta.agent_sha256`, with `agent`, `model`, `task_type`, `repo`,
`lane` and `branch` where applicable.

The code root defaults to the executable's grandparent (the installed release,
as with hosted-lead bundles); `CS_CODE_ROOT` explicitly selects another code root.
The Director reads profiles itself so desktop, MCP and other clients share its
installed defaults. Client-local `CS_AGENTS_DIR` does not alter Director defaults.
MCP sends an explicit model override when requested or when it selects a
fallback for a provider limit. Otherwise the Director applies profile defaults
and node capabilities.

The Director creates `spec.yaml`, `prompts/system.md`, `tasks/<slug>/goal.md` and
executable `tasks/<slug>/check`. A missing/empty check exits successfully; a check
body without a shebang is prefixed with `#!/bin/sh`. The spec includes workspace
repo/lane, one trial, budget and title hypothesis. The goal is preserved verbatim.

Set the lane form's `model` to `harness:<gateway-model>` to run through the
native harness, for example `harness:local-coder`. The Director checks the
assigned node's reported gateway catalog and harness capability. The spec
contains the gateway model name without the prefix. The MCP tools `start_task`
and `retry_task` accept the same selector. Agent profiles can also use it in
`model` and `fallback`. Native jobs can use scratch workspaces or repo lanes.
Repo-only read views require a CLI executor.


For a repo lane, set `lane.plan_with` to a CLI executor template such as `codex` or
`claude-code-sonnet`. The Director first offers a read-only researcher job with
`meta.phase: "planning"`. It prefers the requested node if suitable, then another
capable node that reports the repo. The job reads a snapshot of the lane tip
when present, or the repo's configured base. The node can publish a `plan.md`
run file. After planning, the Director offers the original coding job on the
requested node with `meta.phase: "coding"`, under the same intent. A usable plan
is frozen in `tasks/<slug>/context/plan.md` and included in the coding goal. If
planning is unavailable, fails, times out, or yields no usable plan, the coding
job still starts and the work trace records the reason.


### Agent profiles

`GET agents` is available to any client credential, including leads. It returns
`{agents: [{name, description, model, fallback, writes, lane, check, budget, task_type, sha256}]}`,
sorted by name. Optional fields absent from the profile are null. `sha256` hashes
its exact file bytes; the prompt body is not exposed.

`GET agents/{name}` returns one metadata object in the same shape, or `404 not_found`.
Both endpoints use the built-in lane parser and the Director's code root. These
profiles govern lane prompts, checks, budgets and write policies. `cs mcp` uses
this list and its model/fallback chain; `CS_AGENTS_DIR` does not affect MCP.

## Action approvals

- `GET action-approvals?status=needs_owner&job=JOB` lists the queue in
  `action_approvals`. Both filters are optional. The list returns up to 500
  visible approvals, with `needs_owner` first, then newest first by creation
  time within each group. `GET action-approvals/ID` fetches one record. Read
  access follows the job.
- Show the exact `argv`, `needs`, absolute `write_paths`, requester model
  and reason, job and run, reviewer model, verdict and review reason. An
  absent reviewer model means no model was selected or called.
- `POST action-approvals/ID/decide` with `{"decision":"approve"}` or
  `{"decision":"deny"}` requires a local or paired credential. Leads get 403 `owner_only`.
  Only `needs_owner` is decidable. The approval record includes the acting credential ID as `decided_by`
  and decision time as `decided_at`. A competing or repeated decision returns 409.
- The event stream sends a `change` with kind `action_approval` when a
  pending approval is created, a reviewer model is selected, its status changes,
  it is claimed, or its execution outcome is recorded. The event uses the approval ID as `id` and carries
  `record: {id, job_id, revision}`. Fetch `GET action-approvals/ID` for the
  latest state. Event visibility follows the job.
- Task work traces include approval entries in `needs_you` with
  `action_approval_id` and the escalation reason. Conversation `attention`
  can also link an approval when it has no explicit attention question.
- An approved action may be claimed once by the originating run. The
  harness checks the same record if the model repeats the request. An
  owner decision records authorization even if the original run has ended.
  There is no execution on a finished job. A consumed claim with an unknown
  outcome must be treated as potentially executed.

### Project decision log

Routes below are relative to `projects/{project_id}/decision_log`. The project
belongs to the Director installation. Local and paired clients act for its
owner. A hosted lead can use the log of the project on its conversation.

| Method and path | Request and result |
| --- | --- |
| `GET /` | Optional `status=active\|superseded\|retired` and `q` (case-insensitive text, including alternatives). Returns `{entries, markdown}` in decision-number order. |
| `GET /{entry_id}` | Returns `{entry, revisions, markdown}`. Revisions contain previous snapshots, oldest first. |
| `POST /` | Requires `decision` (one line) and `why`. Optional arrays are `alternatives: [{option, why_not}]`, `reopen_when: [string]`, and `sources: [string]`. Returns the detail shape with an active entry and the next project number. |
| `POST /{entry_id}/revise` | Supply the current `revision` and any fields from create that changed. Returns the detail shape. |
| `POST /{entry_id}/supersede` | Supply `revision` and `superseded_by`, the ID of another active entry in this project. Returns the detail shape. |
| `POST /{entry_id}/retire` | Supply `revision` to close an entry without a replacement. Returns the detail shape. |
| `POST /import` | Supply `{markdown}` containing a single table with columns `# \| Decision \| Why \| Reopen when \| Source`. Returns `{entries, markdown}`. Preserves numbers and gaps, starts alternatives empty, and treats blank or dash cells for reopen conditions and sources as empty. Separate sources with a semicolon and a space. A duplicate number rolls back the whole import. |

List and create use the base path without a trailing slash. Every POST takes
`Idempotency-Key`. Create and import return 201. Other writes and replays
return 200. A stale revision or a change to a closed entry returns 409.
Entry IDs start with `dle_`. Entries include `project_id`, `number`, `status`,
`superseded_by`, `created_at`, `created_by`, `updated_at`, `updated_by`, and
`revision` alongside the content fields. Changed entries appear on the event
stream with kind `decision_log`. There is no hard delete.

`cs mcp` exposes `decision_log(project_id, status?, search?)` and
`record_decision(project_id, action, ...)` with actions `create`, `revise`,
`supersede`, and `retire`. Pass `context: ["decision_log"]` to `start_task` or
as part of a built-in intent's `lane` object, together with the intent's
`project_id`. The job bundle stores the current active log at
`tasks/<slug>/context/decision-log.md` and adds one reference line to the goal.
The node includes the file in the frozen run stimulus and passes its Markdown
as task context to the worker. Replays and MCP retries use the original
attachment.

## Project backlog and heartbeat

The Director installation owns its projects and backlog records. Paired devices share the owner's
access. A lead credential can access the backlog and backlog events for its conversation's project.

| Route under `projects/{project_id}` | Result |
|---|---|
| `GET backlog[?status=next]` | `200 {items}` in ascending priority order, then by item ID |
| `GET backlog/{item_id}` | `200 {item, revisions}`, with previous snapshots in revision order |
| `POST backlog` | `201 {item}` |
| `POST backlog/{item_id}/update` | `200 {item}`. Pass the current `revision` and the fields to change. |
| `POST backlog/reorder` | `200 {items}`. Pass `{items: [{item_id, revision, priority}, ...]}` for an atomic priority update. |
| `GET backlog/next_unblocked` | `200 {item}`, where `item` is null when nothing is ready |
| `GET backlog/needs_you` | `200 {items}` with all open items that have an owner blocker |
| `GET heartbeat` | `200 {project_id, interval_seconds, conversation_id}` |
| `PUT heartbeat` | `200` with the saved schedule. Enable with a positive integer `interval_seconds` and an unarchived hosted lead's `conversation_id` in the project. Send `{interval_seconds: null}` to turn it off. |

Backlog writes take an `Idempotency-Key`. Create requires a non-empty `title`. `body` and
`blocked_reason` are text and default to empty. `status` defaults to `next` and accepts
`next`, `now`, `blocked`, `done` and `dropped`. `priority` is a signed integer and defaults
to zero. `blocked_on` is an array of `{kind, ref}`; `kind` accepts `job`, `item`, `owner`,
and `external`, and `ref` is a non-empty string. Links are arrays of IDs in `intent_ids`,
`job_ids`, and `decision_log_entry_ids`. Arrays default to empty. `stop` defaults to false.
Responses include `item_id`, `project_id`, `created_at`, `created_by`, `updated_at`,
`updated_by`, and `revision`. Use `dropped` to retire an item and retain its history.

Next selects among items with status `next`. Every blocker must clear. A job blocker clears when
the same-project job finishes. An item blocker clears when the same-project item is done or dropped.
Owner and external blockers clear when removed. A missing reference remains blocked.

The global event stream sends `backlog_item` changes. Hosted conversation attention shows the
first owner-blocked item when there is no conversation question or action approval taking precedence.
Clients can use `backlog/needs_you` to show the entire project's needs-you list.

The heartbeat defaults to off, with both schedule fields null. The Director waits one interval
before the first turn, defers while the target conversation is held or busy, and never overlaps
heartbeat turns in the same project. An open item with an owner blocker and `stop: true` pauses
the project's heartbeat. Turns use the regular conversation records and events with
`origin: "heartbeat"`. Disabling a schedule prevents future turns.

Lead MCP tools are `backlog(project_id, action?, status?)`, where action is `list` or `next`,
and `backlog_update(project_id, item_id?, revision?, ..., key?)`. Omit `item_id` to create an item.
