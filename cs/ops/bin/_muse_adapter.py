"""Opt-in profiled CLI executor; frozen context and truthful terminal evidence.

The generic executor supplies its existing staging,
authority and lifecycle operations. Muse and isolated OpenCode/pi/Grok profiles share this lifecycle.
"""
import hashlib
import importlib.util
import json
import os
import re
import stat
import signal
import subprocess
import sys
import time
from pathlib import Path

from _cli_native import declared_home_reads
import _muse
import _lanes
import _run_record
from _provider_limits import detect_limit, enrich_allowance, record_limit
from _estate_io import state_dir
from _job_reason import persist_output, redact
from _roots import task_label, transcript_id

BUNDLE = ("_executor.py", "executor-templates.json", "_muse.py", "_muse_adapter.py", "muse-profile.json", "_cli_pins.py")
SOURCES = {(p, s) for p in ("defaults", "policy")
           for s in ("system_file", "macos_managed_preferences")}


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def load_profile(path):
    value = json.loads(Path(path).read_text())
    if (not isinstance(value, dict) or value.get("settings") != {"schema_version": 1}
            or value.get("home_mode") != "private_home_keychain_link"
            or not isinstance(value.get("model"), str)
            or not re.fullmatch(r"[a-zA-Z0-9._-]+", value["model"])
            or type(value.get("max_model_steps")) is not int
            or not 1 <= value["max_model_steps"] <= 24
            or value.get("reasoning_effort") not in ("low", "medium", "high")
            or not isinstance(value.get("binary"), str)):
        raise ValueError("invalid pinned Muse profile")
    return value


def stage_home(ws, real_home, profile):
    home = ws / ".lab-tmp" / "muse"
    if home.exists() or not home.resolve().is_relative_to(ws):
        raise ValueError("reserved Muse home is not a fresh workspace path")
    overrides = {"HOME": home, "XDG_CONFIG_HOME": home / "config",
                 "XDG_DATA_HOME": home / "data", "XDG_CACHE_HOME": home / "cache",
                 "TMPDIR": home / "tmp", "TMP": home / "tmp", "TEMP": home / "tmp"}
    for directory in overrides.values():
        directory.mkdir(parents=True, exist_ok=True)
    config = overrides["XDG_CONFIG_HOME"] / "muse"
    config.mkdir()
    metadata = _muse.subscription_metadata(real_home / ".config/muse/auth.json")
    for name, value in (("auth.json", metadata), ("settings.json", profile["settings"])):
        path = config / name
        path.write_text(json.dumps(value, sort_keys=True) + "\n")
        path.chmod(stat.S_IRUSR)
    # Keep personal context absent while Security.framework finds the login Keychain.
    library = home / "Library"
    library.mkdir(parents=True, exist_ok=True)
    keychains_link = library / "Keychains"
    keychains_link.symlink_to(real_home / "Library" / "Keychains", target_is_directory=True)
    return {k: str(v) for k, v in overrides.items()}, [config / "auth.json", config / "settings.json", library]


def config_generation(text):
    # 1.0.3 exposes plain text only; it has no config status --json option.
    generation = re.findall(r"^Generation: (sha256:[0-9a-f]{64})$", text, re.M)
    sources = re.findall(r"^\s*plane=(\S+) source_class=(\S+) state=(\S+)\s*$", text, re.M)
    if (len(generation) != 1 or len(sources) != 4
            or {(p, s) for p, s, _ in sources} != SOURCES
            or any(state != "absent" for _, _, state in sources)):
        raise ValueError("Muse system/managed configuration must have four absent sources")
    return generation[0]


BACKGROUND_CLIENT_ID = "muse-runtime-background-terminal"


