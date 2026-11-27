import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "scripts"))

from _linux_setup import credential_findings, private_path


class CredentialModesTests(unittest.TestCase):
    def test_linux_credential_modes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / "credentials"
            directory.mkdir(mode=0o700)
            credential = directory / "openai"
            credential.touch(mode=0o600)

            for path, default, accepted, refused in (
                (directory, 0o700, (0o700, 0o500), (0o750, 0o300, 0o600)),
                (credential, 0o600, (0o600, 0o400), (0o640, 0o604, 0o644, 0o200)),
            ):
                try:
                    for mode in accepted + refused:
                        with self.subTest(path=path.name, mode=oct(mode)):
                            path.chmod(mode)
                            self.assertEqual(stat.S_IMODE(path.stat().st_mode), mode)
                            checks = {check["check"]: check for check in credential_findings(root, "")}
                            check = checks[f"mode {path}"]
                            self.assertEqual(check["ok"], mode in accepted)
                            if mode in refused:
                                self.assertEqual(check["fix"], f"chmod {default:o} {path}")
                finally:
                    path.chmod(default)

    def test_special_bits_are_refused(self):
        # Some filesystems clear setuid and setgid bits when chmod is called.
        path = Path("fixture")
        for default, kind, valid_type in (
            (0o700, stat.S_IFDIR, stat.S_ISDIR),
            (0o600, stat.S_IFREG, stat.S_ISREG),
        ):
            for bit in (stat.S_ISUID, stat.S_ISGID, stat.S_ISVTX):
                with self.subTest(kind=kind, bit=bit):
                    meta = SimpleNamespace(st_mode=kind | default | bit, st_uid=os.getuid())
                    with patch.object(Path, "lstat", return_value=meta):
                        check = private_path(path, default, valid_type)
                    self.assertFalse(check["ok"])
                    self.assertEqual(check["fix"], f"chmod {default:o} {path}")
