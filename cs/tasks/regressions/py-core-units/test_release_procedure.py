import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

CS = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(CS / "scripts"))
from cs_mcp import Tools


class ReleaseProcedureTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="release-procedure-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.env = {
            "HOME": str(self.root), "PATH": os.environ["PATH"],
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.test",
            "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.test",
        }

    def run_script(self, script, *args):
        return subprocess.run([sys.executable, str(CS / "scripts" / script), *args],
                              cwd=self.root, env=self.env, capture_output=True, text=True)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root,
                                       env=self.env, text=True).strip()

    def test_runbook_is_available_from_chat_and_names_stop_points(self):
        lead = Tools(None, None)
        self.assertIn("release", lead.procedure()["procedures"])
        content = lead.procedure("read", "release")["content"]
        self.assertLessEqual(len(content.splitlines()), 80)
        text = " ".join(content.split())
        for marker in (
            "Export lane", "Clean-VM install lane", "Owner-approved push/tag action",
            "CI-watch lane", "Notes lane", "Stop points", "needs_you", "partial outcome",
            "Never push, tag or publish without an owner approval bound to the exact commit and tag.",
        ):
            self.assertIn(marker, text)

    def fake_gh(self):
        gh = self.root / "gh"
        gh.write_text(f"#!{sys.executable}\n" + """
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
assert args[0] == "run" and args[-2:] == ["--repo", "fixture/product"], args
identity = {"databaseId": 123, "headSha": "a" * 40, "headBranch": "v1.2.3-rc.1", "event": "push"}
with Path("calls").open("a") as log:
    print(args[1], file=log)
if args[1] == "list":
    assert args[2:-2] == ["--workflow", "release-bundles.yml", "--event", "push",
                          "--commit", "a" * 40, "--json",
                          "databaseId,headSha,headBranch,event"], args
    print(json.dumps([dict(identity, headBranch="main"), identity]))
else:
    assert args[1:4] == ["view", "123", "--json"], args
    conclusion = os.environ["FIXTURE_CONCLUSION"]
    status = "completed"
    if not Path("viewed").exists():
        Path("viewed").touch()
        status = "in_progress"
    if conclusion == "mismatch":
        identity["headSha"] = "b" * 40
    print(json.dumps(dict(identity, status=status, conclusion=conclusion,
                          url="https://example.test/run/123", jobs=[
        {"name": "linux", "conclusion": "success"},
        {"name": "macOS", "conclusion": conclusion},
        {"name": "publish", "conclusion": "skipped"},
    ])))
""")
        gh.chmod(0o755)
        self.env["PATH"] = str(self.root)

    def test_ci_watch_polls_and_reports_failures(self):
        self.fake_gh()
        for conclusion, code, failures in (("success", 0, []), ("failure", 1, ["macOS"])):
            with self.subTest(conclusion=conclusion):
                for name in ("calls", "viewed"):
                    (self.root / name).unlink(missing_ok=True)
                self.env["FIXTURE_CONCLUSION"] = conclusion
                result = self.run_script(
                    "release-ci-watch", "--repo", "fixture/product",
                    "--tag", "v1.2.3-rc.1", "--commit", "a" * 40,
                    "--interval", "1", "--timeout", "5",
                )
                self.assertEqual(result.returncode, code, result.stderr)
                payload = json.loads(result.stdout)
                self.assertEqual(payload["run_id"], 123)
                self.assertEqual(payload["failing_jobs"], failures)
                self.assertEqual((self.root / "calls").read_text().splitlines(),
                                 ["list", "view", "view"])

    def test_ci_watch_refuses_changed_run_identity(self):
        self.fake_gh()
        self.env["FIXTURE_CONCLUSION"] = "mismatch"
        result = self.run_script("release-ci-watch", "--repo", "fixture/product",
                                 "--tag", "v1.2.3-rc.1", "--commit", "a" * 40)
        self.assertEqual(result.returncode, 2)
        self.assertIn("run identity changed", result.stderr)

    @unittest.skipUnless(shutil.which("git"), "Git is required")
    def test_notes_use_merged_commits_after_previous_release(self):
        self.git("init", "-b", "main")
        self.git("commit", "--allow-empty", "-m", "Old feature")
        self.git("update-ref", "refs/tags/v1.0.0", "HEAD")
        self.git("checkout", "-qb", "feature")
        self.git("commit", "--allow-empty", "-m", "Add [new] command")
        self.git("checkout", "-q", "main")
        self.git("merge", "--no-ff", "feature", "-m", "Merge feature")
        self.git("update-ref", "refs/tags/v1.1.0-rc.1", "HEAD")
        result = self.run_script("release-notes", "--tag", "v1.1.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("# ContextStack 1.1.0", result.stdout)
        self.assertIn("Changes since v1.0.0", result.stdout)
        self.assertIn(r"- Add \[new\] command", result.stdout)
        self.assertNotIn("Old feature", result.stdout)
        self.assertNotIn("Merge feature", result.stdout)
        self.assertEqual(self.git("tag", "--list"), "v1.0.0\nv1.1.0-rc.1")
