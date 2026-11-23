"""Director task links survive Python run records without requiring code_root identifiers."""
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'ops/bin'))
from _estate_io import spec_intent
import _run_record


class DirectorIntentTests(unittest.TestCase):
    def test_optional_ulid_reaches_terminal(self):
        intent = '01ARZ3NDEKTSV4RRFFQ69G5FAV'
        for fields in ({}, {'intent': intent}):
            with self.subTest(fields=fields), tempfile.TemporaryDirectory() as raw:
                root = Path(raw)
                row = {'transcript': 'ledger/runs/fixture', **spec_intent(fields)}
                _run_record.terminal(root, root, row, 'fixture', 'spec', 'task', time.time(), 60)
                terminal = json.loads((root / 'terminal.json').read_text())
                self.assertEqual({k: terminal[k] for k in ('intent',) if k in terminal}, fields)
                self.assertNotIn('exp', terminal)

    def test_retired_and_invalid_values_fail(self):
        for value in ('INT-' + '0001', 'EXP-' + '1'):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, 'retired format'):
                spec_intent({'intent': value})
        for value in ('hello', '', '81ARZ3NDEKTSV4RRFFQ69G5FAV', '01ARZ3NDEKTSV4RRFFQ69G5FAI', 1):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, 'Director ULID'):
                spec_intent({'intent': value})


if __name__ == '__main__':
    unittest.main()
