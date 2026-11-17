"""Behaviour coverage for _vitals, _job_reason and _agent_stream (py-core-units)."""
import io
import json
import os
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path
from unittest.mock import MagicMock, Mock, patch

HERE = Path(__file__).resolve().parent
LIB = HERE.parent / "_lib"
OPS = HERE.parents[2] / "ops" / "bin"
sys.path[:0] = [str(LIB), str(OPS)]

from hermetic_env import hermetic_env  # noqa: E402
from scratch import scratch_dir  # noqa: E402

import _agent_stream  # noqa: E402
import _job_reason  # noqa: E402
import _vitals  # noqa: E402

VM_STAT = """Mach Virtual Memory Statistics: (page size of 4096 bytes)
Pages free:                   1000.
Pages active:                 5000.
Pages inactive:               2000.
Pages speculative:             500.
Pages purgeable:               250.
Pages wired down:             3000.
"""

MEMINFO_FULL = """MemTotal:       16384000 kB
MemAvailable:   8192000 kB
"""

MEMINFO_ESTIMATE = """MemTotal:       10000 kB
MemFree:        2000 kB
Cached:         3000 kB
Buffers:        500 kB
SReclaimable:   500 kB
"""

PROC_SWAPS = """Filename\t\t\t\tType\tSize\tUsed\tPriority
/dev/zram0                      partition\t5242876\t1024\t-2
"""

PSI_WARN = """some avg10=30.00 avg60=0.00 avg300=0.00 total=0
full avg10=0.00 avg60=0.00 avg300=0.00 total=0
"""

PSI_CRITICAL = """some avg10=0.00 avg60=0.00 avg300=0.00 total=0
full avg10=1.50 avg60=0.00 avg300=0.00 total=0
"""

SWAPUSAGE = "total = 2048.00M  used = 512.00M  free = 1536.00M"

HEX64 = "0123456789abcdef" * 4


class VitalsParserTests(unittest.TestCase):
    def test_parse_vm_stat_derives_available_and_total_bytes(self):
        avail, total = _vitals.parse_vm_stat(VM_STAT, page_size=4096)
        self.assertIsNotNone(avail)
        self.assertIsNotNone(total)
        self.assertGreater(total, avail)

    def test_parse_vm_stat_returns_none_for_empty_input(self):
        self.assertEqual(_vitals.parse_vm_stat(""), (None, None))

    def test_parse_meminfo_uses_mem_available_when_present(self):
        avail, total = _vitals.parse_meminfo(MEMINFO_FULL)
        self.assertEqual(avail, 8192000 * 1024)
        self.assertEqual(total, 16384000 * 1024)

    def test_parse_meminfo_estimates_available_without_mem_available(self):
        avail, total = _vitals.parse_meminfo(MEMINFO_ESTIMATE)
        self.assertEqual(total, 10000 * 1024)
        self.assertEqual(avail, (2000 + 3000 + 500 + 500) * 1024)

    def test_parse_swapusage_converts_megabytes_to_bytes(self):
        used, total = _vitals.parse_swapusage(SWAPUSAGE)
        self.assertEqual(used, 512 * 1024 * 1024)
        self.assertEqual(total, 2048 * 1024 * 1024)

    def test_parse_proc_swap_reads_first_device_line(self):
        used, total = _vitals.parse_proc_swap(PROC_SWAPS)
        self.assertEqual(used, 1024 * 1024)
        self.assertEqual(total, 5242876 * 1024)

    def test_parse_pressure_psi_maps_avg10_to_warn_or_critical(self):
        self.assertEqual(_vitals.parse_pressure_psi(PSI_WARN), "warn")
        self.assertEqual(_vitals.parse_pressure_psi(PSI_CRITICAL), "critical")
        self.assertEqual(_vitals.parse_pressure_psi("some avg10=2.00"), "normal")

    def test_parse_kern_pressure_maps_level_and_unknown(self):
        self.assertEqual(_vitals.parse_kern_pressure("2"), "warn")
        self.assertEqual(_vitals.parse_kern_pressure("garbage"), None)
        self.assertEqual(_vitals.parse_kern_pressure("99"), "unknown")


