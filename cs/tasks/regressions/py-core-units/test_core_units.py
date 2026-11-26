"""Behaviour unit tests for ops/bin core modules (py-core-units)."""
import calendar
import json
import os
import subprocess
import sys
import time
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
LIB = HERE.parent / "_lib"
OPS = HERE.parents[2] / "ops" / "bin"
sys.path[:0] = [str(LIB), str(OPS)]

from hermetic_env import hermetic_env  # noqa: E402
from scratch import scratch_dir  # noqa: E402

import _estate_io  # noqa: E402
import _provider_limits  # noqa: E402
import _toolchains  # noqa: E402


def clear_toolchain_caches():
    for fn in ("discover", "discover_path_dirs", "discover_sandbox_links"):
        getattr(_toolchains, fn).cache_clear()


class HardenedGitEnvTests(unittest.TestCase):
    def test_strips_git_overrides_and_sets_defaults(self):
        parent = {**hermetic_env(), "GIT_DIR": "/hostile", "GIT_INDEX_FILE": "/hostile-index",
                  "GIT_CONFIG_COUNT": "1", "GIT_PAGER": "hostile", "LC_ALL": "hostile"}
        with patch.dict(os.environ, parent, clear=True):
            env = _estate_io.hardened_git_env()
            self.assertEqual(dict(os.environ), parent)
            self.assertNotIn("GIT_DIR", env)
            self.assertNotIn("GIT_INDEX_FILE", env)
            self.assertEqual({k: v for k, v in env.items() if k.startswith("GIT_")}, {
                "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
                "GIT_TERMINAL_PROMPT": "0", "GIT_PAGER": "cat",
            })
            self.assertEqual(env["LC_ALL"], "C")
            self.assertEqual(env["PATH"], parent["PATH"])
            self.assertEqual(_estate_io.hardened_git_env({"LC_ALL": "custom", "EXTRA": "yes"}),
                             {**env, "LC_ALL": "custom", "EXTRA": "yes"})

    def test_hostile_parent_git_dir_does_not_redirect_temp_repo(self):
        with scratch_dir() as root:
            repo = Path(root) / "repo"
            with patch.dict(os.environ, {**hermetic_env(), "GIT_DIR": str(Path(root) / "missing")}, clear=True):
                env = _estate_io.hardened_git_env()
                subprocess.run(["git", "init", "-q", str(repo)], env=env,
                               check=True, capture_output=True)
                result = _estate_io.git(repo, "rev-parse", "--absolute-git-dir",
                                        env=env, errors="strict")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(Path(result.stdout.strip()).resolve(), (repo / ".git").resolve())


