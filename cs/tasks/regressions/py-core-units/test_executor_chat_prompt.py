"""Chat turns on CLI executors use conversation framing, not coding-lane prompts."""
import sys
import tempfile
import unittest
from pathlib import Path

OPS = Path(__file__).resolve().parents[3] / "ops" / "bin"
sys.path.insert(0, str(OPS))

import _executor as ex  # noqa: E402


class ChatPromptFramingTests(unittest.TestCase):
    def test_is_chat_spec(self):
        self.assertTrue(ex.is_chat_spec({"id": "chat"}))
        self.assertFalse(ex.is_chat_spec({"id": "hosted-lead"}))
        self.assertFalse(ex.is_chat_spec({"id": "ordinary"}))
        self.assertFalse(ex.is_chat_spec({}))
        self.assertFalse(ex.is_chat_spec(None))

    def test_spec_prompt_paths_match_harness_order(self):
        self.assertEqual(
            ex.spec_prompt_paths({
                "system": "prompts/system.md",
                "context": ["conversation/context.md", "prompts/extra.md"],
            }),
            ["prompts/system.md", "conversation/context.md", "prompts/extra.md"],
        )
        self.assertEqual(ex.spec_prompt_paths({"id": "chat"}), [])
        self.assertEqual(ex.spec_prompt_paths(None), [])

    def test_chat_prompt_uses_system_and_goal_without_coding_prefix(self):
        with tempfile.TemporaryDirectory() as raw:
            stim = Path(raw)
            (stim / "system.txt").write_text(
                "--- prompts/system.md ---\n"
                "You are a helpful assistant in a conversation. The thread so far is in your context.\n"
                "--- conversation/context.md ---\n"
                "# Conversation so far\n\nThis is the first message of the conversation.\n",
                encoding="utf-8",
            )
            prompt = ex.cli_prompt(chat=True, goal="test 1", stim_dir=stim)
            self.assertIn("You are a helpful assistant in a conversation", prompt)
            self.assertIn("test 1", prompt)
            self.assertNotIn("Implement the task", prompt)
            self.assertNotIn("Do not commit unless asked", prompt)
            self.assertTrue(prompt.strip().endswith("test 1"))

    def test_coding_prompt_keeps_lane_framing(self):
        with tempfile.TemporaryDirectory() as raw:
            stim = Path(raw)
            (stim / "system.txt").write_text("", encoding="utf-8")
            prompt = ex.cli_prompt(chat=False, goal="fix the bug", stim_dir=stim)
            self.assertTrue(prompt.startswith("Implement the task in the current working directory."))
            self.assertIn("fix the bug", prompt)

    def test_hosted_lead_is_not_chat(self):
        # Hosted leads take the lead-host path; their spec must not switch on chat framing.
        self.assertFalse(ex.is_chat_spec({"id": "hosted-lead", "model": "claude-code-sonnet"}))


if __name__ == "__main__":
    unittest.main()
