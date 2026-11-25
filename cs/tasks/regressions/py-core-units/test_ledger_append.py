"""B3: one cut-off ledger line (a crash mid-append) must not break every later read."""
import importlib.machinery
import importlib.util
import json
import os
import sys
import unittest
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
OPS = HERE.parents[2] / 'ops/bin'
sys.path[:0] = [str(HERE.parent / '_lib'), str(OPS)]
from hermetic_env import hermetic_env
from scratch import scratch_dir


def load(name, alias):
    loader = importlib.machinery.SourceFileLoader(alias, str(OPS / name))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(alias, loader))
    loader.exec_module(module)
    return module


class LedgerAppendTests(unittest.TestCase):
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(scratch_dir(prefix='ledger-append-')))
        env = {**hermetic_env(), 'CS_STATE_ROOT': str(self.root), 'CS_CONFIG_ROOT': str(self.root),
               'CS_CODE_ROOT': str(OPS.parents[1])}
        self.stack.enter_context(patch.dict(os.environ, env, clear=True))

    def test_finalize_append_starts_a_fresh_line_after_a_torn_tail(self):
        finalize = load('finalize-run', 'unit_finalize_run')
        ledger = self.root / 'ledger.jsonl'
        ledger.write_bytes(json.dumps({'spec': 's', 'trial': 1}).encode() + b'\n{"spec": "s", "tri')
        with patch.object(finalize, 'LEDGER', ledger):
            finalize.append_row({'spec': 's', 'trial': 2}, {}, {})
            finalize.append_row({'spec': 's', 'trial': 3}, {}, {})
        lines = ledger.read_bytes().split(b'\n')
        self.assertEqual(lines[1], b'{"spec": "s", "tri')
        self.assertEqual([json.loads(l)['trial'] for l in lines[2:] if l], [2, 3])
        self.assertTrue(ledger.read_bytes().endswith(b'\n'))

    def test_finalize_append_to_an_empty_or_clean_ledger_adds_no_blank_line(self):
        finalize = load('finalize-run', 'unit_finalize_run')
        ledger = self.root / 'fresh.jsonl'
        with patch.object(finalize, 'LEDGER', ledger):
            finalize.append_row({'trial': 1}, {}, {})
            finalize.append_row({'trial': 2}, {}, {})
        self.assertNotIn(b'\n\n', ledger.read_bytes())
        self.assertFalse(ledger.read_bytes().startswith(b'\n'))

    def test_schedule_tick_reads_past_a_torn_ledger_line(self):
        schedule = load('schedule-tick', 'unit_schedule_tick')
        ledger = self.root / 'ledger.jsonl'
        ledger.write_text(json.dumps({'trial': 1}) + '\n{"tri\n' + json.dumps({'trial': 2}) + '\n')
        self.assertEqual([r['trial'] for r in schedule.read_ledger(ledger)], [1, 2])


if __name__ == '__main__':
    unittest.main()
