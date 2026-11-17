"""Additional behaviour coverage for py-core-units (90% gate helpers)."""
import json
import os
import subprocess
import sys
import time
import unittest
from pathlib import Path
from unittest.mock import patch, MagicMock

HERE = Path(__file__).resolve().parent
LIB = HERE.parent / "_lib"
OPS = HERE.parents[2] / "ops" / "bin"
FIX = HERE.parent / "provider-limits" / "fixtures"
sys.path[:0] = [str(LIB), str(OPS)]

from hermetic_env import hermetic_env  # noqa: E402
from scratch import scratch_dir  # noqa: E402

import _estate_io  # noqa: E402
import _provider_limits  # noqa: E402
import _toolchains  # noqa: E402


def clear_toolchain_caches():
    for fn in ("discover", "discover_path_dirs", "discover_sandbox_links"):
        getattr(_toolchains, fn).cache_clear()


class ToolchainEnvAndSubprocessTests(unittest.TestCase):
    def setUp(self):
        clear_toolchain_caches()

    def tearDown(self):
        clear_toolchain_caches()

    def test_discover_path_dirs_lists_resolved_bin_roots(self):
        with scratch_dir(prefix="tc-path-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            bindir.mkdir()
            (bindir / "php").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (bindir / "php").chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{"name": "php", "bin": str(bindir)}]), encoding="utf-8")
            env = {**hermetic_env(), "HOME": str(root), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                dirs = _toolchains.discover_path_dirs()
                self.assertIn(str(bindir.resolve()), dirs)

    def test_cargo_lane_env_symlinks_host_registry_and_sets_offline_flags(self):
        with scratch_dir(prefix="tc-cargo-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            bindir.mkdir()
            reg = root / "registry"
            reg.mkdir()
            tc = root / "toolchain/fake"
            tc.mkdir(parents=True)
            rustup = root / "rustup"
            rustup.mkdir()
            (bindir / "cargo").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (bindir / "cargo").chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{
                "name": "cargo", "bin": str(bindir),
                "rustup": str(rustup), "toolchain": str(tc), "registry": str(reg),
            }]), encoding="utf-8")
            env = {**hermetic_env(), "HOME": str(root), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                lane = root / "lane-tmp"
                ws = root / "workspace"
                ws.mkdir()
                out = _toolchains.cargo_lane_env(lane, ws)
                self.assertEqual(out["CARGO_NET_OFFLINE"], "true")
                self.assertEqual(out["RUSTUP_TOOLCHAIN"], tc.name)
                link = lane / "cargo-home" / "registry"
                self.assertTrue(link.is_symlink())
                self.assertIn("target", out["CARGO_TARGET_DIR"])

    def test_toolchain_lane_env_merges_uv_and_cargo_when_both_declared(self):
        with scratch_dir(prefix="tc-merge-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            pydir = root / "python"
            bindir.mkdir()
            pydir.mkdir()
            tc = root / "tc"
            tc.mkdir()
            rustup = root / "rustup"
            rustup.mkdir()
            (bindir / "uv").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (bindir / "uv").chmod(0o755)
            (bindir / "cargo").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (bindir / "cargo").chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([
                {"name": "uv", "bin": str(bindir), "python": str(pydir)},
                {"name": "cargo", "bin": str(bindir), "rustup": str(rustup), "toolchain": str(tc)},
            ]), encoding="utf-8")
            env = {**hermetic_env(), "HOME": str(root), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                merged = _toolchains.toolchain_lane_env(root / "tmp", root / "ws")
                self.assertIn("UV_CACHE_DIR", merged)
                self.assertIn("CARGO_HOME", merged)

    def test_uv_python_dir_reads_subprocess_stdout(self):
        with scratch_dir(prefix="tc-uvpy-") as raw:
            root = Path(raw)
            pyroot = root / "uv-python"
            pyroot.mkdir()
            uv = root / "uv"
            uv.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            uv.chmod(0o755)
            done = subprocess.CompletedProcess([], 0, str(pyroot) + "\n", "")
            with patch("_toolchains.subprocess.run", return_value=done):
                got = _toolchains.uv_python_dir(uv)
            self.assertEqual(got, pyroot.resolve())

    def test_cargo_toolchain_paths_collects_sysroot_rustup_and_registry(self):
        with scratch_dir(prefix="tc-cargop-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            bindir.mkdir()
            sysroot = root / "sysroot"
            sysroot.mkdir()
            cargo = bindir / "cargo"
            rustc = bindir / "rustc"
            for b in (cargo, rustc):
                b.write_text("#!" + sys.executable + "\nimport sys\n"
                             "if '--print' in sys.argv: print(%r); sys.exit(0)\n" % str(sysroot),
                             encoding="utf-8")
                b.chmod(0o755)
            reg = root / ".cargo/registry"
            reg.mkdir(parents=True)
            env = {**hermetic_env(), "HOME": str(root), "CARGO_HOME": str(root / ".cargo")}
            with patch.dict(os.environ, env, clear=True):
                got = _toolchains.cargo_toolchain_paths(cargo)
            self.assertIsNotNone(got)
            self.assertEqual(got["toolchain"], sysroot.resolve())
            self.assertIn("registry", got)

    def test_has_uv_and_cargo_reflect_config_entries(self):
        with scratch_dir(prefix="tc-has-") as raw:
            root = Path(raw)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([
                {"name": "uv", "bin": str(root / "b"), "python": str(root / "p")},
            ]), encoding="utf-8")
            (root / "b").mkdir()
            (root / "p").mkdir()
            env = {**hermetic_env(), "HOME": str(root), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                self.assertTrue(_toolchains.has_uv_toolchain())
                self.assertFalse(_toolchains.has_cargo_toolchain())

    def test_doctor_findings_flags_uv_missing_python_directory(self):
        with scratch_dir(prefix="tc-doc-uv-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            bindir.mkdir()
            (bindir / "uv").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (bindir / "uv").chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{"name": "uv", "bin": str(bindir), "python": str(root / "missing")}]),
                            encoding="utf-8")
            env = {**hermetic_env(), "HOME": str(root), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                findings = _toolchains.doctor_findings(cfg)
                self.assertTrue(any("python directory" in f.get("detail", "") for f in findings))

    def test_entries_falls_back_to_path_lookup_when_config_missing(self):
        with scratch_dir(prefix="tc-which-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            bindir.mkdir()
            php = bindir / "php"
            php.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            php.chmod(0o755)
            env = {**hermetic_env(), "HOME": str(root), "PATH": str(bindir), "CS_TOOLCHAINS_FILE": str(root / "nope.json")}
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                entries = _toolchains._entries()
                self.assertTrue(any(e.get("name") == "php" for e in entries))


class EstateIoTransportTests(unittest.TestCase):
    def test_utc_time_normalizes_offsets_and_refuses_garbage(self):
        self.assertEqual(_estate_io.utc_time("2026-09-08T12").isoformat(), "2026-09-08T12:00:00+00:00")
        self.assertEqual(_estate_io.utc_time("2026-09-08T14:00:00+02:00").isoformat(), "2026-09-08T12:00:00+00:00")
        self.assertIsNone(_estate_io.utc_time("broken"))

    def test_basename_digest_and_sha256_match_transport_contract(self):
        self.assertEqual(_estate_io.basename(" /a/b/ "), "b")
        self.assertEqual(
            _estate_io.digest({"b": 2, "a": 1}),
            _estate_io.sha256_bytes(b'{"a":1,"b":2}'),
        )

    def test_estate_id_and_aliases_follow_cutover_record(self):
        with scratch_dir(prefix="est-est-") as raw:
            root = Path(raw)
            lab = root / "lab"
            state = root / "state"
            lab.mkdir()
            state.mkdir()
            (lab / "INVARIANTS.md").write_text("#\n", encoding="utf-8")
            (lab / "ESTATE").write_text("prod\n", encoding="utf-8")
            record = {"schema": "estate-cutover-v2", "estate": "prod", "aliases": ["dev"]}
            (state / _estate_io.CUTOVER_V2).write_text(json.dumps(record), encoding="utf-8")
            env = {**hermetic_env(), "LAB_CODE_ROOT": str(lab), "LAB_STATE_ROOT": str(state)}
            with patch.dict(os.environ, env, clear=True):
                self.assertEqual(_estate_io.estate_id(lab), "prod")
                self.assertEqual(_estate_io.estate_aliases(lab), ("dev",))
                self.assertTrue(_estate_io.estate_matches(lab, "dev"))
                self.assertEqual(_estate_io.cutover_record(state), record)

    def test_estate_id_refuses_invalid_estate_file(self):
        with scratch_dir(prefix="est-badid-") as raw:
            lab = Path(raw) / "lab"
            lab.mkdir()
            (lab / "ESTATE").write_text("INVALID\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                _estate_io.estate_id(lab)

    def test_pid_live_tracks_child_process(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        try:
            self.assertTrue(_estate_io.pid_live(child.pid))
        finally:
            child.terminate()
            child.wait()
        self.assertFalse(_estate_io.pid_live(child.pid))

    def test_run_op_and_call_op_obey_child_exit_contract(self):
        with scratch_dir(prefix="est-op-") as raw:
            root = Path(raw)
            ops = root / "ops" / "bin"
            ops.mkdir(parents=True)
            child = ops / "child"
            child.write_text("#!" + sys.executable + "\nimport json,sys; print(json.dumps(json.load(sys.stdin)))\n",
                             encoding="utf-8")
            child.chmod(0o755)
            body, err = _estate_io.run_op(root, "child", {"n": 1})
            self.assertIsNone(err)
            self.assertEqual(body["n"], 1)
            self.assertEqual(_estate_io.call_op(root, "child", {"n": 2})["n"], 2)


class ProviderLimitsDetectTests(unittest.TestCase):
  def test_detect_limit_uses_fixture_stderr_patterns(self):
      if not FIX.is_dir():
          self.skipTest("provider-limits fixtures absent")
      cursor = (FIX / "cursor-stderr.txt").read_text(encoding="utf-8")
      hit = _provider_limits.detect_limit("cursor-sol", "cursor-agent", "", cursor)
      self.assertIsNotNone(hit)
      gw = (FIX / "gateway-quota.txt").read_text(encoding="utf-8")
      self.assertEqual(_provider_limits.detect_limit("claude-code", "claude", gw, "")["via"], "gateway")
      gem = (FIX / "gemini-stderr.txt").read_text(encoding="utf-8")
      self.assertIsNotNone(_provider_limits.detect_limit("gemini-cli", "gemini", "", gem))

  def test_limit_from_event_handles_terminal_quota_and_allowance(self):
      ev = {"error": {"type": "TerminalQuotaError", "message": "usage limit exceeded", "code": 429}}
      hit = _provider_limits.limit_from_event(ev)
      self.assertTrue(hit and hit["terminal"])
      allowance = {"rejected": True, "via": "broker", "windows": [{"resets_at": "2099-01-01T00:00:00Z"}]}
      got = _provider_limits.allowance_hit(allowance)
      self.assertIn("allowance", got["message"])
      enriched = _provider_limits.enrich_allowance(allowance, got)
      self.assertTrue(enriched.get("rejected"))

  def test_detect_grok_requires_native_402_balance_exhausted(self):
      if not (FIX / "grok-limit.jsonl").is_file():
          self.skipTest("grok fixture absent")
      text = (FIX / "grok-limit.jsonl").read_text(encoding="utf-8")
      hit = _provider_limits.detect_grok(text, "")
      self.assertIsNotNone(hit)
      self.assertEqual(hit["via"], "grok_error")

  def test_detect_opencode_uses_retry_after_for_reset(self):
      stream = json.dumps({"type": "error", "error": {
          "type": "provider.go-limit", "status": 429, "message": "limit", "retryAfterMs": 60000,
      }})
      with patch("_provider_limits.time.time", return_value=1_700_000_000):
          hit = _provider_limits.detect_opencode(stream)
      self.assertIsNotNone(hit)
      self.assertIsNotNone(hit["resets_at"])

  def test_load_returns_empty_templates_on_corrupt_file(self):
      with scratch_dir(prefix="pl-bad-") as raw:
          state = Path(raw)
          bad = _provider_limits.state_path(state)
          bad.parent.mkdir(parents=True, exist_ok=True)
          bad.write_text("not json\n", encoding="utf-8")
          data = _provider_limits.load(state)
          self.assertEqual(data, {"templates": {}})

  def test_limit_signal_ignores_non_claude_dialect(self):
      self.assertIsNone(_provider_limits.limit_signal("codex", b'{"type":"error"}'))

  def test_detect_limit_generic_quota_on_stderr_only(self):
      hit = _provider_limits.detect_limit("claude-code", "claude", "", "rate limit exceeded on API")
      self.assertIsNotNone(hit)
      self.assertEqual(hit["via"], "stderr")


if __name__ == "__main__":
    unittest.main()


class EstateIoRelocatedAndFailureTests(unittest.TestCase):
    def test_iter_jsonl_yields_nothing_when_path_is_missing(self):
        rows = list(_estate_io.iter_jsonl(Path("/nonexistent/x.jsonl"), strict=True, errors="strict", objects_only=True))
        self.assertEqual(rows, [])

    def test_link_target_returns_non_symlink_unchanged(self):
        with scratch_dir() as raw:
            p = Path(raw) / "plain"
            p.write_text("x", encoding="utf-8")
            self.assertEqual(_estate_io.link_target(Path(raw), p), p)

    def test_estate_dir_uses_config_root_when_lab_config_root_set(self):
        with scratch_dir(prefix="est-cfg-") as raw:
            root = Path(raw)
            code = root / "code"
            config = root / "config"
            code.mkdir()
            config.mkdir()
            (code / "INVARIANTS.md").write_text("#\n", encoding="utf-8")
            env = {**hermetic_env(), "LAB_CODE_ROOT": str(code), "LAB_CONFIG_ROOT": str(config)}
            with patch.dict(os.environ, env, clear=True):
                self.assertEqual(Path(_estate_io.estate_dir(code)).resolve(), Path(config).resolve())

    def test_estate_row_relocated_state_stamps_and_refuses_mismatch(self):
        with scratch_dir(prefix="est-reloc-") as raw:
            root = Path(raw)
            config = root / "config"
            state = root / "state"
            config.mkdir()
            state.mkdir()
            (config / "ESTATE").write_text("prod\n", encoding="utf-8")
            (state / "runs.jsonl").touch()
            shelf = state / "runs.jsonl"
            record = {"schema": "estate-cutover-v2", "estate": "prod", "aliases": ["dev"]}
            (state / _estate_io.CUTOVER_V2).write_text(json.dumps(record), encoding="utf-8")
            code = root / "code"
            code.mkdir()
            (code / "INVARIANTS.md").write_text("#\n", encoding="utf-8")
            env = {
                **hermetic_env(),
                "LAB_CODE_ROOT": str(code),
                "LAB_CONFIG_ROOT": str(config),
                "LAB_STATE_ROOT": str(state),
            }
            with patch.dict(os.environ, env, clear=True):
                row = _estate_io.estate_row(shelf, {"estate": "dev", "n": 1})
                self.assertEqual(row["estate"], "prod")
                with self.assertRaises(ValueError):
                    _estate_io.estate_row(shelf, {"estate": "other-estate"})

    def test_run_op_surfaces_child_failure_messages(self):
        with scratch_dir(prefix="est-fail-") as raw:
            root = Path(raw)
            ops = root / "ops/bin"
            ops.mkdir(parents=True)
            child = ops / "fail"
            child.write_text("#!" + sys.executable + "\nimport sys; print('stderr', file=sys.stderr); sys.exit(3)\n",
                             encoding="utf-8")
            child.chmod(0o755)
            body, err = _estate_io.run_op(root, "fail", {})
            self.assertIsNone(body)
            self.assertEqual(err, "stderr")
            with self.assertRaises(RuntimeError):
                _estate_io.call_op(root, "fail", {}, failure="boom")


class ToolchainEdgeCaseTests(unittest.TestCase):
    def setUp(self):
        clear_toolchain_caches()

    def tearDown(self):
        clear_toolchain_caches()

    def test_binary_roots_expands_tilde_and_follows_shell_paths(self):
        with scratch_dir(prefix="tc-shell-") as raw:
            root = Path(raw)
            target = root / "nested/prog"
            target.parent.mkdir(parents=True)
            target.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            target.chmod(0o755)
            launcher = root / "bin/wrap"
            launcher.parent.mkdir(parents=True)
            launcher.write_text('#!/bin/sh\nDIR="%s"\nOTHER="%s"\n' % (target.parent, target), encoding="utf-8")
            launcher.chmod(0o755)
            env = {**hermetic_env(), "HOME": str(root)}
            with patch.dict(os.environ, env, clear=True):
                roots = _toolchains.binary_roots(launcher)
                self.assertTrue(len(roots) >= 2)

    def test_validate_uv_entry_requires_python_directory(self):
        with scratch_dir(prefix="tc-val-uv-") as raw:
            root = Path(raw)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{"name": "uv", "bin": str(root / "b"), "python": "relative"}]),
                            encoding="utf-8")
            (root / "b").mkdir()
            env = {**hermetic_env(), "HOME": str(root), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                with self.assertRaises(ValueError):
                    _toolchains.discover()

    def test_entries_rejects_non_list_toolchains_json(self):
        with scratch_dir(prefix="tc-notlist-") as raw:
            root = Path(raw)
            cfg = root / "toolchains.json"
            cfg.write_text("{}", encoding="utf-8")
            env = {**hermetic_env(), "HOME": str(root), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                with self.assertRaises(ValueError):
                    _toolchains._entries()

    def test_grantable_refuses_home_and_contextstack_paths(self):
        with scratch_dir(prefix="tc-grant-") as raw:
            root = Path(raw)
            home = root / "home"
            home.mkdir()
            cs = home / ".contextstack/state"
            cs.mkdir(parents=True)
            tool = cs / "bin/tool"
            tool.parent.mkdir(parents=True, exist_ok=True)
            tool.write_text("x", encoding="utf-8")
            env = {**hermetic_env(), "HOME": str(home)}
            with patch.dict(os.environ, env, clear=True):
                self.assertFalse(_toolchains._grantable(cs))
                self.assertFalse(_toolchains._grantable(home))

    def test_uv_python_dir_returns_none_on_failure(self):
        with patch("_toolchains.subprocess.run", side_effect=OSError("nope")):
            self.assertIsNone(_toolchains.uv_python_dir(Path("/tmp/uv")))


class ProviderLimitsEdgeTests(unittest.TestCase):
    def test_parse_us_date_accepts_two_digit_year(self):
        self.assertEqual(_provider_limits._parse_us_date("11/17/26"), "2026-11-17T00:00:00Z")

    def test_iso_epoch_accepts_numeric_timestamp(self):
        self.assertTrue(_provider_limits._iso_epoch(1700000000).startswith("2023-"))

    def test_limit_from_event_ignores_provider_dot_types(self):
        ev = {"error": {"type": "provider.quota", "message": "usage limit"}}
        self.assertIsNone(_provider_limits.limit_from_event(ev))

    def test_detect_grok_stderr_internal_error_json(self):
        msg = (
            'Internal error: {"message": "API error (status 402 Payment Required): '
            'Grok Build usage balance exhausted", "http_status": 402}'
        )
        hit = _provider_limits.detect_grok("", msg)
        self.assertIsNotNone(hit)

    def test_detect_limit_routes_grok_template_by_name(self):
        line = json.dumps({
            "type": "error",
            "message": "API error (status 402): Grok Build usage balance exhausted",
        })
        hit = _provider_limits.detect_limit("grok", "grok", line, "")
        self.assertIsNotNone(hit)

    def test_is_limited_true_when_no_resets_at_in_row(self):
        limited, _ = _provider_limits.is_limited("codex", [{"template": "codex"}], now=time.time())
        self.assertTrue(limited)

    def test_pick_fallback_returns_none_when_chain_exhausted(self):
        profile = {"fallback": ["codex"]}
        rows = [{"template": "codex", "resets_at": "2099-01-01T00:00:00Z"}]
        self.assertIsNone(_provider_limits.pick_fallback("codex", profile, rows))

    def test_load_strips_non_dict_template_rows(self):
        with scratch_dir() as raw:
            state = Path(raw)
            path = _provider_limits.state_path(state)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({"templates": {"bad": "x", "ok": {
                "message": "m", "resets_at": "2099-01-01T00:00:00Z", "recorded_at": "2026-01-01T00:00:00Z",
            }}}), encoding="utf-8")
            data = _provider_limits.load(state)
            self.assertNotIn("bad", data["templates"])



class ProviderLimitsMoreDetectTests(unittest.TestCase):
    def test_detect_limit_scans_combined_stream_objects_for_quota(self):
        blob = "noise\n" + json.dumps({"error": {"code": 429, "message": "insufficient_quota"}}) + "\n"
        hit = _provider_limits.detect_limit("claude-code", "claude", blob, "")
        self.assertIsNotNone(hit)

    def test_refuse_message_without_row_uses_unknown_time(self):
        msg = _provider_limits.refuse_message("codex", None, None)
        self.assertIn("unknown time", msg)


class ToolchainDoctorCargoTests(unittest.TestCase):
    def setUp(self):
        clear_toolchain_caches()

    def tearDown(self):
        clear_toolchain_caches()

    def test_doctor_findings_reports_missing_cargo_rustup_and_registry(self):
        with scratch_dir(prefix="tc-doc-cargo-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            bindir.mkdir()
            (bindir / "cargo").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (bindir / "cargo").chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{
                "name": "cargo", "bin": str(bindir),
                "rustup": str(root / "missing-rustup"),
                "toolchain": str(root / "missing-tc"),
                "registry": str(root / "missing-reg"),
            }]), encoding="utf-8")
            env = {**hermetic_env(), "HOME": str(root), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                findings = _toolchains.doctor_findings(cfg)
                details = " ".join(f.get("detail", "") for f in findings)
                self.assertIn("rustup", details)
                self.assertIn("registry", details)

    def test_discover_skips_non_grantable_uv_python_under_home(self):
        with scratch_dir(prefix="tc-uvhome-") as raw:
            root = Path(raw)
            home = root / "home"
            home.mkdir()
            bindir = home / "bin"
            py = home / ".contextstack/python"
            bindir.mkdir()
            py.mkdir(parents=True)
            (bindir / "uv").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (bindir / "uv").chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{"name": "uv", "bin": str(bindir), "python": str(py)}]), encoding="utf-8")
            env = {**hermetic_env(), "HOME": str(home), "CS_TOOLCHAINS_FILE": str(cfg)}
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                self.assertEqual(_toolchains.discover(), ())

    def test_limit_signal_returns_none_for_non_json_line(self):
        self.assertIsNone(_provider_limits.limit_signal("claude", b"not json"))

    def test_limit_signal_returns_none_for_non_claude_dialect(self):
        self.assertIsNone(_provider_limits.limit_signal("codex", b'{"x":1}'))

    def test_detect_opencode_skips_non_error_events(self):
        stream = json.dumps({"type": "text", "part": {"text": "usage limit"}})
        self.assertIsNone(_provider_limits.detect_opencode(stream))

    def test_detect_grok_skips_lines_without_required_prefix(self):
        msg = '{"message": "API error (status 402): Grok Build usage balance exhausted"}'
        self.assertIsNone(_provider_limits.detect_grok("", msg))

    def test_is_limited_treats_unparseable_resets_as_active(self):
        limited, _ = _provider_limits.is_limited(
            "codex", [{"template": "codex", "resets_at": "not-a-date"}], now=0,
        )
        self.assertTrue(limited)

    def test_load_applies_unknown_reset_from_recorded_at(self):
        with scratch_dir() as raw:
            state = Path(raw)
            path = _provider_limits.state_path(state)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({"templates": {"x": {
                "message": "m", "via": "t",
                "recorded_at": "2020-01-01T00:00:00Z",
            }}}), encoding="utf-8")
            _provider_limits.load(state)
            self.assertNotIn("x", _provider_limits.load(state).get("templates", {}))

