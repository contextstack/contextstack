"""Best-effort report of a cs land outcome to the Director.

The landing decision is already made. This posts it to
POST /api/v1/nodes/me/runs/{run_id}/land with the node token and one
Idempotency-Key. The run is --job's, or the newest finished Client API job
whose meta names this repo and lane (the owner credential, as cs mcp reads it).
A miss is reported on the result and does not change the landing.
"""
import hashlib
import json
import os
import re
import sys
import time
import urllib.parse
import uuid
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from _director_client import Client, DirectorUnreachable, receiver_config  # noqa: E402
from _node_env import node_env  # noqa: E402

FINISHED = ("done", "error")
JOB_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
FAIL_LINE = re.compile(r"\bFAIL\b|^FAILED|^error:")
_MCP = None


def record_landing(result, job_id, root):
    """Return {recorded: True, land_id} or {recorded: False, record_error}. Never raises."""
    try:
        return _record(result if isinstance(result, dict) else {}, (job_id or "").strip() or None, Path(root))
    except Exception as error:
        return {"recorded": False, "record_error": _short(error)}


def _record(result, job_id, root):
    if job_id is not None and not JOB_ID.fullmatch(job_id):
        return _miss("invalid job id")
    config = root / "config"
    install = _read_json(config / "install.json")
    receiver = _receiver(config / "policy/spool.json")
    url = _director_url(install, receiver)
    credential = root / "state/director/local-credential"
    if not url or not credential.is_file():
        return _miss("no Director configured to record this landing")
    repo, lane = str(result.get("repo") or ""), str(result.get("lane") or "")
    run_id, err = _resolve_run(url, credential, repo, lane, job_id)
    if err:
        return _miss(err)
    if not isinstance(receiver, dict):
        return _miss("no director receiver; the landing could not be recorded")
    try:
        cfg = receiver_config(receiver)
    except ValueError as error:
        return _miss(error)
    # node.env is the store. A variable already in the environment covers a
    # machine that exported token_env and a test that cannot create *.env.
    token = (node_env(config / "node.env").get(cfg["token_env"])
             or os.environ.get(cfg["token_env"]) or "")
    if not token:
        return _miss("director token %s is not set" % cfg["token_env"])
    body = _body(result, landed_by=_landed_by(receiver, install), landed_at=_now(),
                 landing_branch=_landing_branch(repo))
    key = str(uuid.uuid4())
    try:
        answer = Client(cfg, timeout=10, token=token).call(
            "POST", "runs/%s/land" % urllib.parse.quote(run_id, safe=""), body=body,
            headers={"Idempotency-Key": key})
    except DirectorUnreachable as error:
        return _miss(error)
    land_id = _land_id(answer)
    if not land_id:
        return _miss("director answer has no land_id")
    return {"recorded": True, "land_id": land_id}


def _resolve_run(url, credential, repo, lane, job_id):
    """The run to report: --job's run, else the newest finished job for this repo and lane."""
    director, refused = _owner(url, credential)
    if job_id:
        body, err = _get(director, refused, "jobs/" + urllib.parse.quote(job_id, safe=""))
        if err:
            return None, err
        job = _as_job(body)
        if job is None:
            return None, "job %s was not found" % job_id
        run_id = _run_id(job)
        return (run_id, None) if run_id else (None, "job %s has no run" % job_id)
    body, err = _get(director, refused, "jobs")
    if err:
        return None, err
    jobs = body.get("jobs") if isinstance(body, dict) else None
    if not isinstance(jobs, list):
        return None, "GET jobs returned no job list"
    job = newest_finished_job(jobs, repo, lane)
    if job is None:
        return None, "no finished job for repo %s lane %s" % (repo, lane)
    run_id = _run_id(job)
    return (run_id, None) if run_id else (None, "job %s has no run" % job.get("job_id"))


def _parse_instant(text):
    raw = str(text or "").strip()
    if not raw:
        return None
    if raw.endswith("Z"):
        raw = raw[:-1] + "+00:00"
    try:
        return datetime.fromisoformat(raw)
    except ValueError:
        return None


