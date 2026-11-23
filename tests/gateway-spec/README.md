# modelgateway-spec — the gateway's black-box contract suite

Phase 1 of `Plan.ModelGateway.v2.1` §10 (vault `runtime-modernization/`), inventory §8 as amended by V2.2 (legacy routes retired), extended with phase 5's provider evolution (areas `catalog` and `policy`) and the pay-as-you-go source budgets (then area `spend`).. Any gateway binary that passes it honours the contract the code_root and the CS depend on; Go is the reference, the Rust candidate (`gateway/`, later) runs the same cases.

    tests/gateway-spec/run                       # build modelgateway/ (working tree), run all 207 cases (~40 s)
    tests/gateway-spec/run --fast                # offline subset, 201 cases (~17 s)
    CSGATEWAY_BIN=/path/to/csgateway tests/gateway-spec/run --impl go
    tests/gateway-spec/run --list | --area admission | --case queue-then-full | --keep | -v
    tests/gateway-spec/run --manifest out.json --source-commit <sha>   # baseline manifest

Exit 0 when every case passes, is dispositioned for this implementation, or is blocked by a missing profile capability.

## Why Python stdlib

The suite drives a process over HTTP, sockets and signals and runs mock servers; Python's stdlib does all of it with no build step and no dependencies, on macOS and Ubuntu alike. That matches the code_root's characterisation harnesses (`runner-characterisation-v2`). A Rust test crate would put the referee in the same workspace, toolchain and lint regime as one of the two contestants, and would need a build before every run. The suite has to stay neutral between the implementations.

## Safety: nothing leaves the machine

- Every gateway runs with a fresh HOME and cwd, a minimal environment (nothing inherited, no `*_API_KEY`), `CSGATEWAY_CREDENTIALS=off` unless a case supplies a temp file store with **fake** keys (or, in `default-store-is-the-file-store`, the default store under that temp HOME, run on macOS under `sandbox-exec` denying `/usr/bin/security`), and all executables pinned in `models.json` (fake `claude`, absent `codex`/`gemini`, this Python for work orders).
- `HTTP(S)_PROXY` is the suite's egress proxy. A CONNECT to `api.anthropic.com` or `api.openai.com` is TLS-terminated locally with a throwaway CA (`openssl`) and served by a mock. Every other destination is recorded and refused, and cases assert that the refused list is empty. Loopback mocks (Ollama, vLLM) are reached directly.
- The Go binary honours `SSL_CERT_FILE` on macOS only with `GODEBUG=x509sslcertoverrideplatform=1`, which the profile sets. That is what makes the Anthropic adapter (unary and stream) and cloud discovery testable without an endpoint override.
- `vllm-spark2` (the DGX tunnel port `127.0.0.1:8001`) left the embedded catalog in phase 5; it now exists only by host overlay file, and every gateway here has an empty HOME, so `impls/go.json` disables nothing. The DGX overlay case loads the repo's `providers.d.example/dgx.json` with both tunnel endpoints rewritten to mocks. Overlay and policy cases point new endpoints only at loopback mocks or dead loopback ports (`127.0.0.1:9`). A binary that still compiles a disabled name in is guarded by `disable_endpoints` and the probe (`dropped_disables` in the manifest).

**Harness requirements:** Python ≥ 3.9 and an `openssl` CLI (OpenSSL or LibreSSL). The TLS intercept also needs a harness Python linked against OpenSSL ≥ 1.1.1. macOS's `/usr/bin/python3` is LibreSSL 2.8 and fails the handshake with Go 1.27. Under such a Python the 22 intercept cases report `blocked` and the run notes why. Use Homebrew's python3.

**Second host:** the same binary and suite ran on prod-1 (macOS 27.0.1, Homebrew Python 3.14, LibreSSL 3.3.6 CLI) with the same result: 138 pass, 1 dispositioned (`baselines/go-2b458753e.prod-1.json`). With `/usr/bin/python3` there: 116 pass, 22 blocked, 1 dispositioned.

## Layout