class ToolchainDiscoveryTests(unittest.TestCase):
    def setUp(self):
        clear_toolchain_caches()

    def tearDown(self):
        clear_toolchain_caches()

    def _fixture_env(self, root, extra=None):
        env = {**hermetic_env(), "HOME": str(root), "PATH": "/usr/bin:/bin"}
        if extra:
            env.update(extra)
        return env

    def test_config_file_honours_cs_toolchains_file_override(self):
        with scratch_dir(prefix="tc-cfg-") as raw:
            root = Path(raw)
            cfg = root / "toolchains.json"
            env = self._fixture_env(root, {"CS_TOOLCHAINS_FILE": str(cfg)})
            with patch.dict(os.environ, env, clear=True):
                self.assertEqual(_toolchains.config_file(), cfg)

    def test_discover_empty_config_suppresses_all_roots(self):
        with scratch_dir(prefix="tc-empty-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            bindir.mkdir()
            php = bindir / "php"
            php.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            php.chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text("[]", encoding="utf-8")
            env = self._fixture_env(root, {"CS_TOOLCHAINS_FILE": str(cfg), "PATH": str(bindir)})
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                self.assertEqual(_toolchains.discover(), ())

    def test_discover_follows_symlinked_shell_launcher_into_runtime(self):
        with scratch_dir(prefix="tc-symlink-") as raw:
            root = Path(raw)
            wrapper, runtime = root / "wrapper/bin", root / "runtime/bin"
            wrapper.mkdir(parents=True)
            runtime.mkdir(parents=True)
            real_php = runtime / "php"
            real_php.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            real_php.chmod(0o755)
            launcher = wrapper / "php"
            launcher.write_text(
                '#!/bin/sh\nPHP_BINARY="%s"\nexec "$PHP_BINARY" "$@"\n' % real_php,
                encoding="utf-8",
            )
            launcher.chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{"name": "php", "bin": str(wrapper)}]), encoding="utf-8")
            env = self._fixture_env(root, {"CS_TOOLCHAINS_FILE": str(cfg)})
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                roots = _toolchains.discover()
                self.assertIn(str(wrapper), roots)
                self.assertIn(str(runtime), roots)

    def test_doctor_findings_reports_missing_declared_binary(self):
        with scratch_dir(prefix="tc-miss-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            bindir.mkdir()
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{"name": "php", "bin": str(bindir)}]), encoding="utf-8")
            env = self._fixture_env(root, {"CS_TOOLCHAINS_FILE": str(cfg)})
            with patch.dict(os.environ, env, clear=True):
                findings = _toolchains.doctor_findings(cfg)
                self.assertTrue(findings and not findings[0]["ok"])
                self.assertIn("not found", findings[0]["detail"])

    def test_doctor_findings_refuses_invalid_json(self):
        with scratch_dir(prefix="tc-badjson-") as raw:
            root = Path(raw)
            cfg = root / "toolchains.json"
            cfg.write_text("{broken", encoding="utf-8")
            env = self._fixture_env(root, {"HOME": str(root)})
            with patch.dict(os.environ, env, clear=True):
                findings = _toolchains.doctor_findings(cfg)
                self.assertEqual(len(findings), 1)
                self.assertFalse(findings[0]["ok"])

    def test_uv_lane_env_points_cache_at_lane_tmp_and_python_install_dir(self):
        with scratch_dir(prefix="tc-uv-env-") as raw:
            root = Path(raw)
            bindir = root / "bin"
            pydir = root / "python"
            bindir.mkdir()
            pydir.mkdir()
            (bindir / "uv").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (bindir / "uv").chmod(0o755)
            cfg = root / "toolchains.json"
            cfg.write_text(
                json.dumps([{"name": "uv", "bin": str(bindir), "python": str(pydir)}]),
                encoding="utf-8",
            )
            env = self._fixture_env(root, {"CS_TOOLCHAINS_FILE": str(cfg)})
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                lane_env = _toolchains.uv_lane_env(root / "tmp")
                self.assertEqual(lane_env["UV_CACHE_DIR"], str(root / "tmp/uv-cache"))
                self.assertEqual(lane_env["UV_OFFLINE"], "1")
                self.assertEqual(lane_env["UV_PYTHON_INSTALL_DIR"], str(pydir.resolve()))

    def test_rust_profile_denies_toolchain_writes_and_grants_symlink_metadata(self):
        with scratch_dir(prefix="tc-rules-") as raw:
            root = Path(raw).resolve()  # macOS /var -> /private/var: keep link and home on one side
            bindir = root / "bin"
            bindir.mkdir()
            cargo = bindir / "cargo"
            target = root / "real/cargo"
            target.parent.mkdir(parents=True)
            target.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            target.chmod(0o755)
            cargo.symlink_to(target)
            tc = root / "toolchain"
            tc.mkdir()
            (root / "rustup").mkdir()
            cfg = root / "toolchains.json"
            cfg.write_text(
                json.dumps([
                    {
                        "name": "cargo",
                        "bin": str(bindir),
                        "rustup": str(root / "rustup"),
                        "toolchain": str(tc),
                    }
                ]),
                encoding="utf-8",
            )
            env = self._fixture_env(root, {"CS_TOOLCHAINS_FILE": str(cfg)})
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                from sandbox_profile import render_profile
                rules = render_profile(root, env=env).splitlines()
                joined = "\n".join(rules)
                self.assertIn(f'(deny file-write* (subpath "{tc.resolve()}"))', joined)
                self.assertIn(f'(deny file-write* (literal "{cargo}"))', joined)
                self.assertIn(f'(allow file-read-metadata (literal "{bindir}"))', joined)

    def test_validate_entry_refuses_relative_bin_directory(self):
        with scratch_dir(prefix="tc-rel-") as raw:
            root = Path(raw)
            cfg = root / "toolchains.json"
            cfg.write_text(json.dumps([{"name": "php", "bin": "relative"}]), encoding="utf-8")
            env = self._fixture_env(root, {"CS_TOOLCHAINS_FILE": str(cfg)})
            with patch.dict(os.environ, env, clear=True):
                clear_toolchain_caches()
                with self.assertRaises(ValueError):
                    _toolchains.discover()

    def test_binary_roots_returns_parents_when_binary_is_not_shell_script(self):
        with scratch_dir(prefix="tc-binroots-") as raw:
            root = Path(raw)
            exe = root / "bin/prog"
            exe.parent.mkdir(parents=True)
            exe.write_bytes(b"\x7fELF fake\n")
            roots = _toolchains.binary_roots(exe)
            self.assertIn(exe.parent, roots)


