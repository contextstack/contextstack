#!/usr/bin/env python3
"""Child-lane wake-ups for the hosted lead session host (Design.HostedLead §3).

Demanded-by: Design.HostedLead 2026-11-11 lane 6.

Holds a long-lived Client API events stream, refetches watched jobs on reconnect
(CLIENT-API §8), nudges the live CLI session, waits for the reply, and records it on the
Director as ``POST conversations/{id}/wake-turns``: a done turn with ``origin: wake``, an
empty message, the follow-up as ``reply`` and ``detail: {nudge, jobs}``. Nothing reaches the
person until that post succeeds. Once the nudge is sent it is never re-sent, even when the
reply cannot be recorded (that failure goes to the serve log), so a Director outage cannot
loop the lead.
"""
from __future__ import annotations

import hashlib
import uuid
import json
import os
import re
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Callable

from _job_reason import redact
from _lead_socket import report_lead_watch

ENDED = ("done", "error")
OTHER_LANES_FETCH_LIMIT = 20
OTHER_LANES_DISPLAY_CAP = 20
DEFAULT_STALL_S = 180.0
DEFAULT_BATCH_S = 0.2
DEFAULT_WAKE_REPLY_S = 600.0
WATCHES_FILE = "watched-jobs.json"
WAKE_SPOOL_DIR = "wake-reply-spool"
WAKE_SPOOL_MAX_AGE_S = 24 * 3600.0
WAKE_SPOOL_BACKOFF_S = 1.0
WAKE_SPOOL_BACKOFF_MAX_S = 60.0


def resolve_director_url(config_root: Path, meta: dict | None = None) -> str | None:
    """The Director this conversation's lead talks to: the override, the conversation's own metadata (what its
    turns use), then the install record in the config root (``<config root>/install.json``)."""
    override = os.environ.get("LEAD_WAKE_DIRECTOR_URL", "").strip()
    if override:
        return override.rstrip("/")
    url = str((meta or {}).get("director") or "").strip()
    if url:
        return url.rstrip("/")
    for path in (config_root / "install.json", config_root / "config/install.json"):
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        url = str(record.get("director") or "").strip()
        if url:
            return url.rstrip("/")
    return None


def nudge_jobs(nudge: str) -> list[str]:
    """Job ids named by a (possibly batched) nudge, in order, once each."""
    seen: list[str] = []
    for found in re.findall(r"\bjob (\S+) ended:", nudge):
        if found not in seen:
            seen.append(found)
    return seen


def format_nudge(job: dict, *, lane: str | None = None) -> str:
    job_id = str(job.get("job_id") or job.get("id") or "?")
    state = str(job.get("state") or "?")
    check = job.get("check") if isinstance(job.get("check"), dict) else {}
    verdict = str(job.get("verdict") or check.get("verdict") or "—")
    lane_s = str(lane or (job.get("meta") or {}).get("lane") or "?")
    text = "Lane %s job %s ended: %s, verdict %s" % (lane_s, job_id, state, verdict)
    err = str(job.get("error") or "").strip()
    if err:
        text += ", error %s" % err
    return text


def _lane_name(job: dict, intent: dict | None) -> str:
    job_meta = job.get("meta") if isinstance(job.get("meta"), dict) else {}
    intent_meta = (intent or {}).get("meta") if isinstance((intent or {}).get("meta"), dict) else {}
    return str(job_meta.get("lane") or intent_meta.get("lane") or "?")


def _job_verdict(job: dict) -> str:
    check = job.get("check") if isinstance(job.get("check"), dict) else {}
    return str(job.get("verdict") or check.get("verdict") or "—")


def _job_error_brief(job: dict) -> str:
    err = job.get("error")
    if isinstance(err, dict):
        text = str(err.get("detail") or err.get("code") or err.get("stop") or "").strip()
    else:
        text = str(err or "").strip()
    if not text:
        return "—"
    tokens = text.replace(",", " ").split()
    return tokens[0] if tokens else text[:40]