| Path | What |
| --- | --- |
| `run` | Runner (`--impl`, `CSGATEWAY_BIN`, `--fast`, `--manifest`) |
| `harness/gateway.py` | Gateway process: env, start/stop/signals, TCP and unix-socket HTTP, SSE parsing, log rows |
| `harness/upstreams.py` | Scripted mocks (OpenAI-compatible, Ollama tags, vLLM models, Anthropic JSON and SSE replay, hold/delay/drop/cut/redirect), the egress proxy, TLS material |
| `harness/run.py` | Run context, the suite's `models.json`, shared gateways (`base`, `auth`, `admit`, `cloud`), per-case extra mocks (`t.mock`), case registry, dispositions |
| `harness/manifest.py` | Baseline manifest |
| `cases/*.py` | One module per area (below) |
| `fixtures/fake_cli.py` | Fake CLI child, the work-order executor |
| `impls/go.json` | How a Go binary is configured (upstream env vars, CA env, GODEBUG, capabilities) |
| `dispositions.json` | Per-case, per-implementation verdicts on failures |
| `baselines/go-2b458753e.json` | Pinned Go baseline (phase 1, 138 cases) |
| `baselines/go-481c95c44.json` | Go after phase 5 and the tool-args fix: 165 pass, 2 `go_bug` |
| `baselines/go-f2eebbfec.json` | Go with both phase 5 defects fixed: 167/167 pass, no dispositions |
| `baselines/go-d838d4f60-cred-once.json`, `baselines/rust-d838d4f60-cred-once.json` | Both gateways with credentials resolved once per start: 168/168 pass each, no dispositions |
| `baselines/go-file-default.json`, `baselines/rust-file-default.json` | Both gateways with the file store as default and the Keychain opt-in |