class VitalsPolicyAndHeldTests(unittest.TestCase):
    def test_load_resources_policy_merges_json_and_spool_disk_floor(self):
        with scratch_dir(prefix="vit-pol-") as raw:
            root = Path(raw)
            policy = root / "policy"
            policy.mkdir()
            (policy / "resources.json").write_text(
                json.dumps({"min_available_memory_fraction": 0.05, "hold_at_memory_pressure": "warn"}),
                encoding="utf-8",
            )
            (policy / "spool.json").write_text(json.dumps({"min_free_bytes": 1_000_000}), encoding="utf-8")
            got = _vitals.load_resources_policy(root)
            self.assertEqual(got["min_available_memory_fraction"], 0.05)
            self.assertEqual(got["min_free_disk_bytes"], 1_000_000)

    def test_load_resources_policy_returns_defaults_when_policy_missing(self):
        with scratch_dir(prefix="vit-def-") as raw:
            got = _vitals.load_resources_policy(Path(raw))
            self.assertEqual(got["min_available_memory_fraction"], 0.10)
            self.assertIsNone(got["min_free_disk_bytes"])

    def test_resource_held_reports_memory_pressure_on_darwin(self):
        policy = {"hold_at_memory_pressure": "critical", "min_available_memory_fraction": 0.01}
        with patch("platform.system", return_value="Darwin"):
            self.assertEqual(
                _vitals.resource_held({"memory_pressure": "critical"}, policy),
                {"reason": "memory_pressure"},
            )

    def test_resource_held_reports_memory_low_when_fraction_below_policy(self):
        policy = {"min_available_memory_fraction": 0.50, "hold_at_memory_pressure": "critical"}
        vitals = {"memory_total_bytes": 1000, "memory_available_bytes": 100}
        with patch("platform.system", return_value="Linux"):
            self.assertEqual(_vitals.resource_held(vitals, policy), {"reason": "memory_low"})

    def test_resource_held_reports_swap_high_on_linux_only(self):
        policy = {"max_swap_used_fraction": 0.50, "min_available_memory_fraction": None}
        vitals = {"swap_total_bytes": 1000, "swap_used_bytes": 900, "memory_total_bytes": 10_000, "memory_available_bytes": 9000}
        with patch("platform.system", return_value="Linux"):
            self.assertEqual(_vitals.resource_held(vitals, policy), {"reason": "swap_high"})
        with patch("platform.system", return_value="Darwin"):
            self.assertIsNone(_vitals.resource_held(vitals, policy))

    def test_resource_held_reports_disk_low_from_policy_floor(self):
        policy = {"min_free_disk_bytes": 5000, "min_available_memory_fraction": None}
        vitals = {"disk_free_bytes": 100}
        self.assertEqual(_vitals.resource_held(vitals, policy), {"reason": "disk_low"})

    def test_disk_free_bytes_returns_bavail_times_frsize_for_existing_path(self):
        with scratch_dir(prefix="vit-df-") as raw:
            free = _vitals.disk_free_bytes(raw)
            self.assertIsNotNone(free)
            self.assertGreater(free, 0)


class VitalsReadAndStatusTests(unittest.TestCase):
    def tearDown(self):
        _vitals._VITALS_CACHE.clear()

    def _darwin_run(self, argv, timeout=0.25):
        key = tuple(argv)
        table = {
            ("sysctl", "-n", "hw.memsize"): "8589934592\n",
            ("sysctl", "-n", "hw.pagesize"): "4096\n",
            ("vm_stat",): VM_STAT,
            ("sysctl", "-n", "kern.memorystatus_vm_pressure_level"): "4\n",
            ("sysctl", "vm.swapusage"): SWAPUSAGE + "\n",
        }
        return table.get(key, "")

    def test_read_vitals_darwin_patches_subprocess_and_populates_fields(self):
        with scratch_dir(prefix="vit-rd-") as raw:
            with patch("platform.system", return_value="Darwin"), patch("_vitals._run", side_effect=self._darwin_run):
                got = _vitals.read_vitals(state_root=raw)
            self.assertEqual(got["memory_total_bytes"], 8589934592)
            self.assertEqual(got["memory_pressure"], "critical")
            self.assertIsNotNone(got["swap_used_bytes"])

    def test_read_vitals_linux_reads_proc_fixtures_via_path_hooks(self):
        def read_text(self, encoding="utf-8", errors="strict"):
            name = self.name
            if name == "meminfo":
                return MEMINFO_FULL
            if name == "memory":
                return PSI_WARN
            if name == "swaps":
                return PROC_SWAPS
            raise OSError("missing")

        def is_file(self):
            return self.name in ("meminfo", "memory", "swaps") or Path.is_file(self)

        with scratch_dir(prefix="vit-lin-") as raw:
            with patch("platform.system", return_value="Linux"), patch.object(Path, "read_text", read_text), patch.object(
                Path, "is_file", is_file
            ):
                got = _vitals.read_vitals(state_root=raw, disk_free=999)
            self.assertEqual(got["disk_free_bytes"], 999)
            self.assertEqual(got["memory_pressure"], "warn")

    def test_status_extras_includes_held_when_resources_tight(self):
        with scratch_dir(prefix="vit-st-") as raw:
            root = Path(raw)
            state = root / "state"
            config = root / "config"
            state.mkdir()
            config.mkdir()
            policy_dir = config / "policy"
            policy_dir.mkdir()
            (policy_dir / "resources.json").write_text(
                json.dumps({"min_free_disk_bytes": 1_000_000}), encoding="utf-8"
            )
            vitals = {"disk_free_bytes": 1, "memory_total_bytes": 8, "memory_available_bytes": 8}
            with patch.object(_vitals, "read_vitals", return_value=vitals):
                body = _vitals.status_extras(state_root=state, config_root=config)
            self.assertEqual(body["held"], {"reason": "disk_low"})

    def test_status_extras_reuses_cache_within_ttl_when_snapshot_stable(self):
        with scratch_dir(prefix="vit-cache-") as raw:
            root = Path(raw)
            state, config = root / "state", root / "config"
            state.mkdir()
            config.mkdir()
            (config / "policy").mkdir()
            (config / "policy" / "resources.json").write_text("{}", encoding="utf-8")
            calls = []

            def snapshot(**_kw):
                calls.append(1)
                return {
                    "memory_total_bytes": 10_000,
                    "memory_available_bytes": 9000,
                    "memory_pressure": "normal",
                    "disk_free_bytes": 9_000_000,
                }

            now = [1000.0]
            with patch.object(_vitals, "read_vitals", side_effect=snapshot), patch.object(
                _vitals.time, "time", side_effect=lambda: now[0]
            ), patch.object(_vitals, "_refresh_memory_pressure", side_effect=lambda v: dict(v)):
                _vitals.status_extras(state_root=state, config_root=config, cache_ttl=60)
                now[0] += 10
                _vitals.status_extras(state_root=state, config_root=config, cache_ttl=60)
            self.assertEqual(len(calls), 1)

    def test_vitals_cache_may_change_true_near_policy_thresholds(self):
        policy = {
            "min_available_memory_fraction": 0.10,
            "max_swap_used_fraction": 0.90,
            "min_free_disk_bytes": 1000,
        }
        vitals = {
            "memory_total_bytes": 1000,
            "memory_available_bytes": 100,
            "swap_total_bytes": 1000,
            "swap_used_bytes": 860,
            "disk_free_bytes": 1040,
        }
        with patch("platform.system", return_value="Linux"):
            self.assertTrue(_vitals._vitals_cache_may_change(vitals, policy))


