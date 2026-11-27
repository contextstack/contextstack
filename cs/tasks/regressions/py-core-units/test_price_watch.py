"""Deterministic price extraction for a scheduled watch."""
import hashlib
import importlib.machinery
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from unittest.mock import patch

CS = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(CS / "tools/bin"))
from _watch_extract import extract_price
sys.path.pop(0)

WRAPPER = """#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

policy, workspace, tool = map(Path, (sys.argv[2], sys.argv[4], sys.argv[6]))
assert sys.argv[1:6:2] == ["--policy", "--cwd", "--"]
host = json.loads(policy.read_text())
request = json.loads(tool.with_suffix(".json").read_text())["sandbox"]
socket = "browser-price-watch-test"
assert request == {"net": "none", "sockets": [socket]}, request
assert host["allow_net"] == ["none"] and list(host["sockets"]) == [socket], host
assert set(host) == {"version", "runtime_read", "allow_net", "sockets"}, host
assert Path(host["sockets"][socket]) == Path.home() / ".contextstack/public-browser/sockets/price-watch-test.sock"
assert "CS_BROWSER_BROKER_EXEC" not in os.environ
os.environ["CS_BROWSER_BROKER_EXEC"] = os.environ["FIXTURE_BROKER"]
os.chdir(workspace)
os.execv(tool, [str(tool)])
"""
BROKER = """#!/usr/bin/env python3
import hashlib, json, os, sys
from urllib.error import HTTPError
from urllib.request import ProxyHandler, build_opener
from xml.etree import ElementTree

request = json.load(sys.stdin)
assert request["url"] == "https://shop.example/widget" and request["verb"] == "open", request
opener = build_opener(ProxyHandler({}))
try:
    response = opener.open(os.environ["FIXTURE_PAGE"], timeout=5)
except HTTPError as error:
    response = error
with response:
    status = response.code
    if status == 408:
        print(json.dumps({"ok": False, "error_kind": "timeout"}))
        sys.exit(0)
    document = ElementTree.fromstring(response.read())
text = "".join(document.itertext())
reply = {"ok": True, "status": status, "final_url": request["url"], "text": text,
         "sha256": hashlib.sha256(text.encode()).hexdigest()}
if "selector" in request:
    assert request["selector"] == ".price", request
    match = document.find(".//*[@class='price']")
    reply["selected"] = None if match is None else match.text
print(json.dumps(reply))
"""


class PriceWatchExtraction(unittest.TestCase):
    def test_regex_price(self):
        self.assertEqual(extract_price("<p>Total $1,234.50</p>", {
            "regex": r"Total \$([\d,.]+)", "number_parser": "decimal_dot",
        }), 1234.50)

    def test_selector_price(self):
        self.assertEqual(extract_price("Store", {
            "selector": "div .price", "number_parser": "decimal_comma",
        }, selected="€ 1.234,50"), 1234.50)


