"""Behaviour coverage for _lane_policy, _stuck and _cli_budget (py-core-units)."""
import json
import os
import subprocess
import sys
import unittest
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
LIB = HERE.parent / "_lib"
OPS = HERE.parents[2] / "ops" / "bin"
sys.path[:0] = [str(LIB), str(OPS)]

from hermetic_env import hermetic_env  # noqa: E402
from scratch import scratch_dir  # noqa: E402

import _cli_budget  # noqa: E402
import _lane_policy  # noqa: E402
import _stuck  # noqa: E402


class GitFixture:
    """Hermetic git repo; git_out matches refuse_agent_configs contract."""

    def __init__(self, root):
        self.root = Path(root)
        self.env = {
            **hermetic_env(),
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_TERMINAL_PROMPT": "0",
        }
        subprocess.run(
            ["git", "-C", str(self.root), "init", "-q", "-b", "main"],
            env=self.env,
            check=True,
        )
        (self.root / "README").write_text("seed\n", encoding="utf-8")
        self.run("add", "-A")
        self.run("commit", "-q", "-m", "seed")

    def run(self, *args):
        proc = subprocess.run(
            [
                "git", "-C", str(self.root),
                "-c", "user.name=py-core-units",
                "-c", "user.email=units@example.test",
                *args,
            ],
            capture_output=True,
            text=True,
            env=self.env,
        )
        if proc.returncode != 0:
            raise RuntimeError("git %s: %s" % (args, proc.stderr))
        return proc.stdout.strip()

    def git_out(self, _main, *args):
        return self.run(*args)


class PolicyError(Exception):
    pass