def _validate_command(records, accepted_record):
    """Validate one runtime.command.accepted (turn.submit) chain: linked/start/
    terminal identity, kinds, exactly one of each, complete terminal, ordering
    accepted<link<start<terminal. Returns (command_id, terminal_record_index)."""
    accepted_payload = accepted_record.get("payload", {})
    if accepted_payload.get("kind") != "command_accepted":
        raise ValueError("Muse command accepted has wrong payload kind")
    cid = accepted_payload.get("command_id")
    if not cid or not isinstance(cid, str):
        raise ValueError("Muse command accepted missing command_id")

    # Find session.run.linked for that command
    linked = [r for r in records if r.get("payload_type") == "session.run.linked"
              and r.get("payload", {}).get("command_id") == cid]
    if len(linked) != 1:
        raise ValueError("Muse must report exactly one session.run.linked for command")
    linked_payload = linked[0].get("payload", {})
    if linked_payload.get("kind") != "session_run_linked":
        raise ValueError("Muse session.run.linked has wrong payload kind")
    stream = linked_payload.get("run_stream")
    if (not stream or not isinstance(stream, dict)
            or stream.get("kind") != "run" or not stream.get("id") or not isinstance(stream.get("id"), str)):
        raise ValueError("Muse session.run.linked missing valid run_stream")

    # Any other record sharing this run_stream must belong to the same command
    for r in records:
        r_payload = r.get("payload", {})
        if r_payload.get("run_stream") == stream and r_payload.get("command_id") != cid:
            raise ValueError("Muse run_stream reused by a different command")

    # Find run.lifecycle.started for this command
    starts = [r for r in records if r.get("payload_type") == "run.lifecycle.started"
              and r.get("payload", {}).get("command_id") == cid]
    if len(starts) != 1:
        raise ValueError("Muse must report exactly one run start")
    start = starts[0]
    start_payload = start.get("payload", {})
    if start_payload.get("kind") != "run_started":
        raise ValueError("Muse run start has wrong payload kind")
    if start_payload.get("run_stream") != stream:
        raise ValueError("Muse run start has mismatched run_stream")

    # Verify ordering: accepted < linked < start
    if (records.index(accepted_record) >= records.index(linked[0])
            or records.index(linked[0]) >= records.index(start)):
        raise ValueError("Muse record ordering invalid")

    # Find run.terminal.* records for this command
    terminals = [r for r in records if str(r.get("payload_type", "")).startswith("run.terminal.")
                 and r.get("payload", {}).get("command_id") == cid]
    if len(terminals) != 1:
        raise ValueError("Muse stream is incomplete or has conflicting terminals")
    terminal = terminals[0]
    terminal_payload = terminal.get("payload", {})
    if terminal_payload.get("run_stream") != stream:
        raise ValueError("Muse terminal has mismatched run_stream")

    # Validate terminal type, kind, terminal value, text, and ordering
    if (terminal.get("payload_type") != "run.terminal.completed"
            or terminal_payload.get("kind") != "run_terminal"
            or terminal_payload.get("terminal") != "completed"
            or not isinstance(terminal_payload.get("text"), str)
            or records.index(terminal) <= records.index(start)):
        raise ValueError("Muse command did not complete")

    return cid, records.index(terminal)


