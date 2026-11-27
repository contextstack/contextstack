"""Owner configuration shared by schedule-enable, schedule-tick and browser-broker."""
import hashlib
import json
import math
import os
import re
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit

from _schedule_delivery import destinations

PREFIX = "price-watch-"


def watch_name(name):
    if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,31}", name):
        raise ValueError("watch name must be 1–32 lowercase ASCII letters, digits or hyphens")
    return name


def watch_directory():
    return Path(os.environ.get("CS_CONFIG_ROOT") or Path.home() / ".contextstack/config") / "watches"


def https_url(value):
    if not isinstance(value, str) or len(value) > 4096 or any(c.isspace() or ord(c) < 32 for c in value):
        raise ValueError("watch url must be a valid public HTTPS URL")
    parts = urlsplit(value)
    host = (parts.hostname or "").rstrip(".").encode("idna").decode("ascii")
    if (parts.scheme != "https" or parts.username is not None or parts.password is not None or
            parts.port not in (None, 443) or "." not in host or host.endswith(".local") or
            any(not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?", label) for label in host.split("."))):
        raise ValueError("watch url must use a public HTTPS origin on port 443")
    # Chrome accepts alternative encodings of numeric IPv4 addresses. Require a DNS hostname.
    if host.rsplit(".", 1)[1].isdigit():
        raise ValueError("watch url must use a public DNS hostname")
    return urlunsplit(("https", host, parts.path or "/", parts.query, "")), "https://" + host


def load_watch(name):
    path = watch_directory() / (watch_name(name) + ".json")
    config = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(config, dict) or set(config) != {"url", "extract", "threshold", "currency", "delivery"}:
        raise ValueError("watch config requires url, extract, threshold, currency and delivery")
    url, origin = https_url(config["url"])
    extract = config["extract"]
    if (not isinstance(extract, dict) or
            set(extract) not in ({"selector", "number_parser"}, {"regex", "number_parser"}) or
            extract["number_parser"] not in ("decimal_dot", "decimal_comma")):
        raise ValueError("extract needs selector or regex, and number_parser decimal_dot or decimal_comma")
    pattern = extract.get("selector", extract.get("regex"))
    if not isinstance(pattern, str) or not pattern.strip() or len(pattern) > 4096:
        raise ValueError("selector or regex must be a nonempty string of at most 4096 characters")
    if "regex" in extract:
        try:
            if re.compile(pattern).groups > 1:
                raise ValueError("price regex accepts at most one capture group")
        except (re.error, OverflowError, RecursionError) as error:
            raise ValueError("invalid price regex: " + str(error)) from error
    threshold = config["threshold"]
    if not isinstance(threshold, dict) or set(threshold) not in ({"below"}, {"above"}):
        raise ValueError("threshold needs exactly one of below or above")
    amount = next(iter(threshold.values()))
    if type(amount) not in (int, float) or not 0 <= amount < math.inf:
        raise ValueError("threshold must be a finite nonnegative number")
    if not isinstance(config["currency"], str) or not re.fullmatch(r"[A-Z]{3}", config["currency"]):
        raise ValueError("currency must be a three-letter uppercase code")
    destinations(config["delivery"])
    config["url"] = url
    config["origin"] = origin
    return config


def config_digest(config):
    return hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest()
