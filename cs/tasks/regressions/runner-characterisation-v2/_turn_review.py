"""Focused turn-boundary regressions using the characterisation estate and gateway."""

import json
from pathlib import Path
import tempfile
import unittest

import _harness


HERE = Path(__file__).resolve().parent
TEXT = {"ok": {"text": "I see what needs changing."}}
GUIDANCE = "You replied without using a tool. Continue: make your planned edit with fs_edit, then run_check."


class TurnBoundaries(unittest.TestCase):
    def run_turns(self, replies, *, tools="fx_tool, fs_write", hook="", seconds=0,
                  check="exit 0", goal="Fixture goal"):
        with tempfile.TemporaryDirectory(prefix="turn-boundaries-") as directory:
            scratch = Path(directory)
            case = scratch / "case"
            task = case / "task"
            task.mkdir(parents=True)
            (task / "goal.md").write_text(goal + "\n")
            (task / "check").write_text("#!/bin/sh\n" + check + "\n")
            (case / "spec.yaml").write_text(f"""id: fx-turn-boundary
model: fake
system: prompts/coder-min.md
tools: [{tools}]
post_turn: [{hook}]
budget: {{turns: {len(replies)}, seconds: {seconds}, usd: 1, tool_calls: 10}}
tasks: [tasks/fx]
trials: 1
hypothesis: "fixture-000: turn boundary"
""")
            (case / "case.json").write_text(json.dumps({"messages": True}))
            (case / "gateway.json").write_text(json.dumps(replies))
            _, actual, _ = _harness.run_case(
                case, _harness.current_runner(), _harness.CODE_ROOT, HERE,
                _harness.load_gateway(HERE), scratch, False,
            )
        return actual

    def assert_done(self, actual, turns):
        row, = actual["rows"]
        self.assertEqual((row["stop"], row["turns"]), ("done", turns), row)
        self.assertEqual(len(actual["requests"]), turns)

    def assert_guidance(self, actual, turns):
        events = [event for event in actual["runs"][0]["messages"]
                  if event["event"] == "guidance"]
        self.assertEqual(events, [
            {"event": "guidance", "turn": turn, "content": GUIDANCE} for turn in turns
        ])
        for turn in turns:
            prompt = actual["requests"][turn]["body"]["messages"][0]["content"]
            self.assertEqual(prompt.splitlines().count(GUIDANCE), 1)

    def test_coder_continues_until_edit_and_check(self):
        actual = self.run_turns([
            TEXT, call("fs_write", {"path": "solution", "content": "fixed"}),
            TEXT, call("run_check", {}), TEXT, TEXT,
        ], tools="fs_write, run_check")
        self.assert_done(actual, 5)
        self.assert_guidance(actual, [1, 3])

    def test_coder_stops_after_three_text_only_replies(self):
        actual = self.run_turns([TEXT] * 4)
        self.assert_done(actual, 3)
        self.assert_guidance(actual, [1, 2])

    def test_coder_continues_after_a_failing_check(self):
        actual = self.run_turns([
            call("fs_write", {"path": "solution", "content": "fixed"}),
            call("run_check", {}), TEXT, TEXT, TEXT, TEXT,
        ], tools="fs_write, run_check", check="exit 7")
        self.assert_done(actual, 5)
        self.assert_guidance(actual, [3, 4])

    def test_coder_continues_after_a_failing_post_turn_check(self):
        actual = self.run_turns([
            TEXT, call("fs_write", {"path": "solution", "content": "fixed"}), TEXT, TEXT,
        ], hook="rerun-check", check="test -s solution")
        # The passing post-turn check ends the run immediately after the write.
        self.assert_done(actual, 2)
        self.assert_guidance(actual, [1])

    def test_non_coding_text_only_reply_stops(self):
        for options in ({"tools": "fx_tool"}, {"goal": "Write answer.txt."}):
            with self.subTest(**options):
                actual = self.run_turns([TEXT, TEXT], **options)
                self.assert_done(actual, 1)
                self.assert_guidance(actual, [])

    def test_passing_post_turn_check_beats_retry(self):
        actual = self.run_turns([tool_reply()], hook="rerun-check")
        self.assert_done(actual, 1)

    def test_produced_deliverable_beats_retry(self):
        actual = self.run_turns(
            [tool_reply(mode="write", path="answer.txt", content="answer"), tool_reply()],
            goal="Write answer.txt.", check='test -s answer.txt',
        )
        self.assert_done(actual, 2)

    def test_read_only_palette_stays_available(self):
        actual = self.run_turns([{"ok": {"text": "answer"}}], tools="fx_tool")
        body = actual["requests"][0]["body"]
        self.assertEqual([tool["name"] for tool in (body.get("tool_schema") or [])], ["fx_tool"], body)
        self.assertNotIn("tool_choice", body)

    def test_withheld_tool_reports_final_turn(self):
        actual = self.run_turns([tool_reply()], check="test -s answer.md")
        first, retry = actual["requests"]
        for request in (first, retry):
            self.assertEqual(request["body"]["tool_choice"],
                             {"type": "function", "function": {"name": "fs_write"}})
        transcript = actual["runs"][0]["transcript"]
        self.assertIn("read tools are unavailable on the final turn", transcript)

    def test_coder_last_turn_keeps_optional_check_without_a_write_or_retry(self):
        actual = self.run_turns([{"ok": {"text": "Finished."}}],
                                tools="fs_write, exec, run_check", check="exit 0",
                                goal="Fix the parser in src/main.rs.")
        self.assert_done(actual, 1)
        body = actual["requests"][0]["body"]
        self.assertNotIn("tool_choice", body)
        self.assertIn("run_check", [tool["name"] for tool in body["tool_schema"]])
        self.assertIn("summarize the changes", body["messages"][0]["content"])
        exhausted = self.run_turns([tool_reply()], tools="fx_tool, fs_write, run_check")
        self.assertEqual(exhausted["rows"][0]["stop"], "budget:turns")
        self.assertEqual(len(exhausted["requests"]), 1)

    def test_shell_exec_and_frozen_check_return_results_to_model(self):
        actual = self.run_turns([
            call("exec", {"cmd": "mkdir src && cd src && printf '%s\\n' fixed | tail > result"}),
            call("run_check", {}, before_respond=[
                {"write": {"path": "tasks/fx/check", "content": "#!/bin/sh\nexit 99\n"}}]),
            {"ok": {"text": "Finished."}},
        ], tools="exec, run_check",
            check="test \"$(cat src/result)\" = fixed || exit 3\n"
                  "head -c 10000 /dev/zero | tr '\\000' x\necho tail-marker\nexit 7")
        self.assert_done(actual, 3)
        row, = actual["rows"]
        self.assertEqual(row["tool_calls"], 2)
        body = actual["requests"][-1]["body"]
        result = next(m["content"] for m in body["messages"]
                      if m.get("tool_call_id") == "run_check")
        self.assertIn("Check exit code: 7.", result)
        self.assertTrue(result.endswith("tail-marker\n"), result[-80:])
        self.assertLess(len(result), 8300)

    def test_bad_arguments_are_one_tool_error_then_model_can_recover(self):
        malformed = {"ok": {"tool_calls": [
            {"id": "bad", "name": "fs_write", "args": {}, "invalid_arguments": True}]}}
        actual = self.run_turns([malformed, {"ok": {"text": "Recovered."}}])
        self.assert_done(actual, 2)
        body = actual["requests"][1]["body"]
        result = next(m for m in body["messages"] if m.get("tool_call_id") == "bad")
        self.assertTrue(result["is_error"])
        self.assertIn("arguments were not valid JSON", result["content"])
        self.assertIn("fs_edit with a smaller change", result["content"])

    def test_post_turn_hook_deadline(self):
        actual = self.run_turns([{"ok": {}}], hook="rerun-check", seconds=2, check="sleep 5")
        row, = actual["rows"]
        self.assertEqual(row["stop"], "budget:seconds", row)
        self.assertNotEqual(row["verdict"], "void", row)

    def test_post_turn_hook_signal(self):
        actual = self.run_turns([{"ok": {}}], hook="fx_signal")
        self.assertIn("hook fx_signal: signal: terminated", "\n".join(actual["stdout"]))


def call(name, args, **step):
    return dict(step, ok={"tool_calls": [{"id": name, "name": name, "args": args}]})


def tool_reply(**args):
    return {"ok": {"tool_calls": [{"id": "one", "name": "fx_tool",
                                  "args": args or {"mode": "echo"}}]}}


def check():
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(TurnBoundaries)
    return unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful()


if __name__ == "__main__":
    raise SystemExit(0 if check() else 1)