class PriceWatchBroker(unittest.TestCase):
    def test_watch_set_uses_one_origin_and_an_isolated_browser_context(self):
        loader = importlib.machinery.SourceFileLoader("watch_broker", str(CS / "ops/bin/browser-broker"))
        broker = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
        loader.exec_module(broker)
        with tempfile.TemporaryDirectory() as raw, patch.dict(os.environ, {"CS_CONFIG_ROOT": raw}):
            root = Path(raw)
            watches = root / "watches"
            watches.mkdir()
            selector = 'div [data-price="offer"]'
            (watches / "fixture.json").write_text(json.dumps({
                "url": "https://shop.example/widget", "extract": {"selector": selector, "number_parser": "decimal_dot"},
                "threshold": {"above": 75}, "currency": "USD", "delivery": "attention",
            }))
            config = broker.load_config(CS / "policy/browser.json")
            self.assertEqual(config["sets"]["price-watch-fixture"], ["https://shop.example"])
            self.assertEqual(config["sets"]["public"], ["https://example.com"])
            calls = []

            class FakeCDP:
                def __init__(self):
                    self.commands = []
                    self.sent = []

                def send(self, method, params, session):
                    self.sent.append((method, params, session))
                    self.commands.append(len(self.sent))
                    return self.commands[-1]

                def call(self, method, params=None, session=None, timeout=None):
                    calls.append((method, params))
                    if method == "Page.navigate":
                        self.on_event({"sessionId": "session", "method": "Target.attachedToTarget", "params": {
                            "sessionId": "iframe", "targetInfo": {"type": "iframe", "targetId": "child"},
                            "waitingForDebugger": True,
                        }})
                        self.on_event({"method": "Target.attachedToTarget", "params": {
                            "sessionId": "popup", "targetInfo": {"type": "page", "targetId": "new-window"},
                            "waitingForDebugger": True,
                        }})
                        self.on_event({"sessionId": "session", "method": "Network.responseReceived",
                                       "params": {"type": "Document", "loaderId": "load", "response": {"status": 200}}})
                    if method == "Runtime.evaluate":
                        assert "document.querySelector(%s)" % json.dumps(selector) in params["expression"]
                    return {
                        "Target.createBrowserContext": {"browserContextId": "isolated"},
                        "Target.createTarget": {"targetId": "tab"},
                        "Target.attachToTarget": {"sessionId": "session"},
                        "Page.navigate": {"loaderId": "load"},
                        "Target.getTargetInfo": {"targetInfo": {"url": "https://shop.example/widget"}},
                        "Runtime.evaluate": {"result": {"value": '{"text":"Widget $80","selected":"$80"}'}},
                    }.get(method, {})

                def pump(self, deadline, until):
                    while self.commands:
                        self.on_event({"id": self.commands.pop(0), "result": {}})
                    self.on_event({"sessionId": "session", "method": "Page.loadEventFired"})
                    assert until()

            cdp = FakeCDP()
            instance = broker.Broker(cdp, config["sets"], root / "broker.jsonl")
            blocked = instance.handle("price-watch-fixture", {"verb": "open", "url": "https://example.com/"})
            self.assertFalse(blocked["ok"])
            self.assertEqual(calls, [])
            result = instance.handle("price-watch-fixture", {
                "verb": "open", "url": "https://shop.example/widget", "selector": selector,
            })
            self.assertTrue(result["ok"], result)
            self.assertEqual(result["selected"], "$80")
            self.assertEqual(calls[1], ("Target.createTarget", {"url": "about:blank", "browserContextId": "isolated"}))
            self.assertEqual([(method, session) for method, _, session in cdp.sent], [
                ("Fetch.enable", "iframe"), ("Target.closeTarget", None),
                ("Target.setAutoAttach", "iframe"), ("Runtime.runIfWaitingForDebugger", "iframe"),
            ])
        self.assertEqual(calls[-2], ("Target.disposeBrowserContext", {"browserContextId": "isolated"}))
        self.assertEqual(calls[-1][0], "Target.setAutoAttach")
        self.assertFalse(calls[-1][1]["autoAttach"])


