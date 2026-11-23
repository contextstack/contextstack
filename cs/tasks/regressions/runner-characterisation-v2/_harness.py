"""Shared harness for the runner characterisation fixture sets.

Drives a runner binary as a black box: a temporary estate, a scripted
fake gateway, a spec and a task per case, then pins everything visible at
the process boundary (argv, stdin to ops, stdout, stderr, exit code, rows,
transcript, run-dir files, workspace files, outbound requests). Nothing
internal is asserted. Never touches cs/ledger.

The check loads cases, fixtures and the fake gateway from its directory.
Its source callable returns the runner binary and the working tree whose
real tools, hooks, ops and prompt are copied into every estate.
`$CS_RUNNER_BIN` overrides the binary.

Versioned expectation: finalize-run writes row_version 2 rows
(optional `error`, `call_errors`). An expected row without `row_version` is
a version 1 expectation, so the actual row is projected onto version 1 by
dropping those three keys before the diff. A case whose expected row names
`row_version` is compared in full.

There is no --update. On a mismatch the first differing field is shown
and the full normalised actual output is printed for a human to
disposition (fixture error, runner bug, or deliberate contract change).
"""
import argparse
import concurrent.futures
import importlib.util
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

CODE_ROOT = Path(__file__).resolve().parents[3]  # the live code root; guarded, never written

REAL_TOOLS = ("fs_read", "fs_write", "exec", "_calllog.py")
REAL_HOOKS = ("rerun-check", "doom-loop", "step-confirm")
FIXTURE_HOOKS = ("fx_veto", "fx_gate", "fx_signal")
REAL_OPS = ("setup-workspace", "capture-stimulus", "publish-evidence", "finalize-run")
OP_HELPERS = ("_estate_io.py", "_lifecycle_evidence.py", "_harvest_git.py", "_setup_workspace.py",
              "_workspace_setup.py", "_env_policy.py", "_roots.py", "_sandbox_policy.py", "_runner.py", "_errors.py", "_api_run.py", "_node_env.py")
# Ops a case may name as its entry point (`case.json` `entry`).
ENTRY_OPS = ("run-spec", "run-api", "machine-finish")
FIXTURE_TOOLS = ("fx_tool", "fx_tool.json", "fx_snap", "fx_snap.json")
# A runtime exe tool: its descriptor enters tools/bin with no file of its own; its binary
# and the fixture prefix stay in the estate's fixture/ (the descriptor's bin is relative to the estate's
# parent, the repo).
FIXTURE_EXE = {"fx_exe.json": "tools/bin", "fx_exe_bin": "fixture", "fx_prefix": "fixture", "fx_unenforceable": "fixture",
               "fx_reporting": "fixture"}
RUNSTAMP = re.compile(r"\d{8}-\d{6}\.\d{9}")
# The macOS Python launcher can add these before any application code runs.
PLATFORM_ENV = {"__CF_USER_TEXT_ENCODING", "CPATH", "LIBRARY_PATH", "MANPATH", "SDKROOT"} if sys.platform == "darwin" else set()
DEFAULT_TIMEOUT_MS = 120000
RUN_TIMEOUT_S = 120


def copy_exec(src, dst):
    shutil.copy2(src, dst)
    if not dst.name.endswith(".json") and not dst.name.endswith(".py"):
        dst.chmod(0o755)


def install_exe(desc, src, estate):
    """A real runtime exe tool (fs_read, fs_edit are cs-tools): its descriptor, with bin made
    absolute against the source repo, so the estate execs the built binary."""
    d = json.loads(desc.read_text(encoding="utf-8"))
    binary = Path(d["bin"]) if Path(d["bin"]).is_absolute() else src.resolve().parent / d["bin"]
    if not binary.is_file():
        raise SystemExit("check: %s names %s, which is not built: cargo build --release -p cs-tools" % (desc.name, binary))
    d["bin"] = str(binary)
    (estate / "tools/bin" / desc.name).write_text(json.dumps(d, indent=2) + "\n", encoding="utf-8")


def current_runner():
    """$CS_RUNNER_BIN if set, else the default Rust runner (_runner.built_runner; scripts/check builds it)."""
    sys.path.insert(0, str(CODE_ROOT / "ops/bin"))
    from _runner import built_runner
    return built_runner(CODE_ROOT).resolve()