def format_lane_status(job: dict, intent: dict | None = None) -> str:
    lane = _lane_name(job, intent)
    state = str(job.get("state") or "?")
    if state == "done":
        return "%s done (%s)" % (lane, _job_verdict(job))
    if state == "error":
        return "%s error (%s)" % (lane, _job_error_brief(job))
    return "%s %s" % (lane, state)


def _newest_jobs(jobs: list) -> dict[str, dict]:
    by_intent: dict[str, dict] = {}
    for job in jobs:
        if not isinstance(job, dict):
            continue
        iid = str(job.get("intent_id") or "").strip()
        if not iid or iid in by_intent:
            continue
        by_intent[iid] = job
    return by_intent


def format_other_lanes_line(
    client,
    conversation_id: str,
    *,
    exclude_job_ids: set[str] | None = None,
) -> str | None:
    """One compact status line for this lead's other task lanes, or None when the fetch fails."""
    exclude = {str(j) for j in (exclude_job_ids or set()) if str(j).strip()}
    query = urllib.parse.urlencode(
        {"conversation": conversation_id, "lanes": "1", "limit": str(OTHER_LANES_FETCH_LIMIT)}
    )
    status, body = client.request("GET", "intents?" + query, timeout=15)
    if status != 200 or not isinstance(body, dict):
        return None
    intents = [i for i in (body.get("intents") or []) if isinstance(i, dict)]
    jobs = [j for j in (body.get("jobs") or []) if isinstance(j, dict)]
    if len(intents) >= OTHER_LANES_FETCH_LIMIT:
        wide = urllib.parse.urlencode(
            {"conversation": conversation_id, "lanes": "1", "limit": "500"}
        )
        status2, body2 = client.request("GET", "intents?" + wide, timeout=15)
        if status2 == 200 and isinstance(body2, dict):
            intents = [i for i in (body2.get("intents") or []) if isinstance(i, dict)]
            jobs = [j for j in (body2.get("jobs") or []) if isinstance(j, dict)]
    newest = _newest_jobs(jobs)
    entries: list[str] = []
    for intent in intents:
        iid = str(intent.get("intent_id") or "").strip()
        if not iid:
            continue
        job = newest.get(iid)
        if not job:
            continue
        job_id = str(job.get("job_id") or "").strip()
        if job_id in exclude:
            continue
        entries.append(format_lane_status(job, intent))
    if not entries:
        return None
    tail = ""
    if len(entries) > OTHER_LANES_DISPLAY_CAP:
        extra = len(entries) - OTHER_LANES_DISPLAY_CAP
        entries = entries[:OTHER_LANES_DISPLAY_CAP]
        tail = ", and %d more" % extra
    return redact("Your other lanes: %s%s." % (", ".join(entries), tail))


