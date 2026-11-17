"""Short, redacted reason for an errored Director job end, and its per-job log. Not an op.

A job's terminal row may already carry `stop` / `error.detail` / `note`; this module picks one short string
(`reason`, at most REASON_CHARS) for the Director's Client API `error` field, and appends the trial's stderr
to `<state>/jobs/<id>/node.log` so one job's output is never overwritten by the next (node.err.log keeps only
the latest).
"""
import os
import re
from pathlib import Path

REASON_CHARS = 2000
SECRET_NAME = r"[A-Z][A-Z0-9_]*(?:_API_KEY|_TOKEN|_SECRET|_PASSWORD)"
# Director bearer tokens are prefix + 32 random bytes as lower-case hex (director/src/util.rs secret()).
_DIRECTOR_TOKEN = r"(?:csl_|csn_|csd_)[0-9a-f]{64}"
# Whole private-key blocks, headers included, so a tail cut cannot keep the body.
_PEM = r"(?i)-----BEGIN [A-Z0-9 ]{0,80}PRIVATE KEY-----[\s\S]{0,20000}?-----END [A-Z0-9 ]{0,80}PRIVATE KEY-----"
SECRET_SHAPES = (
    (re.compile(_PEM), "<redacted>"),
    (re.compile(r"(?i)([a-z][a-z0-9+.-]*://)([^/\s:@]+):([^/\s@]+)@"), r"\1<redacted>@"),
    (re.compile(r"(?i)(authorization\s*:\s*basic\s+)\S+"), r"\1<redacted>"),
    (re.compile(r"(?i)(authorization\s*:\s*bearer\s+)\S+"), r"\1<redacted>"),
    (re.compile(r"\b(%s)([\"']?\s*(?:=>|=|:)\s*[\"']?)([^\s\"',}]+)" % SECRET_NAME), r"\1\2<redacted>"),
    (re.compile(r"(?i)\b(bearer)(\s+)([A-Za-z0-9._~+/=-]{6,})"), r"\1\2<redacted>"),
    (re.compile(r"()()\b(" + _DIRECTOR_TOKEN + r")\b"), r"\1\2<redacted>"),
    (re.compile(r"()()\b((?:cslead_|sk-ant-)[A-Za-z0-9_-]+)"), r"\1\2<redacted>"),
    (re.compile(r"()()\b(sk-[A-Za-z0-9_-]{8,})"), r"\1\2<redacted>"),
    (re.compile(r"()()\b((?:gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+))"), r"\1\2<redacted>"),
    (re.compile(r"()()\b(xox[abpr]-[A-Za-z0-9-]{10,})"), r"\1\2<redacted>"),
    (re.compile(r"()()\b(AKIA[A-Z0-9]{16}|AIza[A-Za-z0-9_-]{35})\b"), r"\1\2<redacted>"),
    (re.compile(r"()()\b(eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_.-]+)"), r"\1\2<redacted>"),
)
JOB_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")


def redact(text):
    """Drop credential shapes from transcripts, events, and Client API output."""
    text = str(text or "")
    for pattern, replacement in SECRET_SHAPES:
        text = pattern.sub(replacement, text)
    return text


def redact_obj(value):
    """Redact every string in a JSON-shaped value. Keys and non-strings stay."""
    if isinstance(value, str):
        return redact(value)
    if isinstance(value, dict):
        return {key: redact_obj(item) for key, item in value.items()}
    if isinstance(value, list):
        return [redact_obj(item) for item in value]
    return value


def persist_output(path, text):
    """Write a transcript or run log with credential shapes removed."""
    body = redact(text)
    Path(path).write_text(body, encoding="utf-8")
    return body


def clip(text, limit=REASON_CHARS):
    raw = redact(text).encode("utf-8")
    return raw[:limit].decode("utf-8", "ignore")


def reason_of(row, stderr_tail=""):
    """Prefer an explicit reason, then stop, error.detail, note, then the stderr tail; always redacted and bounded."""
    if not isinstance(row, dict):
        row = {}
    err = row.get("error")
    detail = ""
    if isinstance(err, dict):
        detail = str(err.get("detail") or "")
    elif isinstance(err, str):
        detail = err
    for value in (row.get("reason"), row.get("stop"), detail, row.get("note"), stderr_tail):
        text = str(value or "").strip()
        if text:
            return clip(text)
    return ""


def job_log_path(state, job_id):
    """`<state>/jobs/<id>/node.log` when job_id is a plain id; None otherwise."""
    if not JOB_ID.fullmatch(str(job_id or "")):
        return None
    return Path(state) / "jobs" / str(job_id) / "node.log"


def append_job_log(path, text):
    """Append redacted text to a per-job log; create the job dir. No-op when path is None or text empty."""
    if path is None:
        return
    body = redact(text)
    if not body:
        return
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a", encoding="utf-8") as out:
        out.write(body if body.endswith("\n") else body + "\n")
        out.flush()
        os.fsync(out.fileno())