class LanePolicyTests(unittest.TestCase):
    def test_matches_protected_honours_nested_agent_paths(self):
        patterns = [p.casefold() for p in _lane_policy.AGENT_TOOL_CONFIG_PATHS]
        self.assertTrue(_lane_policy._matches_protected("nested/.claude/settings.json", patterns))
        self.assertFalse(_lane_policy._matches_protected("src/app.py", patterns))

    def test_lane_change_rev_uses_three_dot_merge_base_range(self):
        with scratch_dir(prefix="lp-rev-") as raw:
            repo = GitFixture(raw)
            repo.run("checkout", "-q", "-b", "lane")
            (repo.root / "ordinary.txt").write_text("lane\n", encoding="utf-8")
            repo.run("add", "ordinary.txt")
            repo.run("commit", "-q", "-m", "lane change")
            repo.run("checkout", "-q", "main")
            rev = _lane_policy.lane_change_rev(repo.git_out, repo.root, "lane")
            head = repo.run("rev-parse", "HEAD").strip()
            self.assertEqual(rev, "%s...lane" % head)

    def test_lane_changed_names_lists_only_lane_side_paths(self):
        with scratch_dir(prefix="lp-names-") as raw:
            repo = GitFixture(raw)
            repo.run("checkout", "-q", "-b", "lane")
            (repo.root / "src").mkdir()
            (repo.root / "src/only-lane.txt").write_text("x\n", encoding="utf-8")
            repo.run("add", "-A")
            repo.run("commit", "-q", "-m", "lane")
            repo.run("checkout", "-q", "main")
            names = [n for n in _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane") if n]
            self.assertIn("src/only-lane.txt", names)

    def test_refuse_agent_configs_blocks_protected_file_on_lane(self):
        with scratch_dir(prefix="lp-block-") as raw:
            repo = GitFixture(raw)
            repos = Path(raw) / "repos.json"
            repos.write_text(json.dumps({"policy": {"path": str(repo.root.resolve())}}), encoding="utf-8")
            repo.run("checkout", "-q", "-b", "lane")
            (repo.root / "AGENTS.md").write_text("# agent\n", encoding="utf-8")
            repo.run("add", "AGENTS.md")
            repo.run("commit", "-q", "-m", "protected")
            repo.run("checkout", "-q", "main")
            names = _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane")
            with self.assertRaises(PolicyError) as ctx:
                _lane_policy.refuse_agent_configs(
                    repo.root.resolve(), names, repo.git_out, PolicyError, repos, branch="lane",
                )
            self.assertIn("AGENTS.md", str(ctx.exception))

    def test_refuse_agent_configs_allows_ordinary_lane_changes(self):
        with scratch_dir(prefix="lp-ok-") as raw:
            repo = GitFixture(raw)
            repos = Path(raw) / "repos.json"
            repos.write_text("{}", encoding="utf-8")
            repo.run("checkout", "-q", "-b", "lane")
            (repo.root / "notes.txt").write_text("notes\n", encoding="utf-8")
            repo.run("add", "notes.txt")
            repo.run("commit", "-q", "-m", "ordinary")
            repo.run("checkout", "-q", "main")
            names = _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane")
            _lane_policy.refuse_agent_configs(
                repo.root.resolve(), names, repo.git_out, PolicyError, repos, branch="lane",
            )

    def test_refuse_agent_configs_extends_patterns_from_core_hooks_path(self):
        with scratch_dir(prefix="lp-hooks-") as raw:
            repo = GitFixture(raw)
            custom = repo.root / "custom-hooks"
            custom.mkdir()
            (custom / "pre-commit").write_text("#!/bin/sh\n", encoding="utf-8")
            repo.run("add", "-A")
            repo.run("commit", "-q", "-m", "seed hooks")
            repo.run("config", "core.hooksPath", "custom-hooks")
            repos = Path(raw) / "repos.json"
            repos.write_text("{}", encoding="utf-8")
            repo.run("checkout", "-q", "-b", "lane")
            (custom / "pre-commit").write_text("#!/bin/sh\necho lane\n", encoding="utf-8")
            repo.run("add", "-A")
            repo.run("commit", "-q", "-m", "hook change")
            repo.run("checkout", "-q", "main")
            names = _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane")
            with self.assertRaises(PolicyError):
                _lane_policy.refuse_agent_configs(
                    repo.root.resolve(), names, repo.git_out, PolicyError, repos, branch="lane",
                )

    def test_refuse_agent_configs_blocks_symlink_hiding_vscode_layout(self):
        with scratch_dir(prefix="lp-symlink-") as raw:
            repo = GitFixture(raw)
            repos = Path(raw) / "repos.json"
            repos.write_text(json.dumps({"policy": {"path": str(repo.root.resolve())}}), encoding="utf-8")
            repo.run("checkout", "-q", "-b", "lane")
            editor = repo.root / "editor"
            editor.mkdir()
            (editor / "tasks.json").write_text("{}\n", encoding="utf-8")
            os.symlink("editor", repo.root / ".vscode")
            repo.run("add", "-A")
            repo.run("commit", "-q", "-m", "vscode alias")
            repo.run("checkout", "-q", "main")
            names = _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane")
            with self.assertRaises(PolicyError) as ctx:
                _lane_policy.refuse_agent_configs(
                    repo.root.resolve(), names, repo.git_out, PolicyError, repos, branch="lane",
                )
            reason = str(ctx.exception).lower()
            self.assertTrue("symlink" in reason or ".vscode" in reason, str(ctx.exception))

    def test_refuse_agent_configs_blocks_symlink_target_outside_repository(self):
        with scratch_dir(prefix="lp-outside-") as raw:
            repo = GitFixture(raw)
            outside = Path(raw) / "outside.txt"
            outside.write_text("secret\n", encoding="utf-8")
            repos = Path(raw) / "repos.json"
            repos.write_text("{}", encoding="utf-8")
            repo.run("checkout", "-q", "-b", "lane")
            rel_out = os.path.relpath(outside, repo.root)
            os.symlink(rel_out, repo.root / "escape.link")
            repo.run("add", "escape.link")
            repo.run("commit", "-q", "-m", "escape")
            repo.run("checkout", "-q", "main")
            names = _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane")
            with self.assertRaises(PolicyError) as ctx:
                _lane_policy.refuse_agent_configs(
                    repo.root.resolve(), names, repo.git_out, PolicyError, repos, branch="lane",
                )
            self.assertIn("outside", str(ctx.exception).lower())

    def test_refuse_agent_configs_allows_plain_in_repo_file_symlink(self):
        with scratch_dir(prefix="lp-plain-link-") as raw:
            repo = GitFixture(raw)
            repos = Path(raw) / "repos.json"
            repos.write_text("{}", encoding="utf-8")
            repo.run("checkout", "-q", "-b", "lane")
            (repo.root / "src").mkdir()
            (repo.root / "src/notes.txt").write_text("notes\n", encoding="utf-8")
            os.symlink("src/notes.txt", repo.root / "alias.txt")
            repo.run("add", "-A")
            repo.run("commit", "-q", "-m", "alias")
            repo.run("checkout", "-q", "main")
            names = _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane")
            _lane_policy.refuse_agent_configs(
                repo.root.resolve(), names, repo.git_out, PolicyError, repos, branch="lane",
            )


class ChangedState:
    def __init__(self, **files):
        self.files = dict(files)

    def __call__(self, paths=None):
        selected = paths if paths is not None else self.files
        return {path: self.files.get(path, "missing") for path in selected}


def make_monitor(**overrides):
    config = {"enabled": True, "repeat": 99, "idle_turns": 99, "edit_churn": 99}
    config.update(overrides)
    state = ChangedState(**{"app.py": "A"})
    return _stuck.Monitor(config, state), state


class StuckBudgetParsingTests(unittest.TestCase):
    def test_parse_budget_stuck_defaults_when_missing(self):
        cfg = _stuck.parse_budget_stuck({})
        self.assertTrue(cfg["enabled"])
        self.assertEqual(cfg["repeat"], _stuck.DEFAULT_REPEAT)

    def test_parse_budget_stuck_accepts_off_sentinel(self):
        self.assertFalse(_stuck.parse_budget_stuck({"stuck": False})["enabled"])
        self.assertFalse(_stuck.parse_budget_stuck({"stuck": "off"})["enabled"])

    def test_parse_budget_stuck_rejects_unknown_keys_and_bad_integers(self):
        with self.assertRaises(ValueError):
            _stuck.parse_budget_stuck({"stuck": "maybe"})
        with self.assertRaises(ValueError):
            _stuck.parse_budget_stuck({"stuck": {"extra": 1}})
        with self.assertRaises(ValueError):
            _stuck.parse_budget_stuck({"stuck": {"repeat": 0}})

    def test_parse_budget_stuck_merges_positive_overrides(self):
        cfg = _stuck.parse_budget_stuck({"stuck": {"repeat": 5, "idle_turns": 10, "edit_churn": 2}})
        self.assertEqual(cfg, {"enabled": True, "repeat": 5, "idle_turns": 10, "edit_churn": 2, "writes": True})


class StuckMonitorTests(unittest.TestCase):
    def _failed_check(self, mon, ident, text="same failure"):
        self.assertIsNone(mon.feed([
            {"kind": "tool_call", "id": ident, "name": "bash", "args": {"command": "pytest -q"}},
        ]))
        return mon.feed([{"kind": "tool_result", "id": ident, "text": text, "failed": True}])

    def test_monitor_trips_repeat_after_consecutive_matching_failures(self):
        mon, _state = make_monitor(repeat=3)
        self.assertIsNone(self._failed_check(mon, "r1"))
        self.assertIsNone(self._failed_check(mon, "r2"))
        stopped = self._failed_check(mon, "r3")
        self.assertIsNotNone(stopped)
        self.assertEqual(stopped["stop"], "stuck:repeat")

    def test_monitor_resets_repeat_counts_after_passing_check(self):
        mon, _state = make_monitor(repeat=3)
        self.assertIsNone(self._failed_check(mon, "a"))
        self.assertIsNone(self._failed_check(mon, "b"))
        self.assertIsNone(mon.feed([
            {"kind": "tool_call", "id": "green", "name": "bash", "args": {"command": "pytest -q"}},
        ]))
        self.assertIsNone(mon.feed([
            {"kind": "tool_result", "id": "green", "failed": False, "text": "120 passed"},
        ]))
        self.assertIsNone(self._failed_check(mon, "after-green"))
        self.assertIsNone(mon.stopped)

    def test_monitor_trips_edit_churn_when_content_returns_to_earlier_hash(self):
        mon, state = make_monitor(edit_churn=2)
        self.assertIsNone(mon.feed([
            {"kind": "tool_call", "id": "eb", "name": "write", "args": {"path": "app.py"}},
        ]))
        state.files["app.py"] = "B"
        self.assertIsNone(mon.feed([{"kind": "tool_result", "id": "eb", "failed": False, "text": "ok"}]))
        self.assertIsNone(mon.feed([
            {"kind": "tool_call", "id": "ea", "name": "write", "args": {"path": "app.py"}},
        ]))
        state.files["app.py"] = "A"
        stopped = mon.feed([{"kind": "tool_result", "id": "ea", "failed": False, "text": "ok"}])
        self.assertEqual(stopped["stop"], "stuck:edit_churn")
        self.assertIn("earlier state", stopped["reason"])

    def test_monitor_trips_idle_turns_without_state_or_edit_activity(self):
        mon, _state = make_monitor(idle_turns=2)
        self.assertIsNone(mon.feed([{"kind": "turn"}]))
        stopped = mon.feed([{"kind": "turn"}])
        self.assertEqual(stopped["stop"], "stuck:idle_turns")

    def test_read_only_config_skips_idle_turns_but_still_trips_repeat(self):
        mon, _state = make_monitor(idle_turns=2, repeat=2, writes=False)
        self.assertIsNone(mon.feed([{"kind": "turn"}]))
        self.assertIsNone(mon.feed([{"kind": "turn"}]))
        self.assertIsNone(mon.feed([{"kind": "turn"}]))
        self.assertIsNone(mon.stopped)
        self.assertIsNone(self._failed_check(mon, "ro1"))
        stopped = self._failed_check(mon, "ro2")
        self.assertEqual(stopped["stop"], "stuck:repeat")

    def test_writing_job_still_trips_idle_turns(self):
        mon, _state = make_monitor(idle_turns=2, writes=True)
        self.assertIsNone(mon.feed([{"kind": "turn"}]))
        stopped = mon.feed([{"kind": "turn"}])
        self.assertEqual(stopped["stop"], "stuck:idle_turns")

    def test_resolve_stuck_writes_read_only_signals(self):
        self.assertFalse(_stuck.resolve_stuck_writes(agent_writes=False))
        self.assertFalse(_stuck.resolve_stuck_writes(writes=False))
        self.assertFalse(_stuck.resolve_stuck_writes(repo_read_only=True))
        self.assertFalse(_stuck.resolve_stuck_writes(read_only_check=True))
        self.assertFalse(_stuck.resolve_stuck_writes(has_grant=False, agent_named=True))
        self.assertTrue(_stuck.resolve_stuck_writes(agent_writes=True, has_grant=False, agent_named=True))
        self.assertTrue(_stuck.resolve_stuck_writes(has_grant=False, agent_named=False))
        self.assertTrue(_stuck.resolve_stuck_writes(has_grant=True))

    def test_agent_profile_writes_reads_frontmatter(self):
        with scratch_dir(prefix="stuck-agents-") as raw:
            root = Path(raw)
            (root / "researcher.md").write_text(
                "---\nname: researcher\nwrites: false\nlane: false\n---\nbody\n", encoding="utf-8")
            (root / "coder.md").write_text(
                "---\nname: coder\nwrites: true\nlane: true\n---\nbody\n", encoding="utf-8")
            self.assertFalse(_stuck.agent_profile_writes(root, "researcher"))
            self.assertTrue(_stuck.agent_profile_writes(root, "coder"))
            self.assertIsNone(_stuck.agent_profile_writes(root, "missing"))

    def test_monitor_feed_ignored_when_disabled_in_config(self):
        mon, _state = make_monitor(repeat=1)
        mon.config["enabled"] = False
        mon.feed([{"kind": "turn"}])
        self.assertIsNone(mon.stopped)

    def test_monitor_report_and_write_report_cap_output_and_list_tools(self):
        mon, _state = make_monitor(repeat=1)
        stopped = self._failed_check(mon, "one", text="traceback: boom")
        self.assertIsNotNone(stopped)
        report = mon.report("# Goal line\n\nDetail", stopped["stop"], stopped["reason"])
        self.assertIn("Goal:", report)
        self.assertIn("Recent tool calls", report)
        self.assertLessEqual(len(report.encode("utf-8")), _stuck.REPORT_CHARS)
        with scratch_dir(prefix="stuck-rpt-") as raw:
            path = Path(raw) / "stuck.md"
            _stuck.write_report(path, report)
            self.assertEqual(path.read_text(encoding="utf-8"), report)


class CliBudgetValidationTests(unittest.TestCase):
    def test_declared_reads_turn_limits_and_default_check_window(self):
        budget = {"turns": 3, "tool_calls": 7, "usd": 0.25, "seconds": 60}
        spec = {"budget": budget}
        self.assertEqual(_cli_budget.declared(spec), (budget, 600))

    def test_declared_rejects_incomplete_or_non_finite_budgets(self):
        cases = (
            {"budget": {}},
            {"budget": {"turns": 3}},
            {"budget": {"turns": 3, "tool_calls": 7, "usd": float("nan"), "seconds": 60}},
            {"budget": {"turns": 0, "tool_calls": 7, "usd": 1.0, "seconds": 60}},
        )
        for spec in cases:
            with self.assertRaises(ValueError):
                _cli_budget.declared(spec)

    def test_validate_budget_mapping_accepts_none_and_refuses_extra_keys(self):
        _cli_budget.validate_budget_mapping(None)
        with self.assertRaises(ValueError):
            _cli_budget.validate_budget_mapping({
                "turns": 1, "tool_calls": 1, "usd": 0.0, "seconds": 1, "extra": 1,
            })

    def test_admission_denies_after_cap_and_marks_refused(self):
        admission = _cli_budget.Admission(2)
        self.assertEqual(admission.request(), {})
        self.assertEqual(admission.request(), {})
        denied = admission.request()
        self.assertFalse(denied["continue"])
        self.assertEqual(denied["stopReason"], "budget:tool_calls")
        self.assertTrue(admission.refused)

    def test_broker_admits_only_cap_concurrent_consumes(self):
        broker = _cli_budget.Broker(5)
        try:
            port = broker.server.server_address[1]

            def ask(_):
                return json.load(urllib.request.urlopen(
                    urllib.request.Request("http://127.0.0.1:%d/consume" % port, data=b""),
                    timeout=5,
                ))

            with ThreadPoolExecutor(max_workers=8) as pool:
                answers = list(pool.map(ask, range(20)))
            self.assertEqual(sum(1 for a in answers if a == {}), 5)
            self.assertEqual(broker.admission.count, 5)
            with self.assertRaises(urllib.error.HTTPError):
                urllib.request.urlopen(
                    urllib.request.Request("http://127.0.0.1:%d/reset" % port, data=b""),
                )
        finally:
            broker.close()

    def test_broker_hook_fails_closed_when_server_stopped(self):
        with scratch_dir(prefix="cbd-hook-") as raw:
            broker = _cli_budget.Broker(1)
            hook_cmd = broker.hook(raw)
            hook_path = Path(raw) / "cli-budget-hook.py"
            first = subprocess.run(
                [sys.executable, str(hook_path)],
                capture_output=True,
                text=True,
                env=hermetic_env(),
            )
            self.assertEqual(json.loads(first.stdout), {})
            second = subprocess.run(
                [sys.executable, str(hook_path)],
                capture_output=True,
                text=True,
                env=hermetic_env(),
            )
            self.assertFalse(json.loads(second.stdout)["continue"])
            broker.close()
            third = subprocess.run(
                [sys.executable, str(hook_path)],
                capture_output=True,
                text=True,
                env=hermetic_env(),
            )
            self.assertEqual(third.returncode, 2)
            self.assertIn("unavailable", json.loads(third.stdout)["stopReason"])
            self.assertTrue(hook_cmd)

    def test_claude_args_replaces_caps_and_injects_budget_hook(self):
        budget = {"turns": 3, "tool_calls": 5, "usd": 0.5, "seconds": 60}
        args = _cli_budget.claude_args(
            ["--print", "--max-turns", "99", "--max-budget-usd", "99", "task"],
            budget,
            "python3 hook.py",
        )
        self.assertEqual(args[args.index("--max-turns") + 1], "3")
        self.assertEqual(args[args.index("--max-budget-usd") + 1], "0.5")
        settings = json.loads(args[args.index("--settings") + 1])
        self.assertFalse(settings["disableAllHooks"])
        self.assertEqual(len(settings["hooks"]["PreToolUse"]), 1)

    def test_claude_args_refuses_unreviewed_template_hooks(self):
        budget = {"turns": 1, "tool_calls": 1, "usd": 0.0, "seconds": 1}
        with self.assertRaises(ValueError):
            _cli_budget.claude_args(
                ["--settings", '{"hooks": {"PreToolUse": [{"matcher": "*"}]}}'],
                budget,
                "hook",
            )

    def test_native_result_classifies_terminal_outcomes(self):
        budget = {"turns": 3, "tool_calls": 7, "usd": 0.25, "seconds": 60}
        admission = _cli_budget.Admission(7)
        valid = {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "num_turns": 2,
            "result": "done",
            "total_cost_usd": 0.01,
        }
        self.assertEqual(
            _cli_budget.native_result(json.dumps(valid), budget, admission)["stop"],
            "done",
        )
        over_turns = dict(valid, subtype="error_max_turns")
        self.assertEqual(
            _cli_budget.native_result(json.dumps(over_turns), budget, admission)["stop"],
            "budget:turns",
        )

    def test_native_terminal_accepts_stream_json_final_result_line(self):
        payload = {
            "type": "result",
            "subtype": "success",
            "num_turns": 1,
            "result": "ok",
            "total_cost_usd": 0,
        }
        parsed = _cli_budget.native_terminal("noise\n" + json.dumps(payload))
        self.assertEqual(parsed["result"], "ok")



class LanePolicyRelativeAndSymlinkTests(unittest.TestCase):
    def test_repo_relative_rejects_parent_escape_and_absolute_outside_root(self):
        self.assertIsNone(_lane_policy._repo_relative(".", "../outside"))
        self.assertEqual(_lane_policy._repo_relative("src", "/abs/file"), "abs/file")

    def test_refuse_agent_configs_blocks_symlink_to_protected_claude_settings(self):
        with scratch_dir(prefix="lp-claude-link-") as raw:
            repo = GitFixture(raw)
            repos = Path(raw) / "repos.json"
            repos.write_text("{}", encoding="utf-8")
            repo.run("checkout", "-q", "-b", "lane")
            claude = repo.root / ".claude"
            claude.mkdir()
            (claude / "settings.json").write_text("{}\n", encoding="utf-8")
            os.symlink(".claude/settings.json", repo.root / "shortcut.json")
            repo.run("add", "-A")
            repo.run("commit", "-q", "-m", "claude shortcut")
            repo.run("checkout", "-q", "main")
            names = _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane")
            with self.assertRaises(PolicyError) as ctx:
                _lane_policy.refuse_agent_configs(
                    repo.root.resolve(), names, repo.git_out, PolicyError, repos, branch="lane",
                )
            self.assertIn("protected", str(ctx.exception).lower())

    def test_refuse_agent_configs_blocks_symlink_pointing_at_protected_directory(self):
        with scratch_dir(prefix="lp-dir-link-") as raw:
            repo = GitFixture(raw)
            repos = Path(raw) / "repos.json"
            repos.write_text("{}", encoding="utf-8")
            repo.run("checkout", "-q", "-b", "lane")
            (repo.root / ".claude").mkdir()
            os.symlink(".claude", repo.root / "alias-claude")
            repo.run("add", "-A")
            repo.run("commit", "-q", "-m", "dir alias")
            repo.run("checkout", "-q", "main")
            names = _lane_policy.lane_changed_names(repo.git_out, repo.root, "lane")
            with self.assertRaises(PolicyError):
                _lane_policy.refuse_agent_configs(
                    repo.root.resolve(), names, repo.git_out, PolicyError, repos, branch="lane",
                )


class StuckMonitorDepthTests(unittest.TestCase):
    def test_monitor_detects_failure_from_exit_code_without_failed_flag(self):
        mon, _state = make_monitor(repeat=2)
        text = "error: assert failed"
        mon.feed([{"kind": "tool_call", "id": "x", "name": "bash", "args": {"command": "pytest -q"}}])
        self.assertIsNone(mon.feed([{"kind": "tool_result", "id": "x", "exit_code": 1, "text": text}]))
        mon.feed([{"kind": "tool_call", "id": "y", "name": "bash", "args": {"command": "pytest -q"}}])
        stopped = mon.feed([{"kind": "tool_result", "id": "y", "exit_code": 1, "text": text}])
        self.assertEqual(stopped["stop"], "stuck:repeat")

    def test_monitor_treats_named_check_tool_as_gate(self):
        mon, _state = make_monitor(repeat=5)
        mon.feed([{"kind": "tool_call", "id": "c", "name": "check", "args": {}}])
        self.assertIsNone(mon.feed([{"kind": "tool_result", "id": "c", "failed": False, "text": "ok"}]))
        self.assertTrue(mon.check_attempted)

    def test_monitor_extracts_edit_paths_from_apply_patch_payload(self):
        config = {"enabled": True, "repeat": 99, "idle_turns": 99, "edit_churn": 2}
        state = ChangedState(**{"patched.py": "A"})
        mon = _stuck.Monitor(config, state)
        patch = "*** Update File: patched.py\n@@\n"
        mon.feed([{"kind": "tool_call", "id": "p", "name": "apply_patch", "args": {"patch": patch}}])
        state.files["patched.py"] = "B"
        mon.feed([{"kind": "tool_result", "id": "p", "failed": False, "text": "ok"}])
        mon.feed([{"kind": "tool_call", "id": "p2", "name": "apply_patch", "args": {"patch": patch}}])
        state.files["patched.py"] = "A"
        stopped = mon.feed([{"kind": "tool_result", "id": "p2", "failed": False, "text": "ok"}])
        self.assertEqual(stopped["stop"], "stuck:edit_churn")

    def test_monitor_returns_same_stop_without_recomputing_after_trip(self):
        mon, _state = make_monitor(repeat=1)
        mon.feed([{"kind": "tool_call", "id": "a", "name": "bash", "args": {"command": "pytest -q"}}])
        first = mon.feed([{"kind": "tool_result", "id": "a", "failed": True, "text": "boom"}])
        second = mon.feed([{"kind": "turn"}])
        self.assertIs(first, second)
        self.assertTrue(second["stop"].startswith("stuck:"))


    def test_monitor_honours_structured_status_over_output_text(self):
        mon, _state = make_monitor(repeat=2)
        fail = "error: boom"
        mon.feed([{"kind": "tool_call", "id": "s1", "name": "bash", "args": {"command": "pytest -q"}}])
        mon.feed([{"kind": "tool_result", "id": "s1", "status": "fail", "text": fail}])
        mon.feed([{"kind": "tool_call", "id": "s2", "name": "bash", "args": {"command": "pytest -q"}}])
        stopped = mon.feed([{"kind": "tool_result", "id": "s2", "status": "fail", "text": fail}])
        self.assertEqual(stopped["stop"], "stuck:repeat")

    def test_monitor_churns_when_editing_after_same_failing_check(self):
        mon, state = make_monitor(edit_churn=2)
        mon.feed([{"kind": "tool_call", "id": "chk", "name": "bash", "args": {"command": "pytest -q"}}])
        mon.feed([{"kind": "tool_result", "id": "chk", "failed": True, "text": "boom failure"}])
        mon.feed([{"kind": "tool_call", "id": "e1", "name": "write", "args": {"path": "app.py"}}])
        state.files["app.py"] = "B"
        mon.feed([{"kind": "tool_result", "id": "e1", "failed": False, "text": "ok"}])
        mon.feed([{"kind": "tool_call", "id": "chk2", "name": "bash", "args": {"command": "pytest -q"}}])
        mon.feed([{"kind": "tool_result", "id": "chk2", "failed": True, "text": "boom failure"}])
        mon.feed([{"kind": "tool_call", "id": "e2", "name": "write", "args": {"path": "app.py"}}])
        state.files["app.py"] = "C"
        stopped = mon.feed([{"kind": "tool_result", "id": "e2", "failed": False, "text": "ok"}])
        self.assertEqual(stopped["stop"], "stuck:edit_churn")

    def test_monitor_report_lists_last_known_files_when_no_edits_recorded(self):
        mon = _stuck.Monitor({"enabled": False}, ChangedState(**{"z.py": "1", "a.py": "2"}))
        report = mon.report("goal", "stuck:idle_turns", "idle")
        self.assertIn("a.py", report)

    def test_monitor_records_edits_from_paths_and_nested_edit_lists(self):
        mon, state = make_monitor()
        args = {"paths": ["one.py"], "edits": [{"path": "two.py"}, {"file": "three.py"}]}
        mon.feed([{"kind": "tool_call", "id": "w", "name": "multiedit", "args": args}])
        for name in ("one.py", "two.py", "three.py"):
            state.files[name] = "v"
        mon.feed([{"kind": "tool_result", "id": "w", "failed": False, "text": "ok"}])
        self.assertGreaterEqual(mon.edit_counts.get("one.py", 0), 1)

    def test_parse_budget_stuck_rejects_non_mapping_value(self):
        with self.assertRaises(ValueError):
            _stuck.parse_budget_stuck({"stuck": 1})


class CliBudgetDepthTests(unittest.TestCase):
    def test_validate_budget_mapping_rejects_non_mapping_and_missing_keys(self):
        with self.assertRaises(ValueError):
            _cli_budget.validate_budget_mapping("nope")
        with self.assertRaises(ValueError):
            _cli_budget.validate_budget_mapping({"turns": 1, "usd": 0.0, "seconds": 1})

    def test_declared_validates_stuck_sub_budget_and_check_window(self):
        spec = {
            "budget": {
                "turns": 2,
                "tool_calls": 2,
                "usd": 0.0,
                "seconds": 2,
                "stuck": {"repeat": 2},
            },
            "check_window": 30,
        }
        limits, window = _cli_budget.declared(spec)
        self.assertEqual(window, 30)
        self.assertEqual(limits["turns"], 2)
        for invalid in (0, -1, 3601, True, 1.5, "30", None):
            with self.subTest(check_window=invalid), self.assertRaises(ValueError):
                _cli_budget.declared(dict(spec, check_window=invalid))

    def test_claude_args_skips_usd_cap_when_budget_usd_is_zero(self):
        budget = {"turns": 2, "tool_calls": 2, "usd": 0.0, "seconds": 60}
        args = _cli_budget.claude_args(["task"], budget, "hook")
        self.assertNotIn("--max-budget-usd", args)

    def test_claude_args_rejects_non_object_settings_json(self):
        budget = {"turns": 1, "tool_calls": 1, "usd": 0.0, "seconds": 1}
        with self.assertRaises(ValueError):
            _cli_budget.claude_args(["--settings", '[]', "task"], budget, "hook")

    def test_native_result_prefers_tool_call_refusal_over_success_subtype(self):
        budget = {"turns": 5, "tool_calls": 2, "usd": 1.0, "seconds": 60}
        admission = _cli_budget.Admission(1)
        admission.request()
        admission.refused = True
        payload = {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "num_turns": 2,
            "result": "done",
            "total_cost_usd": 0.01,
        }
        out = _cli_budget.native_result(json.dumps(payload), budget, admission)
        self.assertEqual(out["stop"], "budget:tool_calls")
        self.assertEqual(out["tool_calls"], 1)

    def test_native_result_flags_usd_cap_when_measured_cost_reaches_limit(self):
        budget = {"turns": 5, "tool_calls": 5, "usd": 0.5, "seconds": 60}
        admission = _cli_budget.Admission(5)
        payload = {
            "type": "result",
            "subtype": "success",
            "is_error": False,
            "num_turns": 2,
            "result": "done",
            "total_cost_usd": 0.5,
        }
        self.assertEqual(
            _cli_budget.native_result(json.dumps(payload), budget, admission)["stop"],
            "budget:usd",
        )

if __name__ == "__main__":
    unittest.main()