class DirectorClient:
    """Minimal Client API client for events and job refetch."""

    def __init__(self, base_url: str, credential_file: Path, *, refetch_credential: Callable[[], bool] | None = None):
        self.base = base_url.rstrip("/") + "/api/v1/client/"
        self.credential_file = Path(credential_file)
        self._refetch = refetch_credential


    def _once_more_on_unauthorized(self, attempt):
        status, body = attempt()
        if status == 401 and self._refetch is not None and self._refetch():
            return attempt()
        return status, body
    def token(self) -> str:
        return self.credential_file.read_text(encoding="utf-8").strip()

    def request(self, method: str, path: str, *, stream: bool = False, timeout: float = 30):
        def attempt():
            headers = {"Authorization": "Bearer " + self.token()}
            if stream:
                headers["Accept"] = "text/event-stream"
            req = urllib.request.Request(self.base + path, method=method, headers=headers)
            try:
                answer = urllib.request.urlopen(req, timeout=timeout)
            except urllib.error.HTTPError as e:
                text = e.read().decode("utf-8", "replace")
                try:
                    body = json.loads(text)
                except ValueError:
                    body = {"error": text[:300]}
                return e.code, body
            except OSError as e:
                raise RuntimeError("Director unreachable: %s" % e) from e
            if stream:
                return answer.status, answer
            with answer:
                raw = answer.read()
            if "json" not in (answer.headers.get("Content-Type") or ""):
                return answer.status, raw
            return answer.status, json.loads(raw or b"{}")
        if stream:
            return attempt()
        return self._once_more_on_unauthorized(attempt)

    def get_job(self, job_id: str) -> dict | None:
        status, body = self.request("GET", "jobs/%s" % urllib.parse.quote(str(job_id), safe=""), timeout=15)
        if status != 200 or not isinstance(body, dict):
            return None
        return body.get("job") if isinstance(body.get("job"), dict) else body

    def post_json(
        self,
        method: str,
        path: str,
        payload: dict,
        *,
        idempotency_key: str,
        timeout: float = 30,
    ) -> tuple[int, dict | str]:
        def attempt():
            headers = {
                "Authorization": "Bearer " + self.token(),
                "Content-Type": "application/json",
                "Idempotency-Key": idempotency_key,
            }
            data = json.dumps(payload).encode("utf-8")
            req = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
            try:
                answer = urllib.request.urlopen(req, timeout=timeout)
            except urllib.error.HTTPError as e:
                text = e.read().decode("utf-8", "replace")
                try:
                    body = json.loads(text)
                except ValueError:
                    body = {"error": text[:300]}
                return e.code, body
            except OSError as e:
                raise RuntimeError("Director unreachable: %s" % e) from e
            with answer:
                raw = answer.read()
            if "json" not in (answer.headers.get("Content-Type") or ""):
                return answer.status, raw.decode("utf-8", "replace")
            return answer.status, json.loads(raw or b"{}")
        return self._once_more_on_unauthorized(attempt)
    def record_wake_reply(self, conversation_id: str, nudge: str, reply: str, *, idempotency_key: str | None = None) -> bool:
        jobs = nudge_jobs(nudge)
        key = idempotency_key or ("wake-%s" % hashlib.sha256(("%s\n%s" % (nudge, reply)).encode("utf-8")).hexdigest()[:48])
        path = "conversations/%s/wake-turns" % urllib.parse.quote(conversation_id, safe="")
        status, _body = self.post_json(
            "POST",
            path,
            {"nudge": nudge, "reply": reply, "jobs": jobs},
            idempotency_key=key,
            timeout=30,
        )
        return status in (200, 201)


def parse_sse_lines(raw: bytes) -> tuple[str | None, str | None]:
    line = raw.decode("utf-8", "replace").rstrip("\r\n")
    if line.startswith("event:"):
        return line.split(":", 1)[1].strip(), None
    if line.startswith("data:"):
        return None, line.split(":", 1)[1].strip()
    return None, None