def load_gateway(here):
    """Load the fake gateway from the given fixture directory."""
    spec = importlib.util.spec_from_file_location("_fake_gateway", here / "_fake_gateway.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.FakeGateway


def build_estate(root, case_dir, case, src, here):
    """A minimal estate the runner accepts; every file the cases need.

    Real tools, hooks, ops and the prompt come from `src`; fixture-only
    binaries and the op shim from the set's `here/fixture`."""
    estate = root / "estate"
    for d in ("ledger", "prompts", "tools/bin", "hooks/bin", "ops/bin", "ops/real-bin",
              "specs", "tasks", "fixture", "home", "tmp"):
        (estate / d).mkdir(parents=True)
    (estate / "INVARIANTS.md").write_text("# fixture estate\n", encoding="utf-8")
    if not case.get("no_estate"):
        (estate / "ESTATE").write_text("dev\n", encoding="utf-8")
    if case.get("pinned"):
        (estate / "PINNED.md").write_text("# pins\n\npinned: %s\n" % case["pinned"], encoding="utf-8")
    shutil.copy2(src / "prompts/coder-min.md", estate / "prompts/coder-min.md")
    for name in REAL_TOOLS:
        desc = src / "tools/bin" / (name + ".json")
        if not (src / "tools/bin" / name).is_file() and desc.is_file():
            install_exe(desc, src, estate)
            continue
        copy_exec(src / "tools/bin" / name, estate / "tools/bin" / name)
        if desc.is_file():
            shutil.copy2(desc, estate / "tools/bin" / (name + ".json"))
    if case.get("repo_view"):  # exercise tool logic in a static snapshot with a fixture prefix
        for name in ("repo_search", "fs_edit"):
            install_exe(src / "tools/bin" / (name + ".json"), src, estate)
        copy_exec(here / "fixture/fx_repo_prefix", estate / "fixture/fx_repo_prefix")
        shutil.copytree(case_dir / "snapshot", estate / "snapshot")
        (estate / "snapshot/outside").symlink_to("..")
    for name in FIXTURE_TOOLS:
        copy_exec(here / "fixture" / name, estate / "tools/bin" / name)
    for name, dest in FIXTURE_EXE.items():
        if (here / "fixture" / name).is_file():
            copy_exec(here / "fixture" / name, estate / dest / name)
    for name in REAL_HOOKS:
        if (src / "hooks/bin" / name).is_file():
            copy_exec(src / "hooks/bin" / name, estate / "hooks/bin" / name)
    for name in FIXTURE_HOOKS:
        if (here / "fixture" / name).is_file():
            copy_exec(here / "fixture" / name, estate / "hooks/bin" / name)
    for name in REAL_OPS:
        if not (src / "ops/bin" / name).is_file():
            continue
        copy_exec(src / "ops/bin" / name, estate / "ops/real-bin" / name)
        copy_exec(here / "fixture/op_shim.py", estate / "ops/bin" / name)
    for name in OP_HELPERS:  # beside the real ops, and beside the entry ops (run-spec imports _env_policy)
        for dest in ("ops/real-bin", "ops/bin"):
            if (src / "ops/bin" / name).is_file():
                shutil.copy2(src / "ops/bin" / name, estate / dest / name)
    for name in ENTRY_OPS:
        if (src / "ops/bin" / name).is_file():
            copy_exec(src / "ops/bin" / name, estate / "ops/bin" / name)
    if case.get("fail_ops"):
        (estate / "fixture/fail-ops.json").write_text(json.dumps(case["fail_ops"]), encoding="utf-8")
    # Copy the tree's policy/ when present.
    # `case.json` `policy` replaces it with that object, or removes it when null. The launcher
    # (run-spec, `entry`) resolves it; a direct runner launch gets its compiled default.
    # The tree's sandbox.json enters only with `case.json` `sandbox: true`: unset, run-spec stays
    # unconfined and silent, so every other case is byte-identical with or without a built wrapper.
    if (src / "policy").is_dir():
        skip = () if case.get("sandbox") else ("sandbox.json",)
        shutil.copytree(src / "policy", estate / "policy", ignore=lambda d, names: [n for n in names if n in skip])
    if "policy" in case:
        shutil.rmtree(estate / "policy", ignore_errors=True)
        if case["policy"] is not None:
            (estate / "policy").mkdir()
            (estate / "policy/env.json").write_text(json.dumps(case["policy"]) + "\n", encoding="utf-8")
    for task_dir in sorted(case_dir.glob("task*")):
        if task_dir.is_dir():
            dest = estate / "tasks" / ("fx" + task_dir.name[len("task"):])
            shutil.copytree(task_dir, dest)
            if (dest / "check").is_file():
                (dest / "check").chmod(0o755)
    if (case_dir / "spec.yaml").is_file():
        shutil.copy2(case_dir / "spec.yaml", estate / "specs/fx.yaml")
    if case.get("state_root"):  # v2, the state root beside the estate, never inside it
        (root / "state").mkdir()
    if case.get("workspaces_root"):  # v2: workspaces in their own sibling root, apart from the run dirs
        (root / "workspaces").mkdir()
    return estate


def child_env(estate, endpoint, case):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(estate / "home"),
           "TMPDIR": str(estate / "tmp"), "LANG": os.environ.get("LANG", "en_US.UTF-8"),
           "CS_FIXTURE_SECRET_TOKEN": "must-not-inherit"}
    if "endpoint" in case:
        if case["endpoint"] is not None:
            env["CSGATEWAY_ENDPOINT"] = case["endpoint"]
    else:
        env["CSGATEWAY_ENDPOINT"] = endpoint
    if case.get("state_root"):
        env["CS_STATE_ROOT"] = str(state_root(estate))
    if case.get("workspaces_root"):
        env["CS_WORKSPACES_ROOT"] = str(estate.parent / "workspaces")
    env.update({k: v.replace("<ESTATE>", str(estate)) for k, v in case.get("env", {}).items()})  # v2: a path in the estate
    return env