class EstateIoJsonlTests(unittest.TestCase):
    def test_iter_jsonl_skips_blank_lines_and_invalid_json_when_not_strict(self):
        with scratch_dir(prefix="est-jl-") as raw:
            shelf = Path(raw) / "rows.jsonl"
            shelf.write_text('{"ok":1}\n\nbroken\n{"after":2}\n', encoding="utf-8")
            rows = list(_estate_io.iter_jsonl(shelf, strict=False, errors="strict", objects_only=True))
            self.assertEqual(rows, [{"ok": 1}, {"after": 2}])

    def test_read_jsonl_strict_raises_on_partial_invalid_line(self):
        with scratch_dir(prefix="est-strict-") as raw:
            shelf = Path(raw) / "rows.jsonl"
            shelf.write_text('{"before":1}\nbroken\n', encoding="utf-8")
            with self.assertRaises(json.JSONDecodeError):
                _estate_io.read_jsonl(shelf, strict=True)

    def test_bookkeeping_row_skipped_unless_reader_asks(self):
        with scratch_dir(prefix="est-bk-") as raw:
            shelf = Path(raw) / "rows.jsonl"
            row = {"event": "estate_cutover", "from": "a", "to": "b"}
            shelf.write_text(json.dumps({"id": 1}) + "\n" + json.dumps(row) + "\n", encoding="utf-8")
            self.assertEqual(_estate_io.read_jsonl(shelf, strict=True), [{"id": 1}])
            self.assertEqual(
                _estate_io.read_jsonl(shelf, strict=True, with_bookkeeping=True),
                [{"id": 1}, row],
            )
            self.assertTrue(_estate_io.bookkeeping(row))

    def test_append_jsonl_preserves_prefix_and_honours_compact_sort_sync_flags(self):
        with scratch_dir(prefix="est-app-") as raw:
            shelf = Path(raw) / "rows.jsonl"
            shelf.write_bytes(b"prefix\n")
            with patch("_estate_io.os.fsync", wraps=os.fsync) as sync:
                _estate_io.append_jsonl(
                    shelf,
                    {"b": 2, "a": "é"},
                    sort_keys=True,
                    compact=True,
                    sync=True,
                    encoding="utf-8",
                )
                self.assertEqual(sync.call_count, 1)
            body = shelf.read_bytes()
            self.assertTrue(body.startswith(b"prefix\n"))
            self.assertIn(b'"a"', body)

    def test_lab_path_maps_ledger_relative_path_into_relocated_state_root(self):
        with scratch_dir(prefix="est-code_root-") as raw:
            root = Path(raw)
            code_root = root / "code_root"
            state = root / "state"
            code_root.mkdir()
            state.mkdir()
            (code_root / "ledger").mkdir()
            (code_root / "INVARIANTS.md").write_text("# x\n", encoding="utf-8")
            env = {**hermetic_env(), "CS_CODE_ROOT": str(code_root), "CS_STATE_ROOT": str(state)}
            with patch.dict(os.environ, env, clear=True):
                mapped = _estate_io.code_path(code_root, "ledger/setups/x")
                self.assertEqual(Path(mapped).resolve(), (state / "setups/x").resolve())
                self.assertEqual(_estate_io.code_path(code_root, "tasks/y"), code_root / "tasks/y")

    def test_estate_row_stamps_ledger_shelf_rows_with_estate_id(self):
        with scratch_dir(prefix="est-row-") as raw:
            root = Path(raw)
            code_root = root / "code_root"
            code_root.mkdir()
            (code_root / "INVARIANTS.md").write_text("# x\n", encoding="utf-8")
            (code_root / "ESTATE").write_text("devtest\n", encoding="utf-8")
            ledger = code_root / "ledger"
            ledger.mkdir()
            shelf = ledger / "runs.jsonl"
            row = _estate_io.estate_row(shelf, {"job": "j1"})
            self.assertEqual(row["estate"], "devtest")

    def test_estate_row_refuses_foreign_estate_on_ledger_shelf(self):
        with scratch_dir(prefix="est-refuse-") as raw:
            root = Path(raw)
            code_root = root / "code_root"
            code_root.mkdir()
            (code_root / "INVARIANTS.md").write_text("# x\n", encoding="utf-8")
            (code_root / "ESTATE").write_text("devtest\n", encoding="utf-8")
            ledger = code_root / "ledger"
            ledger.mkdir()
            shelf = ledger / "runs.jsonl"
            with self.assertRaises(ValueError):
                _estate_io.estate_row(shelf, {"estate": "other"})

    def test_link_target_resolves_relative_symlink_to_ledger_setup(self):
        with scratch_dir(prefix="est-link-") as raw:
            root = Path(raw)
            code_root = root / "code_root"
            code_root.mkdir()
            (code_root / "INVARIANTS.md").write_text("# x\n", encoding="utf-8")
            target = code_root / "ledger" / "setups" / "x"
            target.mkdir(parents=True)
            link = code_root / "tasks" / "t" / "setup"
            link.parent.mkdir(parents=True)
            link.symlink_to("../../ledger/setups/x")
            env = {**hermetic_env(), "HOME": str(root)}
            with patch.dict(os.environ, env, clear=True):
                resolved = _estate_io.link_target(code_root, link)
                self.assertEqual(resolved, target)