class PriceWatchSchedule(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="price-watch-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.code = self.root / "code"
        ops = self.code / "ops/bin"
        ops.mkdir(parents=True)
        for name in ("schedule-enable", "schedule-enable.json", "schedule-tick", "_grant.py", "_void.py",
                     "_roots.py", "_estate_io.py", "_watch_config.py", "_price_watch.py",
                     "_review_schedule.py", "_schedule_delivery.py", "_sandbox_policy.py", "_job_reason.py"):
            shutil.copy2(CS / "ops/bin" / name, ops / name)
        (self.code / "tools").symlink_to(CS / "tools")
        (self.code / "policy").symlink_to(CS / "policy")
        (self.code / "scripts").symlink_to(CS / "scripts")
        self.state = self.code / "ledger"
        self.state.mkdir()
        (self.code / "ESTATE").write_text("fixture\n")
        grants = self.code / "grants"
        grants.mkdir()
        self.grant = grants / "fixture.yaml"
        self.grant.write_text(
            "id: fixture\nestate: fixture\nrepo: %s\npaths: [ops/bin/_price_watch.py, tools/bin/browser_session]\n"
            'gates: ["true"]\ncommit: false\nstatus: active\nexpiry: 2099-01-01T00:00:00Z\n' % self.code)
        home = self.root / "home"
        watches = home / ".contextstack/config/watches"
        watches.mkdir(parents=True)
        self.config = watches / "test.json"
        self.wrapper = self.script("wrapper", WRAPPER)
        broker = self.script("broker", BROKER)
        self.page = (200, '<html><p>Widget</p><p class="price">$120.00</p></html>')
        self.requests = 0
        self.notes = {}
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                fixture.requests += 1
                status, page = fixture.page
                self.send_response(status)
                self.end_headers()
                self.wfile.write(page.encode())

            def do_POST(self):
                key = self.headers["Idempotency-Key"]
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                note = (self.path, self.headers["Authorization"], body)
                prior = fixture.notes.setdefault(key, note)
                self.send_response(201 if prior is note else 200 if prior == note else 409)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *_):
                pass

        server = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join, 5)
        self.addCleanup(server.shutdown)
        self.env = {
            "PATH": os.environ["PATH"], "HOME": str(home), "TZ": "UTC",
            "PYTHONDONTWRITEBYTECODE": "1", "TMPDIR": str(self.root),
            "CS_SANDBOX_BIN": str(self.wrapper), "FIXTURE_BROKER": str(broker),
            "FIXTURE_PAGE": "http://127.0.0.1:%s/" % server.server_port,
        }

    def test_watch_delivers_note_and_markdown(self):
        config_dir = self.config.parent.parent
        file_root = self.root / "delivery"
        file_root.mkdir()
        (config_dir / "delivery.json").write_text(json.dumps({"file_root": str(file_root)}))
        (config_dir / "install.json").write_text(json.dumps({"director": self.env["FIXTURE_PAGE"]}))
        credential = config_dir.parent / "state/director/local-credential"
        credential.parent.mkdir(parents=True)
        credential.write_text("fixture-owner")
        self.config.write_text(json.dumps({
            "url": "https://shop.example/widget", "currency": "USD",
            "delivery": ["attention", {"conversation": "conv_fixture"}, {"file": "price.md"}],
            "extract": {"selector": ".price", "number_parser": "decimal_dot"}, "threshold": {"below": 100},
        }))
        self.op("schedule-enable", {
            "schedule_id": "price-watch-test", "grant": "fixture",
            "grant_digest": hashlib.sha256(self.grant.read_bytes()).hexdigest(),
            "actor_kind": "human", "actor_id": "director:human:user:1", "request_key": "enable-watch",
            "expected_digest": hashlib.sha256(b"null").hexdigest(),
        })
        self.assertIn("evidence", self.tick(16)["admitted"][0])
        report = (file_root / "price.md").read_text()
        self.assertIn("Cost: $0 (actual)", report)
        self.assertEqual(report, (self.state / "watches/test/latest.md").read_text())
        self.assertEqual(list(self.notes.values()), [
            ("/api/v1/client/conversations/conv_fixture/notes", "Bearer fixture-owner", {"message": report}),
        ])

    def script(self, name, source):
        path = self.root / name
        path.write_text(source)
        path.chmod(0o700)
        return path

    def op(self, name, args):
        result = subprocess.run([sys.executable, str(self.code / "ops/bin" / name)], input=json.dumps(args),
                                text=True, capture_output=True, env=self.env, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout) if result.stdout else {}

    def rows(self, name):
        path = self.state / name
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def tick(self, day, clock="08:00", **overrides):
        return self.op("schedule-tick", {"now": "2090-11-%02dT%s:00+00:00" % (day, clock),
                                         "scan_stale": False, **overrides})

    def test_watch_firings_crossing_retries_and_layout_change(self):
        config = {"url": "https://shop.example/widget", "currency": "USD", "delivery": "attention",
                  "extract": {"selector": ".price", "number_parser": "decimal_dot"}, "threshold": {"below": 100}}
        self.config.write_text(json.dumps(config))
        enabled = self.op("schedule-enable", {
            "schedule_id": "price-watch-test", "grant": "fixture",
            "grant_digest": hashlib.sha256(self.grant.read_bytes()).hexdigest(),
            "actor_kind": "human", "actor_id": "director:human:user:1", "request_key": "enable-watch",
            "expected_digest": hashlib.sha256(b"null").hexdigest(),
        })
        self.assertEqual(enabled["schedule"]["watch_origin"], "https://shop.example")
        self.assertEqual(enabled["schedule"]["missed"], "catch_up")
        self.assertEqual(self.tick(16, "10:00", dry_run=True)["admitted"][0]["job"], "dry-run")
        self.assertEqual(self.requests, 0)
        first = self.tick(16, "10:00")["admitted"][0]
        self.assertTrue(first["catch_up"])
        evidence = json.loads((self.state / first["evidence"]).read_text())
        self.assertEqual(evidence["price"], 120)
        self.assertEqual(evidence["currency"], "USD")
        self.assertTrue(evidence["fetched_at"].endswith("Z"))
        self.assertEqual(evidence["page_sha256"], hashlib.sha256(b"Widget$120.00").hexdigest())
        self.assertEqual(self.tick(16, "10:01"), {})
        self.assertEqual(self.rows("attention.jsonl"), [])

        for day, price in [(17, 100), (18, 90), (19, 80)]:
            self.page = (200, '<html><p class="price">$%s</p></html>' % price)
            fired = self.tick(day)["admitted"][0]
            self.assertIn("evidence", fired)
            if day == 18:
                # Simulate interruption after delivery and before the scheduler records the terminal.
                for shelf in ("dispatch.jsonl", "schedule-fires.jsonl"):
                    saved = [row for row in self.rows(shelf) if row.get("job") != fired["job"]]
                    (self.state / shelf).write_text("".join(json.dumps(row) + "\n" for row in saved))
                replay = self.tick(day, "08:01")["admitted"][0]
                self.assertEqual(replay["evidence"], fired["evidence"])
        alerts = self.rows("attention.jsonl")
        self.assertEqual(len(alerts), 1)
        self.assertEqual(alerts[0]["kind"], "needs_you")
        self.assertIn("crossed the threshold", alerts[0]["question"])
        self.assertEqual(self.tick(19, "08:01"), {})

        self.page = (200, "<html><p>Changed markup</p></html>")
        for day in (20, 21, 22, 23):
            failed = self.tick(day)["admitted"][0]
            self.assertEqual(failed["error_kind"], "layout_changed")
            self.assertFalse(failed["retryable"])
        self.assertEqual(self.tick(23, "08:06", retry_failed=["price-watch-test"]), {})
        self.assertEqual(len(self.rows("attention.jsonl")), 2)
        report = self.state / "watches/test/latest.md"
        self.assertIn("found no parseable price", report.read_text())

        for status, time in [(503, "08:00"), (408, "08:05")]:
            self.page = (status, "<html/>")
            failed = self.tick(24, time)["admitted"][0]
            self.assertTrue(failed["retryable"])
        backoff = self.tick(24, "08:06")
        self.assertEqual(backoff["deferred"][0]["reason"], "retry_backoff")
        config["extract"] = {"regex": r"Price: \$([\d.]+)", "number_parser": "decimal_dot"}
        self.config.write_text(json.dumps(config))
        self.page = (200, "<html><p>Price: $110.50</p></html>")
        self.assertIn("evidence", self.tick(24, "08:10")["admitted"][0])
        self.assertEqual(len(self.rows("attention.jsonl")), 2)
        rows = self.rows("watches/test/history.jsonl")
        self.assertEqual([row["kind"] for row in rows], ["price"] * 4 + ["layout_changed"] * 4 + ["price"])
        self.assertEqual(rows[-1]["evidence"]["price"], 110.50)
        self.assertIn("110.5 USD", report.read_text())
        self.assertEqual(self.requests, 11)

        self.config.write_text(json.dumps({**config, "url": "https://elsewhere.example/widget"}))
        blocked = self.tick(25)["admitted"][0]
        self.assertEqual(blocked["error_kind"], "watch_config")
        self.assertEqual(self.requests, 11)
