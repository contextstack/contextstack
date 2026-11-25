# ContextStack tracing contract

The approved tracing proposal v5.2 defines the boundaries of Step 0 and Step 0b.
This contract defines the records and the checker result. The component that does
the work writes its closing record durably before forwarding it.

## Span records and IDs

Write one JSON object per line. Each record has these fields.

| Field | Meaning |
| --- | --- |
| `trace_id` | Stable nonempty string for one run. A run gets one trace even when its job has other trials. |
| `span_id` | Nonempty string unique within the trace. |
| `parent_id` | Parent span's ID in the same trace, or null for the root. |
| `kind` | Record kind from the table below. |
| `name` | Nonempty operation name for display. |
| `start`, `end` | Producer wall-clock timestamps as UTC RFC 3339 strings with one to six fractional digits and a trailing `Z`. Forward unchanged. |
| `duration_ms` | Nonnegative elapsed milliseconds measured by one monotonic clock on the producer's machine. Fractions are allowed. |
| `attributes` | Object containing `machine` (the producer's nonempty machine ID) and the kind-specific data. Store any receiver timestamp separately as `received_at`. |
| `outcome` | Nonempty outcome string. The tables and rules below fix the values used by the checker. |

| Kind | Parent | Boundary or event |
| --- | --- | --- |
| `run` | null | Harness or adapter start through finalization. |
| `turn` | run | Before issuing the call through tools and post-turn hooks. |
| `model_call` | turn | First send through full reply or terminal error. Use `completed` for a full reply. |
| `gateway_exchange` | model_call | Request received through response written or stream closed. |
| `provider_attempt` | gateway_exchange | Upstream send through the end of the body or stream, or error. |
| `connection_open`, `first_output` | provider_attempt | Instants within an attempt. |
| `retry_backoff` | gateway_exchange | Time spent waiting before the next attempt. |
| `usage`, `settlement` | gateway_exchange | Measurement and actual budget charge events. |
| `tool_call` | turn | Harness takes up a requested call through recorded result or refusal. |
| `pre_tool_hook`, `tool_process` | tool_call | Process launch through exit. |
| `post_turn_hook` | turn | Process launch through exit. |
| `finalization` | run | Finalization through frozen check, verdict and evidence link. |
| `cli_work_order` | run | Harness dispatch through the gateway terminal response or transport failure. |
| `cli_process` | run | CLI launch through exit. |
| `cli_event`, `cli_usage` | cli_process | Adapter stream event and reported usage. |

Instant events have equal start and end and zero duration. Use the producer
timestamp for CLI events when available and the adapter receive time otherwise.
Use links for causal order and `duration_ms` for elapsed time. Never derive elapsed
time from wall-clock differences across components. The gateway owns settlement.
The Director owns the verdict and evidence. Consumers use those records.

The run root has `run_id` and `coverage`. A dispatched root also has `job_id`,
`task` (task identifier), and `trial` (the dispatched nonnegative integer trial
number). Direct runs omit `job_id`. A finalized root has outcome `finalized`.
Finalization attributes include `verdict` and `evidence_id`.

Each full turn carries integer `turn`, starting at 1. Each model call carries
one `request_id` unique within the run and an array `sends` of
`{"send_index":1,"outcome":"delivered"}` objects. Indexes start at 1 and follow
send order, including streaming refusal followed by unary fallback. Put
`job_id` when present, `run_id`, the turn number as `step_id`, `request_id`,
and `send_index` on the wire. The exchange carries `request_id` and
`send_index` in its attributes and the model call's span ID as its parent. Match
on the pair within the run, independent of arrival order and streaming mode.
Each provider attempt carries integer `attempt` starting at 1 in its exchange.

## Coverage

- **`full`** is the Rust harness. Record the full turn, model, send, gateway,
  attempt, tool and hook chain as it occurs. Include the root's `terminal`
  counters and charge, and the finalization and verdict for finalized runs.
- **`observed`** is an installed CLI. Record the CLI process, every received
  stream event, CLI-reported usage when present, and finalization and verdict
  for finalized runs. Include turns and tool calls to the extent reported by the
  CLI. Apply rule 4o in place of rule 4. Gateway reconciliation is for full runs.

Coverage is fixed for the lifetime of a run. Split every aggregate by coverage.

## Content pointers

Use `attributes.content`, an object keyed by role such as `prompt`, `reply`,
`arguments`, `result` or `output`. Each value is `{file, line, sha256}`.
Store pointers for content-bearing operations. Keep the content in the run's own
files and make export an explicit choice. `file` is relative to that run's
directory and must stay inside it even after resolving symlinks. `line` is a
one-based integer. Hash the raw bytes of the line excluding its terminating LF
if present. Encode SHA-256 as 64 lowercase hex digits.

The checker takes a `run_directories` map from trace IDs to paths. Resolve paths
relative to the file declaring them. For each supplied pointer, an invalid
pointer, an undeclared run directory, an escaping path, or an unreadable or
missing line in an existing directory is a missing item under `content`. A
missing declared run directory and a resolvable line with the wrong hash are
findings. The checker does not infer which payloads existed.

## Input, results and rules

A checker input is span JSONL or a JSON object with any of `spans`,
`finalized_runs`, `settled_jobs`, `work_traces`, and `run_directories`.
The first four fields are arrays. Fixtures also carry `expected`, used only by
the self-test. All files in one invocation form one input set. Records can
arrive in any order. The checker accepts a JSON array of spans as well.

The result object has `result`, `missing`, and `findings`. Missing items are
unique strings sorted lexically. Findings are objects sorted lexically by their
key-sorted JSON serialization. Use `incomplete` when any missing item exists,
`complete_with_findings` when only findings exist, and `complete` otherwise.
Return findings even when the trace is incomplete. The CLI exits 0 for either
complete result, 1 for incomplete, and 2 for unreadable or malformed input.
Malformed base span fields are input errors. A run-scoped missing item starts
with the rule and trace ID, for example `4: trace-1/turn 2`.

### 1. Structure

Require one parentless `run` in each trace, unique span IDs, and a same-trace
parent of the allowed kind for every other span. Require a nonempty run ID, a
valid coverage level and the dispatched identity fields when `job_id` exists.
Flag two traces claiming the same run ID.

### 2. Sends and provider attempts

For every model call in a full run, require a unique request ID and a nonempty
send list with exactly one entry for each index 1 through its length. Every
exchange child must match the parent request ID and a listed send index.

| Send outcome | Required exchanges |
| --- | --- |
| `delivered` | Exactly one. The harness got a response or stream. |
| `connection_failed` | Zero. The connection was never established. |
| `unknown` | Zero or one. The harness could not confirm delivery after sending. Always report a finding. |

Require the exchange's integer `attempts_started` in 0 through 3. Require one
attempt per number 1 through that count and flag out-of-range numbers.
Require outcome `retryable_failure` for every attempt before the last.
Require `final_attempt_outcome` to match the last attempt, or explicit null
when the count is zero. A refusal before provider contact requires a count of
zero. Missing attempts are named individually, including the final attempt.

### 3. Termination and billing

Require one exchange outcome from `completed`,
`refused_before_provider_contact`, `upstream_error`, `deadline`,
`stream_fault`, and `client_disconnect`. Require `billing_source` from
`pay_as_you_go`, `free_local`, and `subscription`, and a Boolean
`client_saw_output`. A refusal requires a `refusal_code` and false
`client_saw_output`.

| Exchange | Usage children | Settlement children |
| --- | --- | --- |
| Completed and pay-as-you-go | One, `partial: false` | One, `partial: false` |
| Completed and local or subscription, provider charge reported | One, `partial: false` | One, `partial: false` |
| Completed and local or subscription, no provider charge | One, `partial: false` | Zero, exchange `wallet_charge_usd: 0` |
| Ended with a positive provider charge and an outcome other than completion or refusal | One, `partial: true` | One, `partial: true` |
| Ended after output with a paid source and an outcome other than completion or refusal | One, `partial: true` | One, `partial: true` |
| Paid request body sent, aborted before completion with no provider-reported charge | One, `partial: true`, if output was seen, otherwise zero or one | One, `partial: true` |
| Paid request answered 2xx, then ended with an outcome other than completion, with no provider-reported charge | One, `partial: true`, if output was seen, otherwise zero or one | One, `partial: true` |
| All other cases | Zero | Zero |

Require the listed counts and Boolean `partial` flags on each child. A paid
call without a provider-reported charge is billed once its body reached the
provider. This applies to an abort by deadline, connection loss, client
disconnect, or cancel when the final attempt has `request_body_sent: true`. It
also applies to a failure after the final attempt's `http_status` was 2xx,
such as an unusable body, a stream that broke with or without text, a shutdown
or a spend-file write failure. A 2xx failure needs no body marker unless it is
false. The gateway records a cancel as `client_disconnect` and marks connection
loss with `connection_lost: true` on the attempt. These two billed rows take
precedence over the paid-output row. An abort before sending the body has no
settlement. An explicit upstream HTTP refusal (300 and up) also has no
settlement. Legacy traces can omit the body transmission marker.

A billed settlement is never below the measured usage. With
`basis: "reservation_estimate"` the usage was unknown and `charged_usd`
equals the final attempt's `reservation_usd`. Otherwise the basis is
`measured` or omitted and the charge is measured usage, which can exceed the
reservation. For any paid exchange that ended without completion or refusal and
has no provider-reported charge, `charged_usd` is at least the usage child's
`estimated_usd` when present. A lower charge is a missing item
`charge below measured usage`. The model call ends `error`. The trace is
complete when the run's terminal charge matches the settled charges.
Settlement requires a nonnegative numeric `charged_usd`. When a provider reports
a positive charge, the exchange carries `provider_charge_usd` and the settlement
carries the same amount in `charged_usd` with `basis: "provider_reported"`.
A local or subscription exchange with a reported charge also carries that amount
in `wallet_charge_usd`. All settlement amounts participate in run and job spend
reconciliation. Usage can carry an estimated cost
(`estimated_usd`) and measured tokens. Estimated cost can be positive for
subscription usage. It sets the charge floor above and plays no part in spend
reconciliation.

### 4. Full terminal reconciliation

Require nonnegative integer `terminal.turns` and one turn span per number
1 through that count. Report missing numbers individually and flag extra or
out-of-range turns. Require exactly one model call per turn.

Require each tool-call span to have exactly one of these outcomes.
`executed`, `invalid_arguments`, `unknown_tool` and `refused_by_hook` count toward the terminal's
integer `tool_calls`. `aborted_by_hook`, `cancelled` and
`skipped_for_budget` do not count. Compare the counted total with
`terminal.tool_calls`. Require exactly one tool process for `executed` and
zero for every other outcome. Record the error on an unknown tool as `error`.

Require nonnegative numeric `terminal.charged_usd`. Use
`terminal.accounted_settlements` to record the request and send pairs included
in that total. For a run marked finalized by its root or the fleet inventory,
require exactly one finalization child with a verdict.

### 4o. Observed terminal reconciliation

Require one CLI process and an integer `exit_status`. Its duration uses the
common span field. Its closing attributes give `received_events` as a
nonnegative integer, which must equal the number of CLI event children.
Each CLI event requires `timestamp_source` set to `producer` or
`adapter_receive`. The process's Boolean `usage_reported` requires one
`cli_usage` child when true and zero when false. That child's `source` is a
nonempty CLI usage source name, for example `claude_code_native`. The value
`gateway` is invalid. Check finalized runs for finalization and verdict as in
rule 4.

### 5. Fleet reconciliation

Provide `finalized_runs` from the Director, one object per finalized run with
`run_id` and, for dispatched jobs, `job_id`. For direct runs use the node's
run directories as the reference and omit `job_id`. When an inventory is
provided, require a trace with a valid coverage level for each listed run.
A standalone trace can be checked without a fleet inventory.

### 5w. Work trace

A work trace is one object per task, keyed by its task ULID in `task_id`. It
carries the following records. Run IDs under jobs link to the run traces.

| Field | Shape |
| --- | --- |
| `title` | Task title. |
| `origin` | `{kind, conversation_id, turn_id}` for `conversation_turn`, or `{kind}` for `schedule`, `cs_cli`, or `api`. |
| `jobs` | Creation-ordered objects with `job_id`, Boolean `finished`, and `run_ids`. After the first job include `reason: {kind}` for `retry` or `owner_request`, `{kind, template}` for `fallback`, `{kind, planner_job_id}` for `planned`, or `{kind, detail}` for `planning_fallback`. |
| Job `decisions` | Objects with `kind` (`approve`, `reject`, `land`, `discard`, `cancel`), `actor`, and UTC `at`. A job awaiting a decision uses `decision_status: "none_yet"`. |
| `raised_needs_you_ids`, `needs_you` | Inventory of raised IDs and state objects with `id`, `raised_at`, and `status` (`open` or `cleared`). The cleared state also has `cleared_at` and `cleared_by`. |
| `owner_message_ids`, `owner_messages` | Inventory of owner message IDs on the task thread and mark objects with `message_id`, `actor`, UTC `at`, and `mark` (`instruction`, `answer`, `intervention`). |

Require one of the named origin kinds, and both IDs for a conversation turn.
Require a permitted reason on every job after the first, including the limited
template for a model fallback, the preceding planner job for a planned job,
and a detail for a planning fallback. Require each finished job to have a decision or
explicit `none_yet`. Check every supplied decision for kind, actor and time
presence. For each Needs you ID in the inventory or state records, require a
raise time and open or cleared status with the cleared fields when applicable.
For every message ID in the inventory or mark records, require a valid mark.
The inventories come from the Director and allow the checker to catch a
wholly withheld state or mark. An intervention corrects, chases or redirects
work already assigned. The chief of staff records its judgment and an owner
correction takes precedence. Graders read the mark.

### 6. Findings

| `kind` | Exact condition and fields |
| --- | --- |
| `spend_discrepancy` | For a full run, the sum of settlement charges differs from `terminal.charged_usd`. Report `trace_id`, `settled_usd`, `terminal_usd`, and sorted unique `request_ids` with nonzero settlement sends absent from `accounted_settlements`. An empty list means no unaccounted settlement was identified. |
| `spend_discrepancy` | For a supplied `settled_jobs` entry `{job_id, charged_usd}`, sum the gateway settlements across the job's finalized run inventory when it is nonempty and all listed runs have full traces. If the total differs, report `job_id`, `settled_usd`, and `director_usd`. Amounts in findings are decimal strings. |
| `delivery_unconfirmed` | A send has outcome `unknown`. Report `trace_id`, `request_id`, `send_index`, and Boolean `gateway_found` for a matching exchange. |
| `clock_skew` | A child starts before its parent starts or ends after its parent ends, on different machines. A gateway exchange can finish after its model call errors, so compare exchange ends only for completed model calls. Report each contradictory `boundary` (`start` or `end`), with `trace_id`, `span_id`, `parent_id`, `machine`, `parent_machine`, and the positive `difference_us`. Durations stay unchanged. |
| `pruned_content` | A valid pointer has a declared run directory that no longer exists. Report `trace_id`, `span_id`, `label`, `file`, and `line`. |
| `content_hash_mismatch` | A line resolves but hashes differently from its pointer (scenario V). Report the same location fields plus `expected_sha256` and `actual_sha256`. |

## Decisions made in this contract

- Use snake_case JSON names and outcome values. The span envelope uses `start`,
  `end`, `parent_id`, and `attributes.machine`. Instant events share the
  span envelope. Trace and span IDs are opaque strings.
- Use `terminal` on the run root, `sends` on the model call, attempt counts
  on the exchange, and stream-event count and usage flag on the CLI process.
  Use integer turn, attempt, and send indexes. The terminal's
  `accounted_settlements` makes spend attribution possible when several calls
  settle.
- Use role-keyed `content` pointers and a separate `run_directories` map for
  checking local bytes. Report a resolved hash mismatch as a finding per V.
- Use nested work records with explicit Needs you and owner-message inventories.
  Supply Director job charges separately in `settled_jobs` for comparison in G
  and J.