def validate_root_terminal(text):
    records = [json.loads(line) for line in text.splitlines() if line.strip()]
    if not records or any(not isinstance(r, dict) or r.get("schema_version") != 1
                           or not isinstance(r.get("payload"), dict) for r in records):
        raise ValueError("invalid Muse JSONL envelope")

    # Find every accepted turn.submit command
    accepted = [r for r in records if r.get("payload_type") == "runtime.command.accepted"
                and r.get("payload", {}).get("command_kind") == "turn.submit"]

    # Root is the one accepted command with a missing/null client_id
    roots = [r for r in accepted if r.get("payload", {}).get("client_id") is None]
    if len(roots) != 1:
        raise ValueError("Muse must report exactly one root command accepted")
    root_accepted = roots[0]

    # Every other accepted command must be a background terminal command
    backgrounds = [r for r in accepted if r is not root_accepted]
    for r in backgrounds:
        client_id = r.get("payload", {}).get("client_id")
        if client_id != BACKGROUND_CLIENT_ID:
            raise ValueError("Muse background command accepted has unknown client_id")

    # Validate the root command chain with the old checks
    root_cid, root_terminal_index = _validate_command(records, root_accepted)
    validated_ids = [root_cid]
    seen_ids = {root_cid}

    # Validate each background command chain; must be unique and complete
    # only after the root terminal
    for r in backgrounds:
        cid, _ = _validate_command(records, r)
        if cid in seen_ids:
            raise ValueError("Muse duplicate background command_id")
        seen_ids.add(cid)
        if records.index(r) <= root_terminal_index:
            raise ValueError("Muse background command must be accepted after root terminal")
        validated_ids.append(cid)

    # Collect usage records (any record with usage or token in payload_type)
    usage = [r for r in records if re.search(r"usage|token", str(r.get("payload_type", "")), re.I)]

    # Count model/tool proposals for all validated commands
    proposals = [r.get("payload", {}) for r in records if r.get("payload_type") == "task.lifecycle.proposed"]
    kinds = [p.get("event", {}).get("task_kind", "") for p in proposals if p.get("command_id") in validated_ids]

    # Resolve the configured model across all validated commands
    configured = [r.get("payload", {}) for r in records if r.get("payload_type") == "run.model.configured"
                  and r.get("payload", {}).get("command_id") in validated_ids]
    model_names = {c.get("model_id") for c in configured if isinstance(c.get("model_id"), str) and c["model_id"]}
    resolved_model = next(iter(model_names)) if len(model_names) == 1 else None

    return {"root_command_id": root_cid, "native_usage_records": usage,
            "native_count_scope": "validated_command_proposals",
            "turns": sum(k.startswith("model.") for k in kinds),
            "tool_calls": sum(k.startswith("tool.") for k in kinds),
            "resolved_model": resolved_model, "resolved_model_source": "run.model.configured" if resolved_model else "not reported in characterized JSONL"}

def load_frozen(folder):
    names = ("_muse", "_cli_profile", "_cli_terminal", "_cli_adapter", "_muse_adapter")
    for name in names:
        if not (folder / (name + ".py")).is_file():
            continue
        spec = importlib.util.spec_from_file_location(name, folder / (name + ".py"))
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    return sys.modules["_muse_adapter"]


def execute(folder, ws, real_home, binary, env, protected, identity, prompt, timeout, on_started=None, extra=(), memory_cap_mb=None, on_evidence=None):
    """Called from the frozen module; the model invocation uses its bytes."""
    if identity.get("provider") != "meta":
        from _cli_adapter import execute as execute_profile
        return execute_profile(folder, ws, real_home, binary, env, protected, identity, prompt, timeout, on_started, extra, memory_cap_mb, on_evidence)
    profile = load_profile(folder / "muse-profile.json")
    from _cli_pins import load_pins
    entry = load_pins(os.environ).get("muse-code") or {}
    if entry.get("sha256") != identity["binary_sha256"] or entry.get("version") != identity["version"]:
        raise ValueError("profile changed during preparation")
    if sha(binary) != identity["binary_sha256"]:
        raise ValueError("Muse binary changed after capture")
    for path, digest in identity["config_files"].items():
        if sha(ws / path) != digest:
            raise ValueError("staged Muse configuration changed")
    keychain = ws / ".lab-tmp/muse/Library/Keychains"
    if (not keychain.is_symlink() or str(keychain.readlink()) != identity.get("keychain_link")
            or identity.get("keychain_link") != str(real_home / "Library/Keychains")):
        raise ValueError("Keychain link changed after capture")
    _muse.check_environment(env)
    command = [str(binary), "exec", "--provider", "meta", "--model", profile["model"], "--json", "--workspace", str(ws),
               "--worktree", "off", "--max-model-steps", str(profile["max_model_steps"]),
               "--reasoning-effort", profile["reasoning_effort"], "--no-foreign-personal-context",
               "--no-session-log", "--approval-mode", "never", "--approval-judge", "off",
               "--disable-web-tools", "--disable-sandbox", prompt]
    bounded = _muse.sandbox_command(command, ws, real_home, protected, extra=extra)
    return _muse.run_process(bounded, ws, env, timeout, on_started=on_started, memory_cap_mb=memory_cap_mb, on_evidence=on_evidence)