class ProviderLimitsStateTests(unittest.TestCase):
    def test_save_writes_atomically_via_temp_replace(self):
        with scratch_dir(prefix="pl-atomic-") as raw:
            state = Path(raw)
            data = {"templates": {"x": {"message": "m", "recorded_at": "2026-01-01T00:00:00Z"}}}
            _provider_limits.save(state, data)
            path = _provider_limits.state_path(state)
            self.assertTrue(path.is_file())
            self.assertFalse(path.with_suffix(".tmp").exists())
            loaded = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(loaded["templates"]["x"]["message"], "m")

    def test_load_prunes_expired_limit_on_read(self):
        with scratch_dir(prefix="pl-exp-") as raw:
            state = Path(raw)
            active = {"codex": {"resets_at": "2099-01-01T00:00:00Z"}}
            expired = {"cursor-sol": {"resets_at": "2000-01-01T00:00:00Z"}}
            path = _provider_limits.state_path(state)
            _provider_limits.save(state, {"templates": {**active, **expired}})
            self.assertEqual(_provider_limits.load(state)["templates"], active)
            self.assertEqual(json.loads(path.read_text())["templates"], active)

    def test_load_does_not_save_unchanged_limits(self):
        with scratch_dir(prefix="pl-active-") as raw:
            state = Path(raw)
            data = {"templates": {"codex": {"resets_at": "2099-01-01T00:00:00Z"}}}
            _provider_limits.save(state, data)
            with patch.object(_provider_limits, "save") as save:
                self.assertEqual(_provider_limits.load(state), data)
                save.assert_not_called()

    def test_record_limit_round_trips_through_active_templates(self):
        with scratch_dir(prefix="pl-rec-") as raw:
            state = Path(raw)
            hit = {"message": "cap", "resets_at": "2099-06-01T00:00:00Z", "via": "stderr"}
            _provider_limits.record_limit(state, "codex", hit)
            active = _provider_limits.active_templates(state)
            self.assertEqual(len(active), 1)
            self.assertEqual(active[0]["template"], "codex")

    def test_is_limited_honours_future_resets_at(self):
        rows = [{"template": "codex", "resets_at": "2099-01-01T00:00:00Z"}]
        limited, row = _provider_limits.is_limited("codex", rows, now=time.time())
        self.assertTrue(limited)
        self.assertIsNotNone(row)

    def test_is_limited_returns_false_when_reset_passed(self):
        rows = [{"template": "codex", "resets_at": "2000-01-01T00:00:00Z"}]
        limited, row = _provider_limits.is_limited("codex", rows, now=time.time())
        self.assertFalse(limited)
        self.assertIsNone(row)

    def test_pick_fallback_skips_limited_chain_members(self):
        profile = {"fallback": ["cursor-sol", "codex", "cursor-composer"]}
        rows = [{"template": "codex", "resets_at": "2099-01-01T00:00:00Z"}]
        self.assertEqual(_provider_limits.pick_fallback("cursor-sol", profile, rows), "cursor-composer")

    def test_pick_fallback_starts_after_current_model_in_chain(self):
        profile = {"fallback": ["codex", "cursor-composer"]}
        self.assertEqual(_provider_limits.pick_fallback("codex", profile, []), "cursor-composer")

    def test_refuse_message_names_template_reset_and_suggestion(self):
        row = {"resets_at": "2099-06-01T00:00:00Z", "message": "usage cap"}
        msg = _provider_limits.refuse_message("cursor-sol", row, "codex")
        self.assertIn("cursor-sol", msg)
        self.assertIn("2099-06-01", msg)
        self.assertIn("codex", msg)

    def test_unknown_reset_expires_after_unknown_reset_window(self):
        with scratch_dir(prefix="pl-unknown-") as raw:
            state = Path(raw)
            _provider_limits.record_limit(
                state, "antigravity", {"message": "limited", "resets_at": None, "via": "test"},
            )
            data = json.loads(_provider_limits.state_path(state).read_text())
            data["templates"]["antigravity"]["recorded_at"] = time.strftime(
                "%Y-%m-%dT%H:%M:%SZ",
                time.gmtime(time.time() - _provider_limits.UNKNOWN_RESET_S - 60),
            )
            _provider_limits.save(state, data)
            _provider_limits.load(state)
            self.assertEqual([r["template"] for r in _provider_limits.active_templates(state)], [])

    def test_limit_signal_for_claude_rate_limit_event_returns_forced_stop(self):
        line = json.dumps({
            "type": "rate_limit_event",
            "rate_limit_info": {"status": "rejected", "resetsAt": 1893456000, "rateLimitType": "hour"},
        })
        sig = _provider_limits.limit_signal("claude", line.encode())
        self.assertIsNotNone(sig)
        self.assertEqual(sig["stop"], "provider_limit")

    def test_detect_cursor_parses_monthly_reset_from_stderr(self):
        text = "Error: You've hit your usage limit; monthly cycle ends on 11/17/2026"
        hit = _provider_limits.detect_cursor(text)
        self.assertIsNotNone(hit)
        self.assertEqual(hit["resets_at"], "2026-11-17T00:00:00Z")

    def test_status_limits_returns_wrapper_only_when_active(self):
        with scratch_dir(prefix="pl-st-") as raw:
            state = Path(raw)
            self.assertEqual(_provider_limits.status_limits(state), {})
            _provider_limits.record_limit(
                state, "codex", {"message": "m", "resets_at": "2099-01-01T00:00:00Z", "via": "t"},
            )
            st = _provider_limits.status_limits(state)
            self.assertIn("provider_limits", st)
            self.assertEqual(len(st["provider_limits"]), 1)