class JobReasonTests(unittest.TestCase):
    def test_redact_strips_director_tokens_and_api_key_assignments(self):
        token = "csl_" + HEX64
        text = "failed with %s and OPENAI_API_KEY=secret-value-here" % token
        red = _job_reason.redact(text)
        self.assertNotIn(token, red)
        self.assertNotIn("secret-value-here", red)
        self.assertIn("<redacted>", red)

    def test_redact_obj_walks_nested_json_shapes(self):
        nested = {"a": ["x", {"OPENAI_API_KEY": "sk-" + ("P" * 24)}]}
        got = _job_reason.redact_obj(nested)
        self.assertEqual(got["a"][1]["OPENAI_API_KEY"], "<redacted>")

    def test_clip_bounds_redacted_reason_to_reason_chars(self):
        long_text = "x" * (_job_reason.REASON_CHARS + 500)
        clipped = _job_reason.clip(long_text)
        self.assertLessEqual(len(clipped.encode("utf-8")), _job_reason.REASON_CHARS)

    def test_reason_of_prefers_explicit_fields_before_stderr_tail(self):
        row = {"reason": "primary", "stop": "other", "error": {"detail": "detail"}}
        self.assertEqual(_job_reason.reason_of(row, "stderr tail"), "primary")
        self.assertEqual(_job_reason.reason_of({"stop": "stopped"}, ""), "stopped")
        self.assertEqual(_job_reason.reason_of({"error": "plain"}, ""), "plain")

    def test_job_log_path_accepts_plain_ids_and_rejects_invalid(self):
        self.assertEqual(_job_reason.job_log_path("/state", "job-1"), Path("/state/jobs/job-1/node.log"))
        self.assertIsNone(_job_reason.job_log_path("/state", "bad/id"))

    def test_append_job_log_redacts_and_creates_parent_directory(self):
        with scratch_dir(prefix="jr-log-") as raw:
            path = Path(raw) / "jobs" / "j1" / "node.log"
            _job_reason.append_job_log(path, "line with sk-" + ("Q" * 20) + "\n")
            body = path.read_text(encoding="utf-8")
            self.assertIn("<redacted>", body)
            self.assertNotIn("sk-", body)

    def test_persist_output_writes_redacted_file(self):
        with scratch_dir(prefix="jr-out-") as raw:
            dest = Path(raw) / "out.txt"
            body = _job_reason.persist_output(dest, "Bearer eyJhbGciOiJIUzI1NiJ9.extra.sig")
            self.assertEqual(dest.read_text(encoding="utf-8"), body)
            self.assertIn("<redacted>", body)


class AgentStreamDecodeAndDialectTests(unittest.TestCase):
    def test_dialect_maps_binary_names_to_stream_kinds(self):
        self.assertEqual(_agent_stream.dialect("claude"), "claude")
        self.assertEqual(_agent_stream.dialect("cursor-agent"), "claude")
        self.assertIsNone(_agent_stream.dialect("unknown-binary"))

    def test_signals_returns_empty_for_malformed_or_unknown_dialect(self):
        self.assertEqual(_agent_stream.signals("claude", b"not-json"), [])
        self.assertEqual(_agent_stream.signals("nope", json.dumps({"type": "x"})), [])
        self.assertEqual(_agent_stream.signals("claude", json.dumps([])), [])


