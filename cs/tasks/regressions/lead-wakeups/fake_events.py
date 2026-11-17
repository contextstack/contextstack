#!/usr/bin/env python3
"""Minimal fake Director Client API for lead wake regression tests."""
from __future__ import annotations

import json
import queue
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class FakeDirector:
    def __init__(self):
        self.jobs: dict[str, dict] = {}
        self.turns: dict[str, list[dict]] = {}
        self._streams: list[queue.Queue] = []
        self._lock = threading.Lock()
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None
        self.wake_failures = 0
        self.wake_posts = 0
        self.lead_intents: list[tuple[dict, dict]] = []
        self.intents_fail = False

    @property
    def url(self) -> str:
        if not self._server:
            raise RuntimeError("not started")
        host, port = self._server.server_address
        return "http://127.0.0.1:%d" % port

    def start(self) -> None:
        director = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                return

            def do_POST(self):  # noqa: N802
                if not self.headers.get("Authorization", "").startswith("Bearer "):
                    self.send_error(401)
                    return
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b"{}"
                try:
                    payload = json.loads(raw.decode("utf-8") or "{}")
                except ValueError:
                    self.send_error(400)
                    return
                path = self.path.split("?", 1)[0]
                if path.endswith("/wake-turns") and "/conversations/" in path:
                    if director.wake_failures > 0:
                        director.wake_failures -= 1
                        self.send_error(503)
                        return
                    director.wake_posts += 1
                    conversation = path.split("/conversations/", 1)[1].split("/", 1)[0]
                    nudge = str(payload.get("nudge") or "")
                    reply = str(payload.get("reply") or "")
                    jobs = payload.get("jobs") if isinstance(payload.get("jobs"), list) else []
                    row = {
                        "turn_id": "trn-wake-%d" % (len(director.turns.get(conversation, [])) + 1),
                        "origin": "wake",
                        "message": "",
                        "reply": reply,
                        "detail": {"nudge": nudge, "jobs": jobs},
                        "state": "done",
                    }
                    director.turns.setdefault(conversation, []).append(row)
                    body = json.dumps({"turn": row, "conversation": {"conversation_id": conversation}}).encode()
                    self.send_response(201)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(body)
                    return
                self.send_error(404)

            def do_GET(self):  # noqa: N802
                if not self.headers.get("Authorization", "").startswith("Bearer "):
                    self.send_error(401)
                    return
                path = self.path.split("?", 1)[0]
                if path.rstrip("/").endswith("/events"):
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    self.wfile.write(b"retry: 2000\n\nevent: hello\ndata: {\"revision\": 1}\n\n")
                    self.wfile.flush()
                    q: queue.Queue = queue.Queue()
                    with director._lock:
                        director._streams.append(q)
                    try:
                        while True:
                            try:
                                frame = q.get(timeout=0.5)
                            except queue.Empty:
                                continue
                            if frame is None:
                                return
                            self.wfile.write(frame)
                            self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError, OSError):
                        return
                    finally:
                        with director._lock:
                            if q in director._streams:
                                director._streams.remove(q)
                    return
                if "/conversations/" in path and path.endswith("/thread"):
                    conversation = path.split("/conversations/", 1)[1].split("/", 1)[0]
                    body = json.dumps({"turns": director.turns.get(conversation, [])}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(body)
                    return
                if path.rstrip("/").endswith("/intents"):
                    if director.intents_fail:
                        self.send_error(503)
                        return
                    q = {}
                    if "?" in self.path:
                        from urllib.parse import parse_qs
                        q = {k: v[0] for k, v in parse_qs(self.path.split("?", 1)[1]).items()}
                    conversation = q.get("conversation")
                    lanes_only = q.get("lanes") == "1"
                    try:
                        limit = int(q.get("limit") or "500")
                    except ValueError:
                        limit = 500
                    limit = max(1, min(limit, 500))
                    rows = list(reversed(director.lead_intents))
                    intents = []
                    jobs = []
                    for intent, job in rows:
                        if conversation and intent.get("lead_conversation_id") != conversation:
                            continue
                        meta = intent.get("meta") if isinstance(intent.get("meta"), dict) else {}
                        if lanes_only:
                            source = str(meta.get("source") or "")
                            lane = meta.get("lane")
                            if source not in ("client-web-lane", "cs-mcp") and not lane:
                                continue
                        intents.append(intent)
                        jobs.append(job)
                        if len(intents) >= limit:
                            break
                    body = json.dumps({"intents": intents, "jobs": jobs}).encode("utf-8")
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(body)
                    return
                if "/jobs/" in path:
                    job_id = path.rsplit("/", 1)[-1]
                    job = director.jobs.get(job_id, {"job_id": job_id, "state": "missing"})
                    body = json.dumps({"job": job}).encode("utf-8")
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(body)
                    return
                self.send_error(404)

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        with self._lock:
            for q in self._streams:
                q.put(None)
            self._streams.clear()
        if self._server:
            self._server.shutdown()
            self._server = None

    def add_turn(self, conversation: str, job_id: str) -> None:
        self.turns.setdefault(conversation, []).append({"job_id": job_id, "turn_id": "turn-" + job_id})


    def add_lead_lane(self, conversation: str, intent_id: str, lane: str, job_id: str, **job_fields) -> None:
        intent = {
            "intent_id": intent_id,
            "lead_conversation_id": conversation,
            "meta": {"lane": lane, "source": "cs-mcp"},
            "created_at": "2026-11-%02dT00:00:00Z" % (len(self.lead_intents) + 1),
        }
        job = {
            "job_id": job_id,
            "intent_id": intent_id,
            "meta": {"lane": lane},
            "state": "running",
            **job_fields,
        }
        self.lead_intents.append((intent, job))
        self.jobs[job_id] = job

    def set_job(self, job_id: str, **fields) -> None:
        row = dict(self.jobs.get(job_id) or {})
        row.update(fields)
        row["job_id"] = job_id
        self.jobs[job_id] = row

    def emit_job_change(self, job_id: str) -> None:
        job = self.jobs.get(job_id, {"job_id": job_id, "state": "done"})
        payload = json.dumps(
            {"kind": "job", "id": job_id, "revision": 2, "record": job}
        ).encode("utf-8")
        frame = b"id: 2\nevent: change\ndata: " + payload + b"\n\n"
        with self._lock:
            for q in list(self._streams):
                q.put(frame)

    def drop_streams(self) -> None:
        with self._lock:
            for q in list(self._streams):
                q.put(None)
            self._streams.clear()