def _utc_z(instant):
    if instant.tzinfo is None:
        instant = instant.replace(tzinfo=timezone.utc)
    else:
        instant = instant.astimezone(timezone.utc)
    return instant.strftime("%Y-%m-%dT%H:%M:%SZ")


def _job_instant(job):
    return _parse_instant(job.get("finished_at") or job.get("created_at") or "")


def newest_finished_job(jobs, repo, lane, before=None):
    """Newest finished job whose meta names this repo and lane. Ties keep the earlier list entry
    (the Client API lists jobs newest first). When `before` is set, only jobs finished strictly
    before that instant count (ISO-8601 string or datetime)."""
    before_dt = _parse_instant(before) if isinstance(before, str) else before
    matched = []
    for index, job in enumerate(jobs):
        if not isinstance(job, dict) or job.get("state") not in FINISHED:
            continue
        meta = job.get("meta") if isinstance(job.get("meta"), dict) else {}
        if meta.get("repo") != repo or meta.get("lane") != lane:
            continue
        if before_dt is not None:
            finished = _job_instant(job)
            if finished is None or finished >= before_dt:
                continue
        matched.append((str(job.get("finished_at") or job.get("created_at") or ""), -index, job))
    if not matched:
        return None
    matched.sort(reverse=True)
    return matched[0][2]


def _newest(jobs, repo, lane):
    return newest_finished_job(jobs, repo, lane)


def backfill_idempotency_key(run_id, commit):
    digest = hashlib.sha256(("%s\0%s" % (run_id, commit)).encode()).hexdigest()
    return "land-backfill:%s" % digest


def landed_body(repo, lane, main, commit, landed_by, landed_at):
    return _body({"status": "landed", "repo": repo, "lane": lane, "main": main, "commit": commit},
                 landed_by=landed_by, landed_at=landed_at)


def recorder_session(root):
    """Director client plus owner job list for backfill. Never raises; (session, err) on miss."""
    try:
        return _recorder_session(Path(root)), None
    except Exception as error:
        return None, _short(error)


def _recorder_session(root):
    config = root / "config"
    install = _read_json(config / "install.json")
    receiver = _receiver(config / "policy/spool.json")
    url = _director_url(install, receiver)
    credential = root / "state/director/local-credential"
    if not url or not credential.is_file():
        raise ValueError("no Director configured to record landings")
    if not isinstance(receiver, dict):
        raise ValueError("no director receiver")
    cfg = receiver_config(receiver)
    token = (node_env(config / "node.env").get(cfg["token_env"])
             or os.environ.get(cfg["token_env"]) or "")
    if not token:
        raise ValueError("director token %s is not set" % cfg["token_env"])
    director, refused = _owner(url, credential)
    body, err = _get(director, refused, "jobs")
    if err:
        raise ValueError(err)
    jobs = body.get("jobs") if isinstance(body, dict) else None
    if not isinstance(jobs, list):
        raise ValueError("GET jobs returned no job list")
    return {"client": Client(cfg, timeout=10, token=token), "landed_by": _landed_by(receiver, install), "jobs": jobs}


def post_land_record(client, run_id, body, key):
    """Return ('recorded'|'already'|'error', land_id or message). Never raises."""
    try:
        answer = client.call(
            "POST", "runs/%s/land" % urllib.parse.quote(run_id, safe=""), body=body,
            headers={"Idempotency-Key": key})
    except DirectorUnreachable as error:
        if error.status == 409 and isinstance(error.answer, dict) and error.answer.get("code") == "already_landed":
            land = error.answer.get("land") if isinstance(error.answer.get("land"), dict) else error.answer
            return "already", _land_id(land) or ""
        return "error", _short(error)
    land_id = _land_id(answer)
    if not land_id:
        return "error", "director answer has no land_id"
    if isinstance(answer, dict) and answer.get("_idempotent_replay"):
        return "already", land_id
    return "recorded", land_id