class AgentStreamSignalsTests(unittest.TestCase):
    def test_claude_signals_emit_turn_tool_call_and_provider_limit(self):
        assistant = json.dumps({
            "type": "assistant",
            "message": {"content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
            ]},
        })
        kinds = [s["kind"] for s in _agent_stream.signals("claude", assistant)]
        self.assertIn("turn", kinds)
        self.assertIn("tool_call", kinds)
        quota = json.dumps({
            "error": {"type": "TerminalQuotaError", "message": "usage limit exceeded", "code": 429},
        })
        limit = _agent_stream.signals("claude", quota)
        self.assertTrue(any(s.get("kind") == "provider_limit" for s in limit))

    def test_claude_user_tool_result_maps_status_fields(self):
        user = json.dumps({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "name": "Bash", "is_error": True, "content": "fail"},
            ]},
        })
        got = _agent_stream.signals("claude", user)
        self.assertEqual(got[0]["kind"], "tool_result")
        self.assertTrue(got[0]["failed"])

    def test_codex_signals_cover_shell_commands_and_function_tools(self):
        started = json.dumps({
            "type": "item.started",
            "item": {"id": "c1", "type": "command_execution", "command": "pytest"},
        })
        self.assertEqual(_agent_stream.signals("codex", started)[0]["name"], "shell")
        completed = json.dumps({
            "type": "item.completed",
            "item": {"id": "c1", "type": "command_execution", "aggregated_output": "ok", "exit_code": 0},
        })
        self.assertEqual(_agent_stream.signals("codex", completed)[0]["exit_code"], 0)
        fn = json.dumps({
            "type": "item.completed",
            "item": {"id": "f1", "type": "function_call", "name": "grep", "arguments": {"pattern": "x"}},
        })
        self.assertEqual(_agent_stream.signals("codex", fn)[0]["kind"], "tool_call")

    def test_gemini_and_antigravity_signals_map_assistant_and_tools(self):
        gemini = json.dumps({"type": "message", "role": "assistant", "content": "hello"})
        self.assertEqual(_agent_stream.signals("gemini", gemini)[0]["kind"], "turn")
        agy_call = json.dumps({
            "event": "step_update", "step_type": "tool_invoke", "tool_name": "read", "tool_call_id": "a1",
        })
        self.assertEqual(_agent_stream.signals("antigravity", agy_call)[0]["kind"], "tool_call")
        agy_delta = json.dumps({
            "event": "step_update", "step_type": "agent_response", "text_delta": "partial",
        })
        self.assertEqual(_agent_stream.signals("antigravity", agy_delta)[0]["kind"], "turn")


class AgentStreamMapperAndIoTests(unittest.TestCase):
    def test_mapper_claude_collects_deltas_result_and_reply(self):
        mapper = _agent_stream.Mapper("claude")
        delta = json.dumps({
            "type": "stream_event",
            "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "He"}},
        })
        self.assertEqual(mapper.text(delta), "He")
        assistant = json.dumps({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "Hello"}]},
        })
        self.assertEqual(mapper.text(assistant), "")
        result = json.dumps({"type": "result", "result": "Hello"})
        mapper.text(result)
        self.assertEqual(mapper.reply(), "Hello")

    def test_mapper_codex_and_gemini_reply_from_last_agent_message(self):
        codex = _agent_stream.Mapper("codex")
        codex.text(json.dumps({
            "type": "item.completed",
            "item": {"type": "agent_message", "text": "done"},
        }))
        self.assertEqual(codex.reply(), "done")
        gem = _agent_stream.Mapper("gemini")
        gem.text(json.dumps({"type": "message", "role": "assistant", "content": "hi"}))
        self.assertEqual(gem.reply(), "hi")

    def test_mapper_antigravity_joins_deltas_when_no_result_event(self):
        agy = _agent_stream.Mapper("antigravity")
        agy.text(json.dumps({"event": "step_update", "step_type": "agent_response", "text_delta": "ab"}))
        self.assertEqual(agy.reply(), "ab")

    def test_line_serializes_json_with_runner_unicode_escapes(self):
        raw = _agent_stream.line({"msg": "\u2028"})
        self.assertIn("\\u2028", raw)
        self.assertTrue(raw.endswith("\n"))

    def test_append_and_events_write_redacted_delta_to_jsonl(self):
        with scratch_dir(prefix="as-ev-") as raw:
            run = Path(raw)
            ev = _agent_stream.Events(run)
            token = "csl_" + HEX64
            ev.write(1, "delta " + token, discard=True)
            text = (run / "events.jsonl").read_text(encoding="utf-8")
            self.assertNotIn(token, text)
            self.assertIn("<redacted>", text)
            _agent_stream.append(run / "extra.jsonl", {"seq": 1, "kind": "test"}, durable=True)
            self.assertTrue((run / "extra.jsonl").is_file())


class CursorAvailabilityTests(unittest.TestCase):
    def test_cursor_availability_resets_on_assistant_progress(self):
        now = [0.0]
        mon = _agent_stream.CursorAvailability(clock=lambda: now[0])
        line = json.dumps({"type": "connection", "subtype": "reconnecting"})
        mon.feed(line.encode())
        self.assertEqual(mon.count, 1)
        assistant = json.dumps({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "ok"}]},
        })
        mon.feed(assistant.encode(), progress=False)
        self.assertEqual(mon.count, 0)

    def test_cursor_availability_poll_stops_after_repeated_reconnect_churn(self):
        now = [0.0]
        mon = _agent_stream.CursorAvailability(clock=lambda: now[0])
        payload = json.dumps({"type": "connection", "subtype": "reconnecting"}).encode()
        for _ in range(5):
            mon.feed(payload)
        stop = mon.poll()
        self.assertIsNotNone(stop)
        self.assertEqual(stop["stop"], "provider_unavailable")