class WriteAtomicTests(unittest.TestCase):
    def test_scripts_and_ops_bin_copies_are_identical(self):
        # cs/scripts and cs/ops/bin are separate import roots, so the helper ships twice (in ops/bin inside
        # _estate_io, which partial test estates already copy); the two must not drift.
        import ast
        def functions(path):
            tree = ast.parse(path.read_text(encoding="utf-8"))
            return {n.name: ast.dump(n) for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in ("write_atomic", "_fsync_dir")}
        scripts_copy = functions(OPS.parents[1] / "scripts" / "_atomic_write.py")
        ops_copy = functions(OPS / "_estate_io.py")
        self.assertEqual(set(scripts_copy), {"write_atomic", "_fsync_dir"})
        self.assertEqual(scripts_copy, ops_copy)

    def test_bytes_and_text_round_trip(self):
        with scratch_dir(prefix="wa-") as raw:
            root = Path(raw)
            path = root / "data.bin"
            _estate_io.write_atomic(path, b"binary\n", mode=0o600)
            self.assertEqual(path.read_bytes(), b"binary\n")
            self.assertEqual(oct(path.stat().st_mode & 0o777), oct(0o600))
            _estate_io.write_atomic(path, "text\n")
            self.assertEqual(path.read_text(encoding="utf-8"), "text\n")

    def test_failure_before_replace_leaves_destination_intact(self):
        with scratch_dir(prefix="wa-fail-") as raw:
            root = Path(raw)
            path = root / "keep.json"
            path.write_text("original\n", encoding="utf-8")
            real = os.replace
            def boom(src, dst):
                if Path(dst) == path:
                    raise OSError(28, "simulated failure")
                return real(src, dst)
            with patch("os.replace", boom):
                with self.assertRaises(OSError):
                    _estate_io.write_atomic(path, b"partial\n")
            self.assertEqual(path.read_text(encoding="utf-8"), "original\n")
            leftovers = list(root.glob(".*.tmp"))
            self.assertEqual(leftovers, [])

    def test_mode_applied_on_create(self):
        with scratch_dir(prefix="wa-mode-") as raw:
            root = Path(raw)
            path = root / "secret"
            _estate_io.write_atomic(path, b"x\n", mode=0o600)
            self.assertEqual(oct(path.stat().st_mode & 0o777), oct(0o600))