class WakeCoordinator:
    """Background events loop + nudge batching for one conversation host."""

    def __init__(
        self,
        host,
        *,
        director_url: str,
        credential_file: Path,
        stall_s: float | None = None,
        batch_s: float | None = None,
        client_factory: Callable[[str, Path], DirectorClient] | None = None,
    ):
        self._host = host
        self._client = (client_factory or DirectorClient)(director_url, credential_file)
        self._stall_s = stall_s if stall_s is not None else _stall_seconds()
        self._batch_s = batch_s if batch_s is not None else _batch_seconds()
        self._watch: dict[str, str | None] = {}
        self._known_state: dict[str, str] = {}
        self._notified: set[str] = set()
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._flusher: threading.Thread | None = None
        self._flush_wake = threading.Event()
        self._flush_now = False
        self._spool_backoff = WAKE_SPOOL_BACKOFF_S
        self._last_event_at = time.monotonic()
        self._pending: list[str] = []
        self._batch_deadline = 0.0

    def _session_dir(self) -> Path:
        from _lead_host import sessions_root

        return sessions_root(self._host.state) / self._host.conversation_id

    def _spool_dir(self) -> Path:
        return self._session_dir() / WAKE_SPOOL_DIR

    def _spool_path(self, key: str) -> Path:
        safe = re.sub(r"[^A-Za-z0-9._-]", "_", str(key))
        return self._spool_dir() / ("%s.json" % safe)

    def _watches_path(self) -> Path:
        return self._session_dir() / WATCHES_FILE

    def _persist_watches(self) -> None:
        with self._lock:
            data = {jid: lane for jid, lane in self._watch.items()}
        path = self._watches_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name("%s.tmp-%s" % (path.name, uuid.uuid4().hex))
        tmp.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(tmp, path)
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass

    def _load_persisted_watches(self) -> None:
        path = self._watches_path()
        if not path.is_file():
            return
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return
        if not isinstance(data, dict):
            return
        with self._lock:
            for job_id, lane in data.items():
                job_id = str(job_id or "").strip()
                if job_id:
                    self._watch[job_id] = str(lane) if lane is not None else None

    def blocks_idle_exit(self) -> bool:
        with self._lock:
            for job_id in self._watch:
                if self._known_state.get(job_id, "") not in ENDED:
                    return True
        return False

    def start(self) -> None:
        self._load_persisted_watches()
        for job_id in list(self._watch):
            try:
                self._refetch_job(job_id, persist=False)
            except Exception:
                pass
        self._retry_spool(force=True)
        if not (self._flusher and self._flusher.is_alive()):
            self._flusher = threading.Thread(target=self._run_flusher, name="lead-wake-flusher", daemon=True)
            self._flusher.start()
        if self._thread and self._thread.is_alive():
            return
        self._thread = threading.Thread(target=self._run, name="lead-wake", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._flush_wake.set()

    def watch_job(self, job_id: str, *, lane: str | None = None) -> None:
        job_id = str(job_id or "").strip()
        if not job_id:
            return
        with self._lock:
            self._watch[job_id] = lane
        self._persist_watches()
        try:
            self._refetch_job(job_id, persist=False)
        except Exception:
            pass

    def drop_watches_for_nudge(self, nudge: str) -> None:
        changed = False
        with self._lock:
            for job_id in nudge_jobs(nudge):
                if job_id in self._watch:
                    self._watch.pop(job_id, None)
                    changed = True
        if changed:
            self._persist_watches()

    def queue_nudge(self, text: str) -> None:
        with self._lock:
            self._pending.append(text)
            self._batch_deadline = time.monotonic() + self._batch_s

    def post_wake_reply(self, conversation_id: str, nudge: str, reply: str) -> bool:
        jobs = nudge_jobs(nudge)
        key = "wake-%s" % hashlib.sha256(("%s\n%s" % (nudge, reply)).encode("utf-8")).hexdigest()[:48]
        payload = {
            "conversation": conversation_id,
            "nudge": nudge,
            "reply": reply,
            "jobs": jobs,
            "key": key,
            "created_at": time.time(),
        }
        path = self._spool_path(key)
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".part")
        tmp.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(tmp, path)
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
        if self._deliver_spool(path, payload):
            self._spool_backoff = WAKE_SPOOL_BACKOFF_S
            return True
        return False

    def _deliver_spool(self, path: Path, payload: dict) -> bool:
        conversation_id = str(payload.get("conversation") or "")
        nudge = str(payload.get("nudge") or "")
        reply = str(payload.get("reply") or "")
        key = str(payload.get("key") or "")
        created = float(payload.get("created_at") or 0)
        if created and time.time() - created >= WAKE_SPOOL_MAX_AGE_S:
            self._host._wake_log("WAKE-SPOOL-ABANDONED: %s" % path.name)
            path.unlink(missing_ok=True)
            return False
        if self._client.record_wake_reply(conversation_id, nudge, reply, idempotency_key=key):
            path.unlink(missing_ok=True)
            return True
        return False

    def _retry_spool(self, *, force: bool = False) -> None:
        spool = self._spool_dir()
        if not spool.is_dir():
            self._spool_backoff = WAKE_SPOOL_BACKOFF_S
            return
        paths = sorted(spool.glob("*.json"))
        if not paths:
            self._spool_backoff = WAKE_SPOOL_BACKOFF_S
            return
        for path in paths:
            try:
                payload = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            if isinstance(payload, dict):
                self._deliver_spool(path, payload)
        if force:
            return
        if any(self._spool_dir().glob("*.json")):
            self._spool_backoff = min(WAKE_SPOOL_BACKOFF_MAX_S, max(WAKE_SPOOL_BACKOFF_S, self._spool_backoff * 2))

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                stream = self._open_stream()
                if stream is None:
                    time.sleep(0.5)
                    continue
                try:
                    self._resync_after_hello()
                    self._follow(stream)
                finally:
                    try:
                        stream.close()
                    except OSError:
                        pass
            except Exception:
                time.sleep(1.0)

    def _run_flusher(self) -> None:
        next_spool_at = 0.0
        while not self._stop.is_set():
            respect = not self._flush_now
            self._flush_wake.wait(timeout=0.25)
            self._flush_wake.clear()
            self._flush_now = False
            now = time.monotonic()
            if now >= next_spool_at:
                self._retry_spool()
                next_spool_at = now + self._spool_backoff
            self._flush(respect_deadline=respect)

    def _open_stream(self):
        try:
            status, stream = self._client.request("GET", "events", stream=True, timeout=20)
        except Exception:
            return None
        if status != 200:
            return None
        try:
            while True:
                raw = stream.readline()
                if not raw:
                    stream.close()
                    return None
                event, _ = parse_sse_lines(raw)
                if event == "hello" or raw.startswith(b"event: hello") or raw.startswith(b"event:hello"):
                    return stream
        except OSError:
            try:
                stream.close()
            except OSError:
                pass
            return None

    def _resync_after_hello(self) -> None:
        with self._lock:
            ids = list(self._watch)
        for job_id in ids:
            self._refetch_job(job_id, persist=False)

    def _refetch_job(self, job_id: str, *, persist: bool = True) -> None:
        job = self._client.get_job(job_id)
        if not job:
            return
        state = str(job.get("state") or "")
        with self._lock:
            lane = self._watch.get(job_id)
            self._known_state[job_id] = state
        if persist:
            self._persist_watches()
        if state in ENDED:
            self._maybe_notify(job_id, job, lane=lane)

    def _follow(self, stream) -> None:
        event: str | None = None
        next_stall = time.monotonic() + self._stall_s if self._stall_s > 0 else None
        sock = getattr(getattr(stream, "fp", None), "raw", None)
        sock_obj = getattr(sock, "_sock", sock)
        while not self._stop.is_set():
            if next_stall is not None and time.monotonic() >= next_stall:
                self._stall_refetch()
                next_stall = time.monotonic() + self._stall_s
            if sock_obj is not None:
                try:
                    sock_obj.settimeout(1.0)
                except OSError:
                    pass
            try:
                raw = stream.readline()
            except (OSError, TimeoutError):
                return
            if not raw:
                return
            if raw.startswith(b":") or raw.strip() == b"":
                continue
            ev, data = parse_sse_lines(raw)
            if ev is not None:
                event = ev
                if event in ("revoked", "resync"):
                    return
                continue
            if data is None:
                continue
            self._last_event_at = time.monotonic()
            if event == "change":
                try:
                    payload = json.loads(data)
                except ValueError:
                    continue
                if payload.get("kind") != "job":
                    continue
                job_id = str(payload.get("id") or "")
                with self._lock:
                    if job_id not in self._watch:
                        continue
                    lane = self._watch[job_id]
                record = payload.get("record") if isinstance(payload.get("record"), dict) else {}
                state = str(record.get("state") or "")
                with self._lock:
                    self._known_state[job_id] = state
                if state not in ENDED:
                    continue
                job = self._client.get_job(job_id) or record
                self._maybe_notify(job_id, job, lane=lane)

    def _stall_refetch(self) -> None:
        with self._lock:
            if not self._watch:
                return
            ids = list(self._watch)
        for job_id in ids:
            self._refetch_job(job_id, persist=False)

    def _maybe_notify(self, job_id: str, job: dict, *, lane: str | None) -> None:
        with self._lock:
            if job_id in self._notified:
                return
            self._notified.add(job_id)
        self.queue_nudge(format_nudge(job, lane=lane))

    def tick_batch(self) -> None:
        if self._flusher and self._flusher.is_alive():
            self._signal_flush(respect_deadline=True)
        else:
            self._flush(respect_deadline=True)

    def force_flush(self) -> None:
        if self._flusher and self._flusher.is_alive():
            self._signal_flush(respect_deadline=False)
        else:
            self._flush(respect_deadline=False)

    def _signal_flush(self, *, respect_deadline: bool) -> None:
        self._flush_now = not respect_deadline
        self._flush_wake.set()

    def _flush(self, *, respect_deadline: bool) -> None:
        with self._lock:
            if not self._pending:
                return
            if respect_deadline and time.monotonic() < self._batch_deadline:
                return
            batch = list(self._pending)
            self._pending.clear()
            self._batch_deadline = 0.0
        ended = set()
        for part in batch:
            ended.update(nudge_jobs(part))
        nudge = "\n".join(batch)
        try:
            extra = format_other_lanes_line(
                self._client,
                self._host.conversation_id,
                exclude_job_ids=ended,
            )
        except Exception:
            extra = None
        if extra:
            nudge = nudge + "\n" + extra
        if not self._host.deliver_nudge(nudge):
            with self._lock:
                self._pending[:0] = batch
            self._signal_flush(respect_deadline=False)