class VitalsEdgeAndRefreshTests(unittest.TestCase):
    def tearDown(self):
        _vitals._VITALS_CACHE.clear()

    def test_run_returns_empty_when_subprocess_raises(self):
        with patch("_vitals.subprocess.run", side_effect=OSError("nope")):
            self.assertEqual(_vitals._run(["vm_stat"]), "")

    def test_disk_free_bytes_returns_none_when_statvfs_unavailable(self):
        with patch("os.statvfs", side_effect=OSError("nope")):
            self.assertIsNone(_vitals.disk_free_bytes("/missing"))

    def test_resource_held_darwin_memory_low_without_pressure_reading(self):
        policy = {"hold_at_memory_pressure": "critical", "min_available_memory_fraction": 0.50}
        vitals = {"memory_total_bytes": 1000, "memory_available_bytes": 10}
        with patch("platform.system", return_value="Darwin"):
            self.assertEqual(_vitals.resource_held(vitals, policy), {"reason": "memory_low"})

    def test_resource_held_darwin_warn_policy_holds_on_warn_pressure(self):
        policy = {"hold_at_memory_pressure": "warn", "min_available_memory_fraction": 0.01}
        with patch("platform.system", return_value="Darwin"):
            self.assertEqual(
                _vitals.resource_held({"memory_pressure": "warn"}, policy),
                {"reason": "memory_pressure"},
            )

    def test_refresh_memory_pressure_updates_darwin_and_linux(self):
        with patch("platform.system", return_value="Darwin"), patch(
            "_vitals._run", return_value="2"
        ):
            got = _vitals._refresh_memory_pressure({"memory_pressure": None})
            self.assertEqual(got["memory_pressure"], "warn")

        def read_text(self, encoding="utf-8", errors="strict"):
            if self.name == "memory":
                return PSI_WARN
            raise OSError

        def is_file(self):
            return self.name == "memory"

        with patch("platform.system", return_value="Linux"), patch.object(Path, "read_text", read_text), patch.object(
            Path, "is_file", is_file
        ):
            got = _vitals._refresh_memory_pressure({})
            self.assertEqual(got["memory_pressure"], "warn")

    def test_status_extras_refreshes_when_cache_may_change(self):
        with scratch_dir(prefix="vit-chg-") as raw:
            root = Path(raw)
            state, config = root / "state", root / "config"
            state.mkdir()
            config.mkdir()
            (config / "policy").mkdir()
            (config / "policy" / "resources.json").write_text(
                json.dumps({"min_available_memory_fraction": 0.10, "min_free_disk_bytes": 1000}),
                encoding="utf-8",
            )
            calls = []

            def snapshot(**_kw):
                calls.append(1)
                return {
                    "memory_total_bytes": 1000,
                    "memory_available_bytes": 100,
                    "memory_pressure": "normal",
                    "disk_free_bytes": 5000,
                }

            now = [1000.0]
            with patch.object(_vitals, "read_vitals", side_effect=snapshot), patch.object(
                _vitals.time, "time", side_effect=lambda: now[0]
            ), patch.object(_vitals, "_refresh_memory_pressure", side_effect=lambda v: dict(v)), patch(
                "platform.system", return_value="Linux"
            ):
                _vitals.status_extras(state_root=state, config_root=config, cache_ttl=60)
                now[0] += 5
                _vitals.status_extras(state_root=state, config_root=config, cache_ttl=60)
            self.assertEqual(len(calls), 2)

    def test_vitals_cache_may_change_when_held_or_pressure_warn(self):
        policy = {"min_available_memory_fraction": 0.10, "max_swap_used_fraction": 0.90, "min_free_disk_bytes": 0}
        self.assertTrue(_vitals._vitals_cache_may_change({"memory_pressure": "warn"}, policy))
        with patch("platform.system", return_value="Darwin"):
            self.assertTrue(
                _vitals._vitals_cache_may_change(
                    {"memory_total_bytes": 100, "memory_available_bytes": 5, "memory_pressure": None},
                    policy,
                )
            )


class JobReasonEdgeTests(unittest.TestCase):
    def test_redact_covers_pem_url_and_authorization_headers(self):
        pem = "-----BEGIN RSA PRIVATE KEY-----\nkey\n-----END RSA PRIVATE KEY-----"
        self.assertNotIn("BEGIN RSA", _job_reason.redact(pem))
        self.assertEqual(_job_reason.redact("https://u:p@host/x"), "https://<redacted>@host/x")
        self.assertEqual(
            _job_reason.redact("Authorization: Bearer sekret"),
            "Authorization: Bearer <redacted>",
        )

    def test_redact_obj_preserves_non_string_leaves(self):
        self.assertEqual(_job_reason.redact_obj(42), 42)

    def test_reason_of_returns_empty_when_no_candidate_text(self):
        self.assertEqual(_job_reason.reason_of({}, ""), "")
        self.assertEqual(_job_reason.reason_of("not-a-dict", ""), "")

    def test_append_job_log_skips_none_path_and_empty_body(self):
        _job_reason.append_job_log(None, "x")
        with scratch_dir(prefix="jr-noop-") as raw:
            path = Path(raw) / "jobs" / "j" / "node.log"
            _job_reason.append_job_log(path, "")
            self.assertFalse(path.exists())


