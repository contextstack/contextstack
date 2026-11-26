"""Short scratch parent selection for socket fixtures."""
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "_lib"))
from scratch import short_tmp


class ShortTmpTests(unittest.TestCase):
    @patch("scratch.os.access", return_value=True)
    @patch("scratch.os.path.isdir", return_value=True)
    @patch("scratch.os.path.realpath", return_value="/private/tmp/check")
    def test_short_writable_tmpdir(self, _realpath, _isdir, access):
        with patch.dict(os.environ, {"TMPDIR": "/tmp/check"}):
            self.assertEqual(short_tmp(), "/tmp/check")
        access.assert_called_once_with("/tmp/check", os.W_OK | os.X_OK)

    def test_long_resolved_tmpdir(self):
        with patch.dict(os.environ, {"TMPDIR": "/tmp/check"}), patch(
            "scratch.os.path.realpath", return_value="/private/tmp/" + "é" * 20,
        ):
            self.assertEqual(short_tmp(), "/tmp")

    def test_unset_tmpdir(self):
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(short_tmp(), "/tmp")

    @patch("scratch.os.path.realpath", return_value="/private/tmp/check")
    def test_missing_or_unwritable_tmpdir(self, _realpath):
        with patch.dict(os.environ, {"TMPDIR": "/tmp/check"}):
            for exists in (False, True):
                with self.subTest(exists=exists), patch(
                    "scratch.os.path.isdir", return_value=exists,
                ), patch("scratch.os.access", return_value=False):
                    self.assertEqual(short_tmp(), "/tmp")