def _landing_branch(repo):
    """Target branch from repos.json (`branch` key), not land()'s main checkout path."""
    name = str(repo or "").strip()
    if not name:
        return "main"
    try:
        from _lanes import LaneError, repo_config
        return str(repo_config(name).get("branch") or "main")
    except LaneError:
        return "main"


def _body(result, landed_by, landed_at, landing_branch=None):
    status = result.get("status")
    if status == "landed":
        state, commit, reason = "landed", result.get("commit"), None
    elif status == "red":
        state, commit, reason = "failed", None, _fail_reason(result)
    else:
        state, commit, reason = "refused", None, _refuse_reason(result)
    main = landing_branch if isinstance(landing_branch, str) and landing_branch else _landing_branch(
        result.get("repo"))
    return {"state": state, "repo": str(result.get("repo") or ""), "lane": str(result.get("lane") or ""),
            "main": main, "commit": commit if isinstance(commit, str) else None,
            "reason": reason, "landed_by": landed_by, "landed_at": landed_at}


def _fail_reason(result):
    failed = [str(line).strip() for line in (result.get("failed") or []) if str(line).strip()]
    for line in failed:
        if FAIL_LINE.search(line):
            return line
    if failed:
        return failed[0]
    return _refuse_reason(result) if result.get("reason") else "check failed"


def _refuse_reason(result):
    reason = result.get("reason")
    if isinstance(reason, str) and reason.strip():
        return reason.strip()
    failed = result.get("failed") or []
    if failed and str(failed[0]).strip():
        return str(failed[0]).strip()
    return "refused"


def _run_id(job):
    run = job.get("run_id")
    if isinstance(run, str) and run:
        return run
    for item in job.get("run_ids") or []:
        if isinstance(item, str) and item:
            return item
    return None


def _as_job(body):
    if not isinstance(body, dict):
        return None
    if isinstance(body.get("job"), dict):
        return body["job"]
    return body if body.get("job_id") else None


def _land_id(answer):
    if not isinstance(answer, dict):
        return None
    if isinstance(answer.get("land_id"), str) and answer["land_id"]:
        return answer["land_id"]
    for wrap in ("land", "record"):
        inner = answer.get(wrap)
        if isinstance(inner, dict) and isinstance(inner.get("land_id"), str) and inner["land_id"]:
            return inner["land_id"]
    return None


def _director_url(install, receiver):
    raw = install.get("director") if isinstance(install, dict) else None
    if not raw and isinstance(receiver, dict):
        raw = receiver.get("url")
    if not isinstance(raw, str) or not raw.startswith(("http://", "https://")):
        return None
    return raw.rstrip("/")


def _landed_by(receiver, install):
    node = receiver.get("node") if isinstance(receiver, dict) else None
    if not node and isinstance(install, dict):
        node = install.get("node")
    return str(node or "local")


def _receiver(path):
    data = _read_json(path)
    receiver = data.get("receiver") if isinstance(data, dict) else None
    return receiver if isinstance(receiver, dict) and receiver.get("kind") == "director" else None


def _read_json(path):
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def _owner(url, credential):
    global _MCP
    if _MCP is None:
        import importlib.util
        path = Path(__file__).resolve().parents[2] / "scripts" / "cs_mcp.py"
        spec = importlib.util.spec_from_file_location("_land_record_mcp", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        _MCP = module
    return _MCP.Director(url, credential), _MCP.Refused


def _get(director, refused, path):
    try:
        status, body = director.request("GET", path, timeout=10)
    except refused as error:
        return None, _short(error)
    except OSError as error:
        return None, _short(error)
    if status != 200:
        return None, _short("GET %s: %s %s" % (path, status, _refusal(body)))
    return body, None


def _refusal(body):
    if isinstance(body, dict):
        return " ".join(str(body[k]) for k in ("code", "detail", "error") if body.get(k))
    return str(body)[:300]


def _miss(error):
    return {"recorded": False, "record_error": _short(error)}


def _short(error):
    text = " ".join(str(error).split())
    return text[:300]


def _now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