class AgentStreamBranchCoverageTests(unittest.TestCase):
    def test_codex_legacy_msg_form_and_gemini_tool_events(self):
        legacy = json.dumps({"msg": {"type": "agent_message", "message": "legacy"}})
        mapper = _agent_stream.Mapper("codex")
        self.assertEqual(mapper.text(legacy), "legacy")
        tool = json.dumps({"type": "tool_call", "id": "g1", "name": "grep", "args": {"pattern": "x"}})
        got = _agent_stream.signals("gemini", tool)
        self.assertEqual(got[0]["kind"], "tool_call")
        tool_result = json.dumps({
            "type": "message", "role": "tool", "tool_call_id": "g1", "content": "found",
        })
        self.assertEqual(_agent_stream.signals("gemini", tool_result)[0]["kind"], "tool_result")

    def test_codex_item_failed_and_antigravity_tool_result_steps(self):
        failed = json.dumps({
            "type": "item.failed",
            "item": {"id": "c1", "type": "command_execution", "error": True, "aggregated_output": "boom"},
        })
        got = _agent_stream.signals("codex", failed)[0]
        self.assertEqual(got["kind"], "tool_result")
        self.assertTrue(got["failed"])
        agy = json.dumps({
            "event": "step_update", "step_type": "tool_result", "tool_name": "read",
            "tool_call_id": "a1", "output": "bytes",
        })
        self.assertEqual(_agent_stream.signals("antigravity", agy)[0]["kind"], "tool_result")

    def test_mapper_ignores_non_object_lines_and_non_text_blocks(self):
        mapper = _agent_stream.Mapper("claude")
        self.assertEqual(mapper.text("[]"), "")
        mapper.partial = True
        assistant = json.dumps({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "hidden"}]},
        })
        self.assertEqual(mapper.text(assistant), "")

    def test_line_escapes_additional_unicode_separators(self):
        self.assertIn("\\u0085", _agent_stream.line({"x": chr(0x85)}))

    def test_append_rolls_back_on_partial_write_failure(self):
        with scratch_dir(prefix="as-rollback-") as raw:
            path = Path(raw) / "events.jsonl"
            path.write_text('{"ok":1}\n', encoding="utf-8")
            before = path.read_bytes()
            real = os.write
            calls = [0]

            def fail_partial(fd, data):
                calls[0] += 1
                if calls[0] == 1:
                    return real(fd, data[:2])
                raise OSError("disk full")

            with patch("_agent_stream.os.write", side_effect=fail_partial):
                with self.assertRaises(OSError):
                    _agent_stream.append(path, {"broken": True}, durable=True)
            self.assertEqual(path.read_bytes(), before)


class InlineThread:
    def __init__(self, target, **kwargs):
        self.target = target

    def start(self):
        self.target()

    def join(self, *args, **kwargs):
        pass


