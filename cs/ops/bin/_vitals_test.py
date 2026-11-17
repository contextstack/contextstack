#!/usr/bin/env python3
"""Unit tests for _vitals parsers and the resource guard. Run: python ops/bin/_vitals_test.py"""
import json
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
FIX = HERE / "fixtures/vitals"


def load(name):
    return (FIX / name).read_text(encoding="utf-8")


class VitalsTest(unittest.TestCase):
    def test_parse_vm_stat(self):
        from _vitals import parse_vm_stat

        avail, _ = parse_vm_stat(load("vm_stat_darwin.txt"), page_size=16384)
        self.assertEqual(avail, (47231 + 842 + 1198892 + 12345) * 16384)

    def test_parse_meminfo(self):
        from _vitals import parse_meminfo

        avail, total = parse_meminfo(load("meminfo_linux.txt"))
        self.assertEqual(total, 32800000 * 1024)
        self.assertEqual(avail, 3600000 * 1024)

    def test_parse_swap_darwin(self):
        from _vitals import parse_swapusage

        used, total = parse_swapusage(load("vm.swapusage_darwin.txt"))
        self.assertEqual(total, int(8192 * 1024 * 1024))
        self.assertEqual(used, int(7372.80 * 1024 * 1024))

    def test_parse_swap_linux(self):
        from _vitals import parse_proc_swap

        used, total = parse_proc_swap(load("swaps_linux.txt"))
        self.assertEqual(total, 8388604 * 1024)
        self.assertEqual(used, 7549743 * 1024)

    def test_parse_pressure_psi(self):
        from _vitals import parse_pressure_psi

        self.assertEqual(parse_pressure_psi(load("pressure_memory_linux.txt")), "warn")

    def test_resource_guard_holds_and_releases(self):
        from _vitals import load_resources_policy, resource_held

        policy = {
            "min_available_memory_fraction": 0.10,
            "max_swap_used_fraction": 0.90,
            "min_free_disk_bytes": 10_000_000_000,
        }
        ok = {
            "memory_total_bytes": 100,
            "memory_available_bytes": 20,
            "swap_total_bytes": 100,
            "swap_used_bytes": 10,
            "disk_free_bytes": 20_000_000_000,
        }
        self.assertIsNone(resource_held(ok, policy))
        self.assertEqual(resource_held({**ok, "memory_available_bytes": 5}, policy)["reason"], "memory_low")
        import platform
        from unittest import mock

        with mock.patch("platform.system", return_value="Linux"):
            self.assertEqual(resource_held({**ok, "swap_used_bytes": 95}, policy)["reason"], "swap_high")
        with mock.patch("platform.system", return_value="Darwin"):
            self.assertIsNone(resource_held({**ok, "swap_used_bytes": 95}, policy))
        self.assertEqual(resource_held({**ok, "disk_free_bytes": 1}, policy)["reason"], "disk_low")
        self.assertIsNone(resource_held({**ok, "memory_total_bytes": None}, policy))
        defaults = load_resources_policy(HERE.parent.parent)
        self.assertIn("min_available_memory_fraction", defaults)
        self.assertEqual(defaults.get("hold_at_memory_pressure"), "critical")

    def test_darwin_pressure_hold_and_fraction_fallback(self):
        from _vitals import resource_held
        from unittest import mock

        healthy = {
            "memory_total_bytes": 128_000_000_000,
            "memory_available_bytes": 2_000_000_000,
            "memory_pressure": "normal",
            "swap_total_bytes": 100,
            "swap_used_bytes": 99,
            "disk_free_bytes": 20_000_000_000,
        }
        policy = {"min_available_memory_fraction": 0.10, "hold_at_memory_pressure": "critical",
                  "max_swap_used_fraction": 0.9, "min_free_disk_bytes": 10_000_000_000}
        with mock.patch("platform.system", return_value="Darwin"):
            self.assertIsNone(resource_held(healthy, policy))
            self.assertEqual(resource_held({**healthy, "memory_pressure": "critical"}, policy)["reason"],
                             "memory_pressure")
            warn_only = {**policy, "hold_at_memory_pressure": "warn"}
            self.assertEqual(resource_held({**healthy, "memory_pressure": "warn"}, warn_only)["reason"],
                             "memory_pressure")
            low_frac = {**healthy, "memory_pressure": None, "memory_available_bytes": 5_000_000_000}
            self.assertEqual(resource_held(low_frac, policy)["reason"], "memory_low")

    def test_take_refuses_while_held_accepts_when_clear(self):
        from unittest import mock

        from _node_intake import take

        class Client:
            calls = []

            def call(self, method, path, body=None, timeout=None):
                self.calls.append((method, path, body))
                return b""

        client = Client()
        offered = [{"job_id": "job1", "bundle_sha256": "a" * 64, "spec": "spec.yaml", "task": "task",
                    "files": [{"path": "spec.yaml", "sha256": "b" * 64, "bytes": 1, "mode": 420}]}]
        state = Path(HERE / "fixtures/vitals")
        with mock.patch("_node_intake.intake_blocked", return_value={"reason": "memory_low"}):
            accepted, queued, failed = take(client, offered, state)
            self.assertEqual(accepted, [])
            self.assertEqual(queued, [])
            self.assertEqual(client.calls, [])
        with mock.patch("_node_intake.intake_blocked", return_value=None), \
                mock.patch("_node_intake.materialise"), \
                mock.patch("_node_intake.ensure_queued", return_value=True):
            accepted, queued, failed = take(client, offered, state)
            self.assertEqual(accepted, ["job1"])
            self.assertEqual(queued, ["job1"])
            self.assertEqual(client.calls[-1], ("POST", "jobs/job1/accept", {"bundle_sha256": "a" * 64}))

    def test_read_vitals_never_raises(self):
        from _vitals import read_vitals

        v = read_vitals(state_root=HERE)
        self.assertIsInstance(v, dict)
        json.dumps(v)


if __name__ == "__main__":
    unittest.main()