def _wake_reply_seconds() -> float:
    raw = os.environ.get("LEAD_WAKE_REPLY_TIMEOUT_S", "").strip()
    if not raw:
        return DEFAULT_WAKE_REPLY_S
    try:
        return max(30.0, float(raw))
    except ValueError:
        return DEFAULT_WAKE_REPLY_S


def _batch_seconds() -> float:
    raw = os.environ.get("LEAD_WAKE_BATCH_S", "").strip()
    if not raw:
        return DEFAULT_BATCH_S
    try:
        return max(0.0, float(raw))
    except ValueError:
        return DEFAULT_BATCH_S


def _stall_seconds() -> float:
    raw = os.environ.get("LEAD_WAKE_STALL_S", "").strip()
    if not raw:
        return DEFAULT_STALL_S
    try:
        return max(0.0, float(raw))
    except ValueError:
        return DEFAULT_STALL_S


def wake_enabled() -> bool:
    return os.environ.get("LEAD_HOST_WAKE", "1").strip().lower() not in ("0", "false", "no")


def _log_no_wake(host, why: str) -> None:
    """A serve without wake-ups must say so: watch requests still succeed, so silence would hide it."""
    log = getattr(host, "_wake_log", None)
    if callable(log):
        log("WAKE-DISABLED: %s" % why)


def start_wake(host, config_root: Path) -> WakeCoordinator | None:
    if not wake_enabled():
        return None
    try:
        meta = host.conversation_meta()
    except Exception:
        meta = None
    url = resolve_director_url(config_root, meta)
    if not url:
        _log_no_wake(host, "no Director address for wake-ups")
        return None
    try:
        cred = host.wake_credential_path()
    except Exception:
        return None
    from _lead_host import refetch_lead_credential_from_director

    def client_factory(base_url: str, credential_file: Path):
        return DirectorClient(
            base_url,
            credential_file,
            refetch_credential=lambda: refetch_lead_credential_from_director(
                host.conversation_id, config_root, host.state),
        )

    coord = WakeCoordinator(host, director_url=url, credential_file=cred, client_factory=client_factory)
    coord.start()
    return coord