class AgentStreamRunStreamingTests(unittest.TestCase):
    def _processes(self):
        processes = Mock()
        processes.evidence = []
        return processes

    def test_run_streaming_stops_early_when_on_line_requests_stop(self):
        from _muse import check_environment

        proc = Mock(pid=900001, stdout=io.BytesIO(b"line\n"), stderr=io.BytesIO())
        proc.wait.side_effect = lambda **kw: -9
        processes = self._processes()
        with patch.object(check_environment, "__call__", lambda env: None) if False else patch(
            "_agent_stream.check_environment"
        ), patch("_agent_stream.subprocess.Popen", return_value=proc), patch(
            "_agent_stream.threading.Thread", InlineThread
        ), patch("_job_memory.prepare_job", return_value=(["fixture"], {}, None)), patch(
            "_job_memory.JobProcesses", return_value=processes
        ):
            done = _agent_stream.run_streaming(
                ["fixture"], "/tmp/ws", hermetic_env(), 30,
                lambda raw: {"stop": "stuck:repeat"},
            )
        self.assertEqual(done.forced_stop, "stuck:repeat")

    def test_run_streaming_returns_completed_process_on_clean_exit(self):
        proc = Mock(pid=900001, stdout=io.BytesIO(b"done\n"), stderr=io.BytesIO(b""))
        proc.wait.return_value = 0
        processes = self._processes()
        with patch("_agent_stream.check_environment"), patch(
            "_agent_stream.subprocess.Popen", return_value=proc
        ), patch("_agent_stream.threading.Thread", InlineThread), patch(
            "_job_memory.prepare_job", return_value=(["fixture"], {}, None)
        ), patch("_job_memory.JobProcesses", return_value=processes):
            done = _agent_stream.run_streaming(
                ["fixture"], "/tmp/ws", hermetic_env(), 30, lambda raw: None,
                on_evidence=processes.evidence.extend,
            )
        self.assertEqual(done.returncode, 0)
        self.assertIn("done", done.stdout)

    def test_run_streaming_honours_on_idle_stop_hook(self):
        proc = Mock(pid=900001, stdout=io.BytesIO(b""), stderr=io.BytesIO(b""))

        def slow_wait(**kwargs):
            time.sleep(0.05)
            return proc.returncode if hasattr(proc, "returncode") else 0

        proc.wait = slow_wait
        proc.returncode = 0
        processes = self._processes()
        with patch("_agent_stream.check_environment"), patch(
            "_agent_stream.subprocess.Popen", return_value=proc
        ), patch("_agent_stream.threading.Thread", InlineThread), patch(
            "_job_memory.prepare_job", return_value=(["fixture"], {}, None)
        ), patch("_job_memory.JobProcesses", return_value=processes):
            done = _agent_stream.run_streaming(
                ["fixture"], "/tmp/ws", hermetic_env(), 30, lambda raw: None,
                on_idle=lambda: {"stop": "idle", "reason": "quiet"},
            )
        self.assertEqual(done.forced_stop, "idle")

    def test_run_streaming_raises_cancelled_when_sigterm_arrives_during_cleanup(self):
        from _muse import Cancelled

        proc = Mock(pid=900001, stdout=io.BytesIO(b"partial\n"), stderr=io.BytesIO(b""))
        proc.wait.return_value = 0
        processes = self._processes()

        def close_raises_term():
            signal.raise_signal(signal.SIGTERM)

        processes.close.side_effect = close_raises_term
        previous = signal.signal(signal.SIGTERM, signal.SIG_DFL)
        try:
            with patch("_agent_stream.check_environment"), patch(
                "_agent_stream.subprocess.Popen", return_value=proc
            ), patch("_agent_stream.threading.Thread", InlineThread), patch(
                "_job_memory.prepare_job", return_value=(["fixture"], {}, None)
            ), patch("_job_memory.JobProcesses", return_value=processes):
                with self.assertRaises(Cancelled) as ctx:
                    _agent_stream.run_streaming(
                        ["fixture"], "/tmp/ws", hermetic_env(), 30, lambda raw: None,
                    )
                self.assertEqual(ctx.exception.stdout, "partial\n")
        finally:
            signal.signal(signal.SIGTERM, previous)

    def test_cursor_availability_time_branch_stops_after_three_reconnects_and_delay(self):
        now = [0.0]
        mon = _agent_stream.CursorAvailability(clock=lambda: now[0])
        payload = json.dumps({"type": "connection", "subtype": "reconnecting"}).encode()
        mon.feed(payload)
        mon.feed(payload)
        mon.feed(payload)
        self.assertIsNone(mon.poll())
        now[0] = 121.0
        stop = mon.poll()
        self.assertIsNotNone(stop)



class AgentStreamMoreBranchTests(unittest.TestCase):
    def test_nested_content_list_reaches_text_helper(self):
        user = json.dumps({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result", "tool_use_id": "t1",
                "content": [{"type": "text", "text": "nested"}, {"text": "block"}],
                "error": {"message": "bad"},
            }]},
        })
        got = _agent_stream.signals("claude", user)[0]
        self.assertIn("nested", got["text"])

    def test_codex_function_call_output_uses_error_status_fallback(self):
        line = json.dumps({
            "type": "item.completed",
            "item": {
                "id": "f1", "type": "function_call_output", "name": "grep",
                "output": "stdout", "result": {"status": "failed"},
            },
        })
        got = _agent_stream.signals("codex", line)[0]
        self.assertEqual(got["kind"], "tool_result")
        self.assertEqual(got.get("status"), "failed")

    def test_gemini_tool_use_and_tool_response_variants(self):
        use = json.dumps({"type": "tool_use", "id": "u1", "name": "read", "parameters": {"path": "a"}})
        self.assertEqual(_agent_stream.signals("gemini", use)[0]["args"], {"path": "a"})
        resp = json.dumps({"type": "tool_response", "tool_call_id": "u1", "output": "ok"})
        self.assertEqual(_agent_stream.signals("gemini", resp)[0]["text"], "ok")

    def test_mapper_paths_cover_claude_codex_gemini_and_antigravity_results(self):
        claude = _agent_stream.Mapper("claude")
        claude.text(json.dumps({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "Hello"}]},
        }))
        self.assertEqual(claude.text(json.dumps({"type": "assistant", "message": {"content": []}})), "")
        codex = _agent_stream.Mapper("codex")
        codex.text(json.dumps({"msg": {"type": "agent_message", "message": "via-msg"}}))
        self.assertEqual(codex.reply(), "via-msg")
        agy = _agent_stream.Mapper("antigravity")
        agy.text(json.dumps({"event": "result", "result": {"response": "final"}}))
        self.assertEqual(agy.reply(), "final")

    def test_events_write_without_discard_omits_fsync_flag(self):
        with scratch_dir(prefix="as-nodiscard-") as raw:
            ev = _agent_stream.Events(Path(raw))
            ev.write(2, None)
            row = json.loads((Path(raw) / "events.jsonl").read_text(encoding="utf-8"))
            self.assertFalse(row["payload"]["partial"])


