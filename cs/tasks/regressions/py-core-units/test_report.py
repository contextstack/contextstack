"""Reports group by spec and ignore obsolete ledger metadata."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE.parent / '_lib'), str(HERE.parents[2] / 'scripts')]
from scratch import scratch_dir


class ReportTests(unittest.TestCase):
    def test_rows_with_and_without_obsolete_metadata_share_spec_stats(self):
        loader = importlib.machinery.SourceFileLoader('unit_report', str(HERE.parents[2] / 'scripts/report'))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        report = importlib.util.module_from_spec(spec)
        loader.exec_module(report)
        with scratch_dir(prefix='report-units-') as raw:
            ledger = Path(raw) / 'ledger.jsonl'
            row = {'spec': 'sample', 'task': 'tasks/sample', 'verdict': 'pass',
                   'cost_class': 'free_local', 'seconds': 2}
            ledger.write_text('\n'.join(map(json.dumps, [
                dict(row, exp={'old': 'unvalidated'}, intent='arbitrary'), row])) + '\n')
            output = io.StringIO()
            with patch.object(sys, 'argv', ['report', str(ledger)]), contextlib.redirect_stdout(output):
                report.main()
            lines = output.getvalue().splitlines()
            self.assertTrue(lines[0].startswith('spec\t'))
            self.assertEqual(lines[1], 'sample\t2\t1.00\t0.0000\t0.0000\t2.0')