if __name__ == "__main__":
    unittest.main()


class LoginRetryTests(unittest.TestCase):
    def test_keychain_retry_repauses_without_duplicate_notice(self):
        with scratch_dir() as raw:
            root = Path(raw)
            state = root / "state"
            with patch.dict(os.environ, hermetic_env(), clear=True):
                _provider_limits.notice(state, "claude-code", "claude", home=root)
                row = _provider_limits.load(state)["templates"]["claude-code"]
                recorded = calendar.timegm(time.strptime(row["recorded_at"][:19], "%Y-%m-%dT%H:%M:%S"))
                for offset, paused in ((_provider_limits.LOGIN_RETRY_S - 1, True),
                                       (_provider_limits.LOGIN_RETRY_S, False)):
                    with patch("_provider_limits.time.time", return_value=recorded + offset):
                        self.assertEqual("claude-code" in _provider_limits.load(state)["templates"], paused)
                _provider_limits.notice(state, "claude-code", "claude", home=root)
                self.assertIn("claude-code", _provider_limits.load(state)["templates"])
                self.assertEqual(len((state / "attention.jsonl").read_text().splitlines()), 1)

    def test_file_login_stays_paused_until_changed(self):
        with scratch_dir() as raw:
            root = Path(raw)
            login = root / ".claude/.credentials.json"
            login.parent.mkdir()
            login.write_text("decoy")
            state = root / "state"
            _provider_limits.notice(state, "claude-code", "claude", home=root)
            with patch("_provider_limits.time.time", return_value=4000000000):
                self.assertIn("claude-code", _provider_limits.load(state)["templates"])
            login.write_text("decoy-new-login")
            self.assertNotIn("claude-code", _provider_limits.load(state)["templates"])
            persisted = json.loads(_provider_limits.state_path(state).read_text())
            self.assertNotIn("claude-code", persisted["templates"])

    def test_node_reports_ready_templates_and_init_preference(self):
        import _vitals
        with scratch_dir() as raw:
            root = Path(raw)
            (root / "agents.json").write_text(json.dumps([
                {"name": "claude", "path": "/fake/claude", "signed_in": True},
                {"name": "codex", "path": "/fake/codex", "signed_in": False},
                {"name": "cursor-agent", "path": "/fake/cursor", "signed_in": True}]))
            (root / "install.json").write_text(json.dumps({"default_model": "claude-code"}))
            with patch("_vitals.shutil.which", side_effect=lambda p: p if p == "/fake/claude" else None):
                caps = _vitals.agent_capabilities(root)["agent_capabilities"]
            self.assertIn("claude-code", caps["templates"])
            self.assertNotIn("codex", caps["templates"])
            self.assertNotIn("cursor-sol", caps["templates"])
            self.assertEqual(caps["preferred"], "claude-code")
