# cs-tools

The CS core tools as one multi-call Rust binary (Plan.CS.v2.1 §7, phase 5): `fs_read` and `fs_edit`, byte-for-byte ports of the Python tools formerly in `cs/tools/bin/` (historical source: `git show 69c507d9b:cs/tools/bin/fs_read`), and `okf`, the port of the cs's only Ruby tool (historical source: `git show e539803ef:cs/tools/bin/okf`), so a node needs no Ruby. Also `yaml-json`, an ops helper. Not in the runner: exec'd like any tool, so the sandbox and fixtures cover it.

## Contract

    cs-tools fs_read|fs_edit|okf < args.json    # descriptor: "runtime": "exe", "bin", "args": [TOOL]
    okf < args.json                              # a symlink named for the tool (argv[0] basename wins)

    cs-tools yaml-json FILE                     # or a symlink named yaml-json; no stdin, no descriptor

Cwd is the workspace, JSON args on stdin. Anything else selected: usage on stderr, exit 2.

- `fs_read`/`fs_edit`: success on stdout, exit 0; refusal is the Python `sys.exit` text on stderr, exit 1. Both append `{"args":..., "name":...}` to `.cs-calls.jsonl` with `json.dumps(sort_keys=True)` bytes. `fs_edit`'s diff is CPython's `difflib` (`src/difflib.rs`); `int`/`str`/truthiness are Python's.
- `okf {action: stamp|migrate|lint|stale, root?, version?, from?, to?, now?}` (`src/okf/`): Ruby's `JSON.generate` lines on stdout; `fail!` is `{"ok":false,"error":...}` on stderr, exit 1; lint failures print the report and exit 1. No call log, as the Ruby wrote none.
- `yaml-json FILE` (`src/yaml_json.rs`): stdout of `ruby -ryaml -rjson -e 'puts JSON.generate(YAML.safe_load( File.read(ARGV[0])))' FILE` (Ruby 4.0, Psych 5.3.1, json 2.18: Grisu2 floats in `src/fpconv.rs`). A Ruby exception is `yaml-json: <message> (<Class>)` on stderr, exit 1 (dates, times, symbols: `DisallowedClass`; any alias: `AliasesNotEnabled`). Not model-facing, so no `cs/tools/bin` descriptor: ops helpers exec it via `cs/ops/bin/_yaml.py` (`CS_TOOLS_BIN` overrides `<repo>/target/release/cs-tools`; copied test estates set it).

## okf: YAML as Psych reads it

Frontmatter is never re-emitted as YAML: okf rewrites it as text (regex substitutions, JSON-quoted scalars), ported as hand matches of each Ruby regex in `src/okf/front.rs`. YAML is only read, through `libyaml-safer` (a safe port of libyaml 0.2.5, the version Psych 5.3 bundles), then Psych's `ToRuby` rules in `src/okf/psych.rs` and `ScalarScanner` in `src/okf/scanner.rs`: YAML 1.1 booleans, sexagesimal and comma integers, `Date`, `Time` (so an unquoted ISO instant is a Time whose `to_s` has no `Z`, and fails `valid_instant?`, as the fixture notes pin), merge keys, aliases, the restricted class loader's `DisallowedClass`. `Psych::SyntaxError` text uses libyaml's problem, context and context mark. Ruby's `Time.iso8601` (`apply_offset`, `force_zone!`), `Float#to_s`, `String#inspect` and Hash `inspect` are in `src/okf/rtime.rs` and `src/okf/rb.rs`; a time without an offset is local (chrono).

Libyaml-safer 0.3.0 panics in two places C libyaml does not: a block scalar running to the end of input (any frontmatter whose last value is `|` or `>`), and a tag directly before `,` in a flow collection. Both are caught and retried so the result is libyaml's (`first_document`; unit-tested). It decodes all input at once, C 16 KiB windows on demand: `src/okf/utf8.rs` feeds it C's windows and C's reader problems.

## Stronger than the originals: symlinks

The Python tools only reject absolute paths and `..`; the ports also resolve the path and refuse an existing path whose real location is outside the real workspace (`path escapes workspace: P`), and skip a call log that is a symlink leaving it. okf's Ruby checked only the root: the port also refuses, before reading or writing anything, a corpus file or root index that dangles or resolves outside the workspace. Links that stay inside still work: file opens resolve the target inside the workspace, use `O_NOFOLLOW`, and match the opened regular file's device/inode before I/O. Edits validate before truncating; OKF uses exclusively created sibling temporary files and renames.

Reads buffer at most a capped line and output window. Large reads stop early and report “of at least N” when EOF is unknown; negative limits count newlines first to preserve slicing from the end. Directory output uses the same 2000-line/50 KB caps and pagination. Invalid UTF-8 text is refused. Edits cap files, requests, and results at 10 MB, and mark diffs cut at 50 KB as `diff truncated`.

Migration stages every changed document and the root index before publishing any of them. Staging errors leave originals unchanged and remove temporary files. Each rename is atomic, but the corpus is not a crash transaction: a crash or rename failure during publication can leave a partial migration; rerun the idempotent migration to finish it.

## Known differences (not byte-identical)

- An uncaught exception: Python prints a traceback, Ruby a backtrace; the port prints one line (`TypeError:...`; okf: `okf: <message> (<Class>)`). Exit 1 either way, same stdout and files. Ruby okf's own crashes include `migrate` over a non-reserved file with no frontmatter (`TypeError`).
- Malformed JSON on stdin: same exit 1, serde's wording (Python's for empty input and a BOM). Python surrogate-escapes invalid UTF-8 on stdin; the port refuses it. serde_json rejects `NaN`, lone surrogates, and reads integers beyond 64 bits as floats (Ruby/Python keep bignums).
- `repr`/`inspect` printability is approximated (controls, separators, noncharacters escaped; unassigned code points are not). okf glob is case-insensitive on macOS (as Ruby's is there). Both YAML readers refuse as unsupported (`NotImplementedError`) `!ruby/...` tags naming Date/Time (okf), setting instance variables, or `!ruby/encoding` (which Ruby loads: `"UTF-8"`, or `ArgumentError`).
- Ruby's stack gives out at 1,408 nested sequences or 1,014 mappings, the port's at 2,000 levels: between, yaml-json says `JSON::NestingError` where Ruby says `SystemStackError` (exit 1, no stdout, both).

## How parity is proved

- `scripts/check`: fmt, clippy, release build, unit tests, `tests/exec.rs` (both install forms, symlinks).
- The originals' contract fixtures (`cs/tools/bin/{fs_read,fs_edit,okf}.fixtures`), run by `cs/scripts/tool-fixtures` through the exe descriptors, plain and under `cs-sandbox`.
- Differential runs against the originals on generated inputs: fs_read/fs_edit about 5,500 cases; okf 15,000 on the final build (random corpora, scalar and mutation fuzz, three time zones): 14,218 identical, 551 Ruby uncaught exceptions (same exit, stdout, files), 231 symlink refusals, no other difference. Earlier batches found the two libyaml-safer panics and two Ruby numerics. yaml-json: every repo YAML (1,656 its callers read, 27,134 others), 38,260 generated and 35 tag probes: 67,038 of 67,050 identical; the rest are the deep-nesting and `!ruby/encoding` gaps above.