| Area | Cases | Covers |
| --- | --- | --- |
| routes | 18 | health, capabilities, models (filters, CLI), retired routes 404, 404/405, bearer auth, schema v1/v2 echo, alias, schema_mismatch, bad_request, empty_messages, tolerant decoder, wire_cutover counting and rows, admission report, inventory refresh |
| routing | 15 | unknown model refuses ($0, no upstream, no egress), empty hint → local-coder / other free_local / api_explicit_only, resolved model echoed, membership not availability, no fallback, CLI refused, no credential, mock provider, system hoisting, rate limits parked, duplicate discovered names (ollama wins, not fail-closed) |
| translation | 17 | OpenAI-compatible request bytes, default max_tokens, native tool calls, Hermes XML, history forwarding, legacy observations, malformed history refused, image refusal (clause 3), invalid neutral image, finish_reason, reasoning not content, upstream status classes, garbage/drop/unreachable |
| streaming | 16 | SSE refusal frames and chunk families, envelope-before-acceptance; openai_compat streams (text, v1 family, upstream body, tool-call fragments, Hermes XML, bad args, EOF without `[DONE]`, error payload, pre-stream status); mock streams (text and tool turn); empty hint streams; `gateway.streaming_unsupported` with `streaming` on /v1/models and /v1/capabilities; equivalence: folded stream == unary reply |
| anthropic | 16 | request bytes and tool-name encoding, unary response, images (neutral → source), tool_result history, status classes, stream: normal, v1 family, stop reason, EOF without stop, error event, pre-stream status, transport cut, deadline, client disconnect, streamed tool_use on the stop chunk, stream/unary equivalence (text and tool turns), tool input not JSON |
| accounting | 10 | api / subscription / free_local, shadow price, rounding, provider-reported charge, negative charge, $0 errors, max_input_tokens not enforced, max_usd a decimal string |
| admission | 12 | report, queue then queue_full, cancel while queued, cancel in flight, deadline, unbound model, release on every path plus timing rows, queue_timeout, no queue, shared host capacity, estate shares, stream holds its permit |
| workorders | 11 | argv/stdin/reply, environment allowlist, exit codes, 400 refusals, 2 MiB cap, unbound executor, queue_full, cancel kills the process tree, surviving descendant (slow), parent death via stdin EOF, not configured |
| cliruns | 11 | coding-agent work orders (Director.Plan step 5), the fake executor printing canned claude stream-json, codex `--json` and gemini stream-json: `stream: true` deltas are the agent's text and the stop chunk's `result` equals the unary reply; deltas arrive while the agent runs; an executor with no streaming mode refuses `gateway.streaming_unsupported` before any child; `POST /v1/agent/cli.run/{id}/cancel` mid-run (unary and stream: process group gone and permit released when it answers, the call ends `outcome: cancelled`), by the work order's `CS_JOB_ID` with `{reason}` (every running trial stopped, `cancelled: true`, the reason logged), after finish (`already_finished`, `cancelled: false`), unknown id (404), unauthorized (no bearer 401, another or no `X-ContextStack-Caller` 403 `not_owner`, the unix socket 403); `execution_id` validation, 409 while running, generated id header |
| transport | 6 | unix socket: same routes, auth, cli.run 403, stale socket replaced and removed on SIGTERM, refusals, `default` under the state root |
| discovery | 10 | modes and states, failure isolation, error classes, stale condition and recovery (slow), stopped-loop staleness from a persisted file, persistence and stopped_at, disabled endpoints, stale_after bound, SIGHUP refresh, Ollama auto-add |
| credentials | 11 | file store keys used, the default store is the file store only (no `security` exec, sandbox-denied on macOS), live list decides availability, refused store fails closed, file mode, provider narrowing, env keys ignored and not inherited, invalid spec, the `credentials` CLI, each credential resolved once per start (found, missing, refused) with SIGHUP the only re-read (slow) |
| process | 8 | auth token required, auth-disabled warning, invalid deployments, executables, cli_executor, missing models.json, SIGTERM/SIGINT exit 0, port in use |
| catalog | 18 | `GET /v1/catalog` shape and `catalog_revision`; overlay add, patch (protocol fixed), remove, env override beats overlay, removed endpoint leaves discovery; 18 strict-decoding and merge-rule rejects refuse startup; default dir, missing dir, `off`; revision determinism; reload by SIGHUP and by poll (slow); a bad file keeps the snapshot whole; the error clears once restored; per-call revision binding across a reload; permits survive a swap; model cache fresh/stale/empty and expired (slow); DGX overlay ids resolve, absent file refuses closed |
| policy | 10 | local capability denial before any permit; endpoint declaration narrows; local ∩ tenant intersection, denial wins; stream denied before connection; discovery skips denied hosts; cross-host redirect refused, same-host followed; offline fail-closed to loopback when the tenant policy is missing, invalid, or expires mid-run (slow); tenant reload rejects rollback, same-revision change, corrupt and missing |
| spend | 15 | pay-as-you-go source budgets: a source with no budget is refused `off` (no file, $0, empty, only another source budgeted; unary, stream, cloud) before any connection; a source budget admits then refuses `exhausted` with its period and `resets_at`; `budget.max_usd` only lowers (never turns a source on or raises a used-up budget; over the estimate is `ceiling`); two calls at once with room for one (the in-flight estimate is held); per-source day and month spend survives a restart and rolls over; the legacy `api_usd_per_day` as the shared `"*"` daily budget, legacy counter included; `GET /v1/budgets` (off, active, exhausted, invalid; auth; no keys); `PUT /v1/budgets` sets and replaces (canonical file, 0600, the next call and GET see it), rejects invalid and oversized documents with the file unchanged, and is owner only: the owner token (`X-ContextStack-Owner-Token`) from a local peer, never the shared token or the tokenless socket alone; the owner token file is created once, 0600 in a 0700 directory, never logged; free_local and subscription never counted; unreadable limit file, max_usd or counter refuse `invalid`. Every other gateway gets an open legacy limit file (harness `OPEN_LIMITS`) |
| typesafe | 5 | `POST /v1/ai/typed-question` (ADR 0020): a TypeSafe request reaches `{endpoint}/systemone` unchanged with the stored key as Bearer (fake key, loopback mock behind an overlay patch), the answer returns unchanged with `X-ContextStack-Cost-Usd` = input tokens × the catalog rate, counted into the `typesafe` source; an upstream error passes through uncounted; refusals before any connection (400 not a TypeSafe request, 402 budget off, 429 model the deployment does not bind, 503 no credential); a `typesafe` deployment lets keyless hosts start (store off, or narrowed by `CSGATEWAY_KEYCHAIN_PROVIDERS`); `model.call` never reaches the endpoint |

See [method and decisions](METHOD.md).