class ProviderLimitsFinalCoverageTests(unittest.TestCase):
    def test_parse_us_date_returns_none_for_garbage(self):
        self.assertIsNone(_provider_limits._parse_us_date("not-a-date"))

    def test_iso_epoch_returns_stripped_string_timestamp(self):
        self.assertEqual(_provider_limits._iso_epoch("  2026-01-01T00:00:00Z  "), "2026-01-01T00:00:00Z")

    def test_limit_from_event_returns_none_for_empty_dict(self):
        self.assertIsNone(_provider_limits.limit_from_event({}))

    def test_limit_signal_returns_none_for_non_object_line(self):
        self.assertIsNone(_provider_limits.limit_signal("claude", b"hello"))

    def test_limit_signal_returns_none_for_invalid_json(self):
        self.assertIsNone(_provider_limits.limit_signal("claude", b"{not-json"))

    def test_detect_limit_returns_none_when_no_signal_present(self):
        self.assertIsNone(_provider_limits.detect_limit("codex", "codex", "done", ""))

    def test_detect_limit_uses_opencode_stdout_branch(self):
        stream = json.dumps({
            "type": "error",
            "error": {"type": "provider.go-limit", "message": "limit", "retryAfterMs": 5000},
        })
        with patch("_provider_limits.time.time", return_value=1_700_000_000):
            hit = _provider_limits.detect_limit("opencode-go", "opencode", stream, "")
        self.assertIsNotNone(hit)

    def test_detect_limit_uses_allowance_when_rejected(self):
        allowance = {"rejected": True, "via": "meter", "windows": [{"resets_at": "2099-01-01T00:00:00Z"}]}
        hit = _provider_limits.detect_limit("codex", "codex", "", "", allowance=allowance)
        self.assertEqual(hit["via"], "meter")

    def test_detect_gemini_requires_ineligible_marker(self):
        self.assertIsNone(_provider_limits.detect_gemini("benign stderr"))

    def test_detect_opencode_tolerates_garbage_lines_before_error(self):
        stream = "not-json\n" + json.dumps({
            "type": "error",
            "error": {"type": "provider.usage-limit", "status": 429, "message": "usage limit"},
        })
        hit = _provider_limits.detect_opencode(stream)
        self.assertIsNotNone(hit)

    def test_detect_grok_accepts_plain_text_after_internal_error_prefix(self):
        stderr = (
            "Internal error: API error (status 402 Payment Required): "
            "Grok Build usage balance exhausted\n"
        )
        hit = _provider_limits.detect_grok("", stderr)
        self.assertIsNotNone(hit)

    def test_load_returns_empty_templates_when_file_is_not_object(self):
        with scratch_dir() as raw:
            state = Path(raw)
            path = _provider_limits.state_path(state)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("[]", encoding="utf-8")
            self.assertEqual(_provider_limits.load(state), {"templates": {}})

    def test_active_templates_skips_non_dict_rows(self):
        with scratch_dir() as raw:
            state = Path(raw)
            path = _provider_limits.state_path(state)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({"templates": {"ok": {
                "message": "m", "resets_at": "2099-01-01T00:00:00Z", "recorded_at": "2026-01-01T00:00:00Z",
            }, "bad": "string"}}), encoding="utf-8")
            active = _provider_limits.active_templates(state)
            self.assertEqual(len(active), 1)