class AgentStreamRunStreamingMoreTests(unittest.TestCase):
    def test_run_streaming_raises_timeout_when_process_never_finishes(self):
        proc = Mock(pid=1, stdout=io.BytesIO(b""), stderr=io.BytesIO(b""))
        proc.wait.side_effect = subprocess.TimeoutExpired(["fixture"], 1)
        processes = Mock()
        processes.evidence = []
        with patch("_agent_stream.check_environment"), patch(
            "_agent_stream.subprocess.Popen", return_value=proc
        ), patch("_agent_stream.threading.Thread", InlineThread), patch(
            "_job_memory.prepare_job", return_value=(["fixture"], {}, None)
        ), patch("_job_memory.JobProcesses", return_value=processes):
            with self.assertRaises(subprocess.TimeoutExpired):
                _agent_stream.run_streaming(
                    ["fixture"], "/tmp/ws", hermetic_env(), 0.25, lambda raw: None,
                )

    def test_run_streaming_applies_memory_monitor_forced_meta(self):
        proc = Mock(pid=1, stdout=io.BytesIO(b""), stderr=io.BytesIO(b""))
        proc.wait.return_value = 0
        processes = Mock()
        processes.evidence = []
        monitor = Mock()
        monitor.forced_meta.return_value = {"stop": "memory_cap"}
        with patch("_agent_stream.check_environment"), patch(
            "_agent_stream.subprocess.Popen", return_value=proc
        ), patch("_agent_stream.threading.Thread", InlineThread), patch(
            "_job_memory.prepare_job", return_value=(["fixture"], {}, None)
        ), patch("_job_memory.JobProcesses", return_value=processes), patch(
            "_job_memory.MemoryMonitor", return_value=monitor
        ):
            done = _agent_stream.run_streaming(
                ["fixture"], "/tmp/ws", hermetic_env(), 30, lambda raw: None, memory_cap_mb=256,
            )
        self.assertEqual(done.forced_stop, "memory_cap")

    def test_run_streaming_survives_on_line_reader_exceptions(self):
        proc = Mock(pid=1, stdout=io.BytesIO(b"still\n"), stderr=io.BytesIO(b""))
        proc.wait.return_value = 0
        processes = Mock()
        processes.evidence = []

        def on_line(_raw):
            raise RuntimeError("reader gone")

        with patch("_agent_stream.check_environment"), patch(
            "_agent_stream.subprocess.Popen", return_value=proc
        ), patch("_agent_stream.threading.Thread", InlineThread), patch(
            "_job_memory.prepare_job", return_value=(["fixture"], {}, None)
        ), patch("_job_memory.JobProcesses", return_value=processes):
            done = _agent_stream.run_streaming(
                ["fixture"], "/tmp/ws", hermetic_env(), 30, on_line,
            )
        self.assertIn("still", done.stdout)


class VitalsParserEdgeTests(unittest.TestCase):
    def test_parse_swapusage_and_proc_swap_tolerate_garbage_rows(self):
        self.assertEqual(_vitals.parse_swapusage("not swap"), (None, None))
        self.assertEqual(_vitals.parse_proc_swap("Filename\nfoo bar\n"), (None, None))

    def test_parse_pressure_psi_returns_none_without_measurements(self):
        self.assertIsNone(_vitals.parse_pressure_psi("header only"))

    def test_vitals_cache_may_change_false_when_vitals_are_comfortable(self):
        policy = {"min_available_memory_fraction": 0.10, "max_swap_used_fraction": 0.90, "min_free_disk_bytes": 100}
        vitals = {
            "memory_total_bytes": 1000,
            "memory_available_bytes": 900,
            "swap_total_bytes": 1000,
            "swap_used_bytes": 100,
            "disk_free_bytes": 50_000,
            "memory_pressure": "normal",
        }
        with patch("platform.system", return_value="Linux"):
            self.assertFalse(_vitals._vitals_cache_may_change(vitals, policy))

    def test_read_vitals_darwin_tolerates_non_numeric_sysctl_output(self):
        def run(argv, timeout=0.25):
            key = tuple(argv)
            if key == ("sysctl", "-n", "hw.memsize"):
                return "not-a-number\n"
            return VitalsReadAndStatusTests()._darwin_run(argv)
        with scratch_dir(prefix="vit-bad-sys-") as raw:
            with patch("platform.system", return_value="Darwin"), patch("_vitals._run", side_effect=run):
                got = _vitals.read_vitals(state_root=raw)
            self.assertIsNone(got["memory_total_bytes"])

    def test_status_extras_merges_provider_limits_when_state_present(self):
        with scratch_dir(prefix="vit-pl-") as raw:
            root = Path(raw)
            state, config = root / "state", root / "config"
            state.mkdir()
            config.mkdir()
            (config / "policy").mkdir()
            (config / "policy" / "resources.json").write_text("{}", encoding="utf-8")
            import _provider_limits
            _provider_limits.record_limit(
                state, "codex",
                {"message": "m", "resets_at": "2099-01-01T00:00:00Z", "via": "test"},
            )
            with patch.object(
                _vitals, "read_vitals",
                return_value={"memory_total_bytes": 1, "memory_available_bytes": 1, "disk_free_bytes": 9_000_000},
            ):
                body = _vitals.status_extras(state_root=state, config_root=config)
            self.assertIn("provider_limits", body)

if __name__ == "__main__":
    unittest.main()