def state_root(estate):
    """`case.json` `state_root: true` (v2): runs and rows land in <root>/state, a sibling of the estate."""
    return estate.parent / "state"


def launch(binary, estate, env, case):
    """The runner by default; `entry: "run-spec"` drives the op, which runs the binary."""
    argv = [str(binary), "run"] + list(case.get("argv", [])) + ["specs/fx.yaml"]
    if case.get("entry") in ENTRY_OPS or (not case.get("repo_view") and (estate / "ops/bin/run-api").is_file()):
        entry = case.get("entry", "run-api")
        env = dict(env, CS_RUNNER_BIN=str(binary))
        argv = [str(estate / "ops/bin" / entry)] + list(case.get("argv", [])) + ["specs/fx.yaml"]
    return subprocess.Popen(argv, cwd=str(estate), env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def file_view(path):
    raw = path.read_bytes()
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        import hashlib
        return "sha256:" + hashlib.sha256(raw).hexdigest()


def tree_view(root):
    out = {}
    if not root.is_dir():
        return out
    for f in sorted(root.rglob("*")):
        if f.is_file() and not f.is_symlink():
            out[f.relative_to(root).as_posix()] = file_view(f)
    return out


def collect(estate, procs, gateway, endpoint, case, binary):
    actual = {}
    results = []
    for p, out, err in procs:
        results.append({"exit_code": p.returncode, "stdout": out.splitlines(),
                        "stderr": err.splitlines()})
    if len(results) == 1:
        actual.update(results[0])
    else:
        actual["processes"] = results
    rows = []
    state = state_root(estate) if case.get("state_root") else estate / "ledger"
    ledger = state / "ledger.jsonl"
    if ledger.is_file():
        rows = [json.loads(l) for l in ledger.read_text(encoding="utf-8").splitlines() if l.strip()]
    if case.get("state_root"):  # everything the estate's own ledger/ gained: expected nothing
        actual["estate_ledger"] = sorted(p.relative_to(estate / "ledger").as_posix() for p in (estate / "ledger").rglob("*"))
    runs = []
    runs_root = state / "runs"
    run_dirs = sorted(runs_root.iterdir()) if runs_root.is_dir() else []
    manifests = {}
    for d in run_dirs:
        manifest = d / "stimulus/manifest.json"
        if manifest.is_file():
            manifests[d.name] = json.loads(manifest.read_text(encoding="utf-8"))
        ws = estate.parent / "workspaces" / d.name / "workspace" if case.get("workspaces_root") else d / "workspace"
        run = {"dir": d.name, "entries": sorted(e.name for e in d.iterdir()),
               "workspace": tree_view(ws)}
        if (d / "transcript.md").is_file():
            run["transcript"] = (d / "transcript.md").read_text(encoding="utf-8")
        if case.get("messages") and (d / "messages.jsonl").is_file():  # v2 opt-in: the event sequence
            run["messages"] = [json.loads(l) for l in (d / "messages.jsonl").read_text(encoding="utf-8").splitlines()]
        if case.get("sandbox_reports"):  # v2 opt-in: the wrapper reports finalize-run claimed, by tool
            run["sandbox_reports"] = sorted(Path(json.loads(p.read_text(encoding="utf-8"))["tool"]).name
                                            for p in (d / "sandbox").glob("*.json"))
        if case.get("repo_view"):
            receipt = json.loads((d / "execution-receipt.json").read_text())
            terminal = receipt["trace"]["harness_trace"]["identity"]["terminal"]
            run["terminal_metrics"] = {key: terminal[key] for key in ("turns", "tool_errors", "deliverables")}
            run["stop"] = receipt["native"]["stop"]
        frozen = {}
        for rel in ("goal.md", "check"):
            if (d / "stimulus" / rel).is_file():
                frozen[rel] = (d / "stimulus" / rel).read_text(encoding="utf-8")
        if frozen:
            run["frozen"] = frozen
        runs.append(run)
    if case.get("sandbox_reports"):  # cell dirs finalize-run left under <state>/sandbox-reports: emptied ones go
        cells = state / "sandbox-reports"
        actual["report_cells"] = sorted(p.name for p in cells.iterdir()) if cells.is_dir() else []
    actual["rows"] = [normalise_row(r, manifests) for r in rows]
    actual["runs"] = runs
    actual["run_dirs_distinct"] = len({d.name for d in run_dirs}) == len(run_dirs)
    actual["requests"] = [normalise_request(r) for r in (gateway.requests if gateway else [])]
    ops_log = estate / "fixture/ops-calls.jsonl"
    actual["ops"] = []
    if ops_log.is_file():
        used = case["endpoint"] if "endpoint" in case else endpoint
        actual["ops"] = [normalise_op(json.loads(l), binary, used) for l in ops_log.read_text(encoding="utf-8").splitlines() if l.strip()]
        if case.get("ops_per_run"):
            actual["ops"] = ops_per_run(actual["ops"])
    real = str(estate.resolve())
    subs = [(real, "<ESTATE>"), (str(estate), "<ESTATE>")]
    if case.get("state_root"):
        subs = [(str(state.resolve()), "<STATE>"), (str(state), "<STATE>")] + subs
    if case.get("workspaces_root"):
        wsr = estate.parent / "workspaces"
        subs = [(str(wsr.resolve()), "<WORKSPACES>"), (str(wsr), "<WORKSPACES>")] + subs
    if endpoint:
        subs.append((endpoint, "<GATEWAY>"))
    return normalise(actual, subs)


def normalise_op(call, binary, endpoint=None):
    """Mask the runner identity, path and gateway endpoint in capture-stimulus.

    Keep unexpected paths and endpoints visible in the fixture diff.
    """
    stdin = call.get("stdin")
    if isinstance(stdin, dict) and "gateway_endpoint" in stdin and stdin["gateway_endpoint"] == (endpoint or ""):
        del stdin["gateway_endpoint"]
    if isinstance(stdin, dict) and stdin.get("runner_implementation") == "rust":
        stdin["runner_implementation"] = "<RUNNER>"
        path = stdin.get("runner_path")
        if isinstance(path, str) and path and Path(path).resolve() == Path(binary).resolve():
            stdin["runner_path"] = "<RUNNER_PATH>"
    return call


def ops_per_run(ops):
    """`case.json` `ops_per_run` (v2): one list per run dir, sorted by dir, each in call order.
    Concurrent runs order their own ops (finalize-run runs publish-evidence as its child, under the run's
    lock), never each other's; the global log interleaves them however the scheduler did. Grouped before
    normalise() turns the distinct run stamps into one <RUNSTAMP>."""
    runs = {}
    for op in ops:
        stdin = op.get("stdin")
        runs.setdefault(stdin.get("run_dir", "") if isinstance(stdin, dict) else "", []).append(op)
    return [runs[k] for k in sorted(runs)]


def normalise_row(row, manifests):
    row = dict(row)
    run_name = Path(str(row.get("transcript", ""))).name
    manifest = manifests.get(run_name, {})
    for key in ("ts", "seconds"):
        if key in row:
            row[key] = "<%s>" % key.upper()
    for key in ("env_sha", "stimulus_sha"):
        if row.get(key):
            want = manifest.get(key)
            row[key] = "<%s>" % key.upper() if row[key] == want else \
                "<%s MISMATCH row=%s manifest=%s>" % (key.upper(), row[key], want)
    return row


V2_ROW_KEYS = ("row_version", "error", "call_errors")


def project_rows(expected, actual):
    """Each actual row whose expected counterpart has no row_version, projected onto version 1. `executed_by`
    (what ran, runner-specific) is dropped unless the expected row names it; tasks/regressions/
    runner-identity pins it."""
    want = expected.get("rows") if isinstance(expected.get("rows"), list) else []
    rows = actual.get("rows") if isinstance(actual.get("rows"), list) else []
    for i, row in enumerate(rows):
        if i < len(want) and isinstance(want[i], dict) and "executed_by" not in want[i] and isinstance(row, dict):
            row.pop("executed_by", None)
        if i < len(want) and isinstance(want[i], dict) and "row_version" not in want[i] and isinstance(row, dict):
            rows[i] = {k: v for k, v in row.items() if k not in V2_ROW_KEYS}
    return actual


def normalise_request(req):
    req = json.loads(json.dumps(req))
    body = req.get("body")
    if isinstance(body, dict):
        if "request_id" in body:
            body["request_id"] = "<REQUEST_ID>"
        budget = body.get("budget") or {}
        if "timeout_ms" in budget and budget["timeout_ms"] != DEFAULT_TIMEOUT_MS:
            req["remaining_ms"] = budget["timeout_ms"]
            budget["timeout_ms"] = "<REMAINING_MS>"
    return req


def normalise(value, subs):
    if isinstance(value, dict):
        return {k: normalise(v, subs) for k, v in value.items()}
    if isinstance(value, list):
        return [normalise(v, subs) for v in value if not isinstance(v, str) or v not in PLATFORM_ENV]
    if isinstance(value, str):
        for old, new in subs:
            value = value.replace(old, new)
        for name in sorted(PLATFORM_ENV):
            value = value.replace(', "%s"' % name, "").replace('["%s", ' % name, "[")
        value = RUNSTAMP.sub("<RUNSTAMP>", value)
        m = re.search(r"invalid: (.*)$", value)
        if m:
            value = value[:m.start(1)] + ", ".join(sorted(m.group(1).split(", ")))
        return value
    return value


def run_case(case_dir, binary, src, here, gateway_cls, scratch, keep):
    case = {}
    if (case_dir / "case.json").is_file():
        case = json.loads((case_dir / "case.json").read_text(encoding="utf-8"))
    if case.get("darwin_only") and sys.platform != "darwin":
        return "skip", None, None
    root = Path(tempfile.mkdtemp(prefix=case_dir.name + "-", dir=str(scratch)))
    gateway = None
    try:
        estate = build_estate(root, case_dir, case, src, here)
        if estate.resolve() == CODE_ROOT or CODE_ROOT in estate.resolve().parents:
            raise SystemExit("check: refusing live code root")
        script = []
        if (case_dir / "gateway.json").is_file():
            script = json.loads((case_dir / "gateway.json").read_text(encoding="utf-8"))
        gateway = gateway_cls(script, estate)
        env = child_env(estate, gateway.endpoint, case)
        procs = [launch(binary, estate, env, case) for _ in range(int(case.get("concurrent", 1)))]
        sig = case.get("signal")
        if sig:
            if not gateway.wait_requests(int(sig.get("after_requests", 1)), 30):
                raise RuntimeError("gateway never saw the request the signal waits for")
            time.sleep(0.2)
            for p in procs:
                p.send_signal(getattr(signal, sig.get("signal", "SIGINT")))
        finished = []
        for p in procs:
            try:
                out, err = p.communicate(timeout=RUN_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                p.kill()
                out, err = p.communicate()
                err += "\n[fixture: runner killed after %ds]" % RUN_TIMEOUT_S
            finished.append((p, out, err))
        gateway.stop.set()
        actual = collect(estate, finished, gateway, gateway.endpoint, case, binary)
        return "ran", actual, root
    finally:
        if gateway:
            gateway.close()
        if not keep:
            shutil.rmtree(root, ignore_errors=True)


def first_diff(expected, actual, path="$"):
    if isinstance(expected, dict) and isinstance(actual, dict):
        for key in sorted(set(expected) | set(actual)):
            if key not in expected:
                return "%s.%s: unexpected field, actual=%s" % (path, key, short(actual[key]))
            if key not in actual:
                return "%s.%s: missing, expected=%s" % (path, key, short(expected[key]))
            d = first_diff(expected[key], actual[key], "%s.%s" % (path, key))
            if d:
                return d
        return None
    if isinstance(expected, list) and isinstance(actual, list):
        for i, (e, a) in enumerate(zip(expected, actual)):
            d = first_diff(e, a, "%s[%d]" % (path, i))
            if d:
                return d
        if len(expected) != len(actual):
            return "%s: length expected=%d actual=%d" % (path, len(expected), len(actual))
        return None
    if expected != actual or type(expected) is not type(actual):
        return "%s: expected=%s actual=%s" % (path, short(expected), short(actual))
    return None


def short(value):
    text = json.dumps(value, ensure_ascii=False)
    return text if len(text) <= 300 else text[:300] + "…(%d chars)" % len(text)


def check_bounds(expected, actual):
    """<REMAINING_MS> placeholders must stay within the run's remaining budget."""
    ceiling = expected.get("remaining_ms_max")
    for i, req in enumerate(actual.get("requests", [])):
        if "remaining_ms" in req:
            value = req["remaining_ms"]
            if ceiling is None:
                return "$.requests[%d].budget.timeout_ms: %d is not the default and no remaining_ms_max is pinned" % (i, value)
            if not 1 <= value <= ceiling:
                return "$.requests[%d].budget.timeout_ms: %d outside [1, %d]" % (i, value, ceiling)
    return None


def main(here, label, source):
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", action="append", default=[], help="run only these case names")
    ap.add_argument("--dump", help="write each case's normalised actual output to DIR/<case>.json")
    ap.add_argument("--keep", action="store_true", help="keep temp estates (prints their paths)")
    ap.add_argument("--jobs", type=int, default=4)
    args = ap.parse_args()
    cases = sorted(d for d in (here / "cases").iterdir() if d.is_dir())
    if args.only:
        cases = [c for c in cases if c.name in args.only]
    scratch = Path(tempfile.mkdtemp(prefix=here.name + "-"))
    started = time.monotonic()
    failed = 0
    try:
        binary, src = source(scratch)
        gateway_cls = load_gateway(here)
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = {c.name: pool.submit(run_case, c, binary, src, here, gateway_cls, scratch, args.keep)
                       for c in cases}
            for case_dir in cases:
                name = case_dir.name
                try:
                    status, actual, root = futures[name].result()
                except Exception as error:  # a broken harness is a failure, not a skip
                    print("FAIL %-32s harness error: %s" % (name, error))
                    failed += 1
                    continue
                if status == "skip":
                    print("skip %-32s (darwin_only)" % name)
                    continue
                if args.dump:
                    Path(args.dump).mkdir(parents=True, exist_ok=True)
                    (Path(args.dump) / (name + ".json")).write_text(
                        json.dumps(actual, indent=1, sort_keys=True, ensure_ascii=False) + "\n", encoding="utf-8")
                expected_path = case_dir / "expected.json"
                if not expected_path.is_file():
                    print("FAIL %-32s no expected.json" % name)
                    failed += 1
                    continue
                expected = normalise(json.loads(expected_path.read_text(encoding="utf-8")), [])
                comparable = project_rows(expected, dict(actual))
                for req in comparable.get("requests", []):
                    req.pop("remaining_ms", None)
                diff = first_diff({k: v for k, v in expected.items() if k != "remaining_ms_max"}, comparable) \
                    or check_bounds(expected, actual)
                if diff:
                    failed += 1
                    print("FAIL %-32s %s" % (name, diff))
                    print("--- actual (normalised) for %s ---" % name)
                    print(json.dumps(comparable, indent=1, sort_keys=True, ensure_ascii=False))
                    print("--- end actual ---")
                    if root:
                        print("estate kept at %s" % root)
                else:
                    print("ok   %-32s" % name)
    finally:
        if not args.keep:
            shutil.rmtree(scratch, ignore_errors=True)
    print("check: %s %d case(s), %d failed, %.1fs"
          % (label, len(cases), failed, time.monotonic() - started))
    return 1 if failed else 0