def check_command(check, workspace, real_home, protected=(), extra=(), tmp=None):
    """Sandbox command for the frozen task check.

    Extra allows are emitted before the estate deny, so they do not keep a path
    under the state root readable. The stimulus directory and the check tmp are
    read roots, the same grant run_check_sandboxed gives the other CLIs. The
    agent's profile, including an OpenCode Go login-store deny, is not used here.
    """
    stim = Path(check).resolve().parent
    roots = [stim]
    caller = dict(extra or {})
    grants = list(caller.get("caller_grants", []))
    if tmp is not None:
        tmp_path = Path(tmp).resolve()
        # A read root may never contain the personal home (the sandbox refuses it); such a tmp keeps the plain grant.
        if not Path(real_home).resolve().is_relative_to(tmp_path):
            roots.append(tmp_path)
        grants.append(dict(path=str(tmp_path), access="read_write"))
    grants.append(dict(path=str(stim), access="read"))
    caller["caller_grants"] = grants
    return _muse.sandbox_command([os.path.abspath(str(check))], workspace, real_home, protected,
                                 extra=caller, read_roots=roots)


def run(c):
    started = time.monotonic()
    started_wall = time.time()
    ws, run_dir, here = c["ws"].resolve(), c["run_dir"], c["HERE"]
    def process_evidence(rows):
        from _agent_stream import append
        for evidence in rows:
            append(run_dir / "process-evidence.jsonl", evidence)
    real_home = c["HOME"].resolve()
    name = c["name"]
    muse = name == "muse-code"
    lane = c.get("lane")
    from _estate_io import native_caller
    sandbox_extra = {}
    if lane:
        sandbox_extra, _ = native_caller({}, real_home, run_dir, ws, run_dir / "tmp", lane)
        sandbox_extra.pop("run_protection", None)
    cli_tmp = Path(c.get("cli_tmp") or Path(run_dir) / "tmp")
    grants = [dict(path=str(root), access="read") for root in
              declared_home_reads(c.get("tmpl") or {}, real_home, cli_tmp)]
    if lane:
        common = _lanes.git_common_dir(Path(lane["main"]))
        grants += [dict(path=str(p), access="metadata") for p in common.resolve().parents
                   if p.is_relative_to(real_home)]
    sandbox_extra["caller_grants"] = grants
    c = {**c, "sandbox_extra": sandbox_extra}
    bundle = BUNDLE + ("_cli_profile.py",) if muse else ("_executor.py", "executor-templates.json", "_muse.py", "_muse_adapter.py",
                                 "_cli_adapter.py", "_cli_pins.py", "_cli_profile.py", "_cli_terminal.py", "cli-profiles.json")
    row = {"ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
           "spec": c["spec_rel"], "spec_sha": c["spec_sha"](c["spec_path"]),
           "task": task_label(c["task"], c["LAB"]), "trial": c["trial"],
           "model": name, "cost_class": c["cost_class"], "cost_usd": 0.0,
           "auth": c["auth"], "auth_source": "unverified", "auth_verified": False,
           "auth_verification": "metadata_only; paid entitlement not asserted",
           "est_usd": None, "est_basis": "chars", "est_rate_known": False,
           "estimated_input_chars": 0, "note": "unpriced estimated-cost shadow; no published rate pinned",
           "turns": 0, "tool_calls": 0, "cli_invoked": False, "native_probe_count": 0,
           "base_sha": c["base"], "transcript": transcript_id(c["run_id"])}
    memory_cap_mb = c.get("memory_cap_mb")
    if memory_cap_mb is not None:
        row["memory_cap_bytes"] = memory_cap_mb * 1024 * 1024
    if lane:
        row["lane"] = {k: lane[k] for k in ("repo", "lane", "path", "branch", "base", "dirty_at_start")}
    if c["gid"]:
        row["grant"] = c["gid"]
    if c["frozen_inputs"]:
        row["frozen_inputs"] = c["frozen_inputs"]
    capture = {"run_dir": str(run_dir), "run_id": c["run_id"], "task": str(c["task"]),
               "spec": c["spec_rel"], "spec_sha": row["spec_sha"],
               "trial": c["trial"], "adapter": "executor", "workspace": str(ws),
               "grant": c["gid"], "grant_known": True, "authority": "work order",
               "executor_files": ["ops/bin/" + n for n in bundle if (here / n).is_file()]}
    captured = False

    def freeze(identity):
        nonlocal captured
        result, error = c["run_op"](c["LAB"], "capture-stimulus", {**capture, "executor_identity": identity})
        if error:
            raise ValueError("capture-stimulus: " + error)
        row.update({k: result[k] for k in ("task_sha", "env_sha", "stimulus_sha")})
        captured = True

    def finish(verdict, stop, stdout="", stderr="", check=None, detect_provider=False):
        # Completion owns the same durable record and outbox envelope as the standard executor.
        signal.signal(signal.SIGTERM, lambda *_: None)
        if detect_provider:
            hit = detect_limit(name, name, stdout, stderr, row.get("allowance"))
            if hit:
                stop = "provider_limit"
                row.update(allowance=enrich_allowance(row.get("allowance"), hit), reason=hit["message"])
                try:
                    record_limit(state_dir(c["LAB"]), name, hit)
                except OSError:
                    pass
        # The Keychain link and staged DB copies serve the CLI only; evidence and harvest never keep them.
        keychain = ws / ".lab-tmp/muse/Library/Keychains"
        if keychain.is_symlink():
            keychain.unlink()
        for db_file in (ws / ".lab-tmp").glob("*/data/opencode/opencode.db*"):
            if db_file.is_file() or db_file.is_symlink():
                db_file.unlink()
        if not captured:
            try:
                freeze({"preflight": "refused", "reason": stop})
            except ValueError as error:
                stderr += "\n" + str(error)
        log = "stdout:\n%s\nstderr:\n%s\nstop=%s\n" % (stdout, stderr, stop)
        persist_output(run_dir / "cli.log", log)
        persist_output(run_dir / "transcript.md", log)
        _run_record.footer(run_dir, stop, verdict, row)
        result, error = c["run_op"](c["LAB"], "publish-evidence", {
            "run_dir": str(run_dir), "adapter": "executor", "workspace": str(ws), "verdict": verdict, "stop": stop,
            "check_skipped": check is None, "check_exit": check.returncode if check else None,
            "check_output": redact(((check.stdout or "") + (check.stderr or "")) if check else stderr),
            **({"archive": "changed_files", "lane_git": {"main": lane["main"], "lane": lane["lane"]}} if lane else {})})
        if error:
            verdict, stop = "fail", "publish_evidence"
            row["note"] += "; " + error
        else:
            row.update({k: result[k] for k in ("evidence_sha", "capture_state")})
        if verdict == "pass" and row.get("capture_state") != "complete":
            verdict, stop = "fail", "evidence_incomplete"
        if verdict == "pass" and lane:
            try:
                row["lane"]["commit"] = _lanes.commit_lane(
                    ws, "lane %s: %s (%s)" % (lane["lane"], c["task"].name, c["run_id"]),
                    main=lane["main"], lane=lane["lane"])
                row["branch"] = lane["branch"]
            except _lanes.LaneError as error:
                verdict, stop = "fail", "harvest"
                row["note"] += "; " + str(error)
        elif verdict == "pass":
            result, error = c["run_op"](c["LAB"], "harvest", {"workspace": str(ws),
                "slug": re.sub(r"[^a-z0-9-]", "", c["task"].name.lower()), "run_id": c["run_id"]})
            if error:
                verdict, stop = "fail", "harvest"
                row["note"] += "; " + error
            else:
                row["branch"] = result["branch"]
        row.update(verdict=verdict, stop=stop, seconds=time.monotonic() - started)
        c["append_ledger"](row)
        window = c.get("check_window", 60)
        _run_record.check_json(run_dir, row.get("stimulus_sha"), window,
                               check.returncode if check is not None else None,
                               ((check.stdout or "") + (check.stderr or "")) if check is not None else "",
                               verdict=verdict)
        _run_record.terminal(run_dir, ws, row, c["run_id"], c["spec_rel"], str(c["task"]), started_wall, window)
        finalized = _run_record.finalize(c["LAB"], run_dir,
            {k: v for k, v in os.environ.items() if not c["CRED_ENV"].search(k)})
        state = finalized.get("state") or "pending"
        print(json.dumps({"ok": verdict == "pass", "run_id": c["run_id"], **row, "finalize": state}))
        raise SystemExit(3 if state != "finalized" else 0 if verdict == "pass" else 1)

    def finish_resource(result):
        meta = result.forced_meta
        for key in ("peak_rss_bytes", "reason"):
            if meta.get(key) is not None:
                row[key] = meta[key]
        finish("fail", str(meta.get("stop") or "memory_cap"), result.stdout, result.stderr)

    try:
        _muse.check_environment(os.environ)
        source_hashes = {part: sha(here / part) for part in bundle}
        if muse:
            from _cli_pins import ensure_pin, min_version_ok, resolve_binary
            profile = load_profile(here / "muse-profile.json")
            binary = resolve_binary(profile["binary"], real_home)
            overrides, protected = stage_home(ws, real_home, profile)
            row.update(auth_source="oauth_keychain", auth_metadata_verified=True)
            env = {k: v for k, v in c["allowlist_env"]([], ws / ".lab-tmp", ws).items()
                   if not k.startswith(("MUSE_", "TBH_", "META_", "XDG_"))}
            env.update(overrides)
            from _cli_native import stage_git
            env.update(stage_git(cli_tmp, real_home))
            _muse.check_environment(env)
            def probe(args):
                row["native_probe_count"] += 1
                command = _muse.sandbox_command([str(binary), *args], ws, real_home, protected, extra=sandbox_extra)
                result = _muse.run_process(command, ws, env, min(c["timeout"], 15), memory_cap_mb=memory_cap_mb, on_evidence=process_evidence)
                _muse.require_resources(result)
                if result.returncode:
                    raise ValueError("Muse identity/config probe failed: " + result.stderr)
                return result.stdout
            version = probe(["--version"]).strip()
            if not min_version_ok(profile.get("min_version"), version):
                raise ValueError("Muse version below profile minimum")
            ensure_pin("muse-code", binary, version, env=os.environ)
            generation = config_generation(probe(["config", "status"]))
            identity = {"version": version, "binary_sha256": sha(binary), "binary_path": str(binary),
                        "config_generation": generation, "provider": "meta",
                        "home_mode": "private_home_keychain_link", "personal_context": "os_read_denied",
                        "keychain_link": str(real_home / "Library/Keychains"),
                        "config_files": {str(p.relative_to(ws)): sha(p) for p in protected if p.is_file()}}
        else:
            from _cli_adapter import prepare
            binary, env, protected, identity = prepare({**c, "on_probe": lambda: row.update(native_probe_count=1)})
            row.update(auth_source=identity["auth_source"], auth_metadata_verified=True)
            if c["cost_class"] == "free_local":
                row.update(est_usd=0.0, est_rate_known=True, note="local backend; zero wallet and estimated cost")
        freeze(identity)
        folder = run_dir / "stimulus/executor/ops/bin"
        if any(sha(folder / name) != value for name, value in source_hashes.items()):
            raise ValueError("executor bundle changed during preparation")
        frozen = load_frozen(folder)
        goal = (run_dir / "stimulus/goal.md").read_text().strip()
        prompt = "Implement the task in the current working directory. Do not leave this directory. Do not commit unless asked.\n\n" + goal
        row["estimated_input_chars"] = len(prompt)
        row["muse_identity" if muse else "cli_identity"] = identity
    except _muse.ForcedStop as error:
        finish_resource(error.result)
    except (ValueError, RuntimeError, OSError, KeyError, TypeError, AttributeError, subprocess.TimeoutExpired, _muse.Cancelled) as error:
        stop = ("timeout" if isinstance(error, subprocess.TimeoutExpired) else
                "cancelled" if isinstance(error, _muse.Cancelled) else "muse_preflight" if muse else "cli_preflight")
        finish("fail" if stop in ("timeout", "cancelled") else "void", stop, stderr=str(error))
    try:
        call_started = time.monotonic()
        cli = frozen.execute(folder, ws, real_home, binary, env, protected, identity, prompt, c["timeout"],
                             on_started=lambda: row.update(cli_invoked=True), extra=sandbox_extra, memory_cap_mb=memory_cap_mb, on_evidence=process_evidence)
        row["per_call_latency_ms"] = (time.monotonic() - call_started) * 1000
        row["cli_exit"] = cli.returncode
        row["builtin_skills_status"] = ("disabled_by_sandbox" if
            "Built-in skills were disabled for this startup" in cli.stderr else "no_disable_warning")
    except subprocess.TimeoutExpired as error:
        finish("fail", "timeout", str(error.stdout or ""), str(error.stderr or ""))
    except (_muse.Cancelled, sys.modules["_muse"].Cancelled) as error:
        finish("fail", "cancelled", error.stdout or "", error.stderr or "")
    except (ValueError, OSError, RuntimeError) as error:
        finish("fail", "muse_launch" if muse else "cli_launch", getattr(error, "stdout", ""), str(error))
    if getattr(cli, "forced_meta", None):
        finish_resource(cli)
    if cli.returncode:
        if not cli.stdout.strip() and "runtime host failed to start:" in cli.stderr:
            finish("void", "muse_runtime_unavailable", cli.stdout, cli.stderr)
        if "missing meta credentials" in cli.stderr or "saved login is no longer valid" in cli.stderr:
            finish("void", "muse_auth_unavailable", cli.stdout, cli.stderr)
        finish("fail", "executor_exit_%s" % cli.returncode, cli.stdout, cli.stderr, detect_provider=True)
    try:
        if muse:
            row.update(frozen.validate_root_terminal(cli.stdout))
        else:
            from _cli_terminal import terminal
            row.update(terminal(name, cli.stdout))
            row.update(resolved_model=identity["model"], resolved_model_source="pinned isolated profile")
        row.update(auth_verified=True, auth_verification=(
            "native meta command completed using OAuth/Keychain; paid entitlement not asserted" if muse else
            "native command completed with pinned local backend" if c["cost_class"] == "free_local" else
            "native command completed with its own %s login; paid entitlement not asserted" % c["auth"]))
    except (ValueError, TypeError, KeyError, AttributeError) as error:
        finish("fail", "muse_terminal" if muse else "cli_terminal", cli.stdout, cli.stderr + "\n" + str(error), detect_provider=True)
    try:
        check_cmd = check_command(run_dir / "stimulus/check", ws, real_home, protected,
                                  sandbox_extra, c["child_env"]["TMPDIR"])
        check = _muse.run_process(check_cmd, ws, c["child_env"], c["timeout"], memory_cap_mb=memory_cap_mb, on_evidence=process_evidence)
    except (_muse.Cancelled, sys.modules["_muse"].Cancelled) as error:
        finish("fail", "cancelled", cli.stdout, cli.stderr + "\n" + str(error))
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        finish("fail", "check_timeout" if isinstance(error, subprocess.TimeoutExpired) else "check_error", cli.stdout, cli.stderr + "\n" + str(error))
    if getattr(check, "forced_meta", None):
        finish_resource(check)
    if check.returncode:
        finish("fail", "check", cli.stdout, cli.stderr, check)
    if c["grant_paths"]:
        bad = [p for p in c["changed_paths"](ws, c["base"], lane) if not c["under_grant"](p, c["grant_paths"])]
        if bad:
            finish("fail", "grant_paths", cli.stdout, "out-of-grant paths: " + ", ".join(bad), check)
    finish("pass", "done", cli.stdout, cli.stderr, check)
