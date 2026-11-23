"""The S3 operations used by cs backup, signed with AWS Signature Version 4."""
import hashlib
import hmac
import http.client
import ipaddress
import re
import shutil
import xml.etree.ElementTree as xml
from contextlib import contextmanager
from datetime import datetime, timezone
from urllib.parse import quote, urlsplit


class BackupError(Exception):
    """An operator-facing error that contains no credential or remote response body."""


def encode(value):
    return quote(value, safe="-_.~")


class S3:
    def __init__(self, config, access, secret):
        endpoint = urlsplit(config["endpoint"])
        host = endpoint.hostname
        loopback = False
        try:
            loopback = ipaddress.ip_address(host).is_loopback
        except ValueError:
            pass
        if (not host or endpoint.username is not None or endpoint.path not in ("", "/")
                or endpoint.query or endpoint.fragment
                or (endpoint.scheme != "https" and not (endpoint.scheme == "http" and loopback))):
            raise BackupError("backup endpoint must be an HTTPS origin (HTTP is allowed on loopback)")
        self.connection = http.client.HTTPSConnection if endpoint.scheme == "https" else http.client.HTTPConnection
        self.host, self.port, self.authority = host, endpoint.port, endpoint.netloc
        self.bucket = config["bucket"]
        self.region = config.get("region", host.split(".")[0] if host.endswith(".digitaloceanspaces.com")
                                 else "us-east-1")
        if not re.fullmatch(r"[a-z0-9-]+", self.region):
            raise BackupError("invalid backup signing region")
        self.access, self.secret = access, secret

    @contextmanager
    def request(self, method, key="", query=None, body=None):
        path = "/" + self.bucket + ("/" + quote(key, safe="/-_.~") if key else "")
        params = sorted((encode(k), encode(v)) for k, v in (query or {}).items())
        query_string = "&".join(f"{k}={v}" for k, v in params)
        now = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        day = now[:8]
        digest = hashlib.sha256()
        if body is not None:
            for block in iter(lambda: body.read(1024 * 1024), b""):
                digest.update(block)
            body.seek(0)
        payload = digest.hexdigest()
        headers = {"host": self.authority, "x-amz-content-sha256": payload, "x-amz-date": now}
        signed = ";".join(headers)
        canonical_headers = "".join(f"{name}:{value}\n" for name, value in headers.items())
        canonical = "\n".join((method, path, query_string, canonical_headers, signed, payload))
        scope = f"{day}/{self.region}/s3/aws4_request"
        message = "\n".join(("AWS4-HMAC-SHA256", now, scope, hashlib.sha256(canonical.encode()).hexdigest()))
        signing_key = b"AWS4" + self.secret
        for part in scope.split("/"):
            signing_key = hmac.digest(signing_key, part.encode(), "sha256")
        signature = hmac.digest(signing_key, message.encode(), "sha256").hex()
        headers["Authorization"] = (
            f"AWS4-HMAC-SHA256 Credential={self.access}/{scope}, SignedHeaders={signed}, Signature={signature}")
        if body is not None:
            body.seek(0, 2)
            headers["Content-Length"] = str(body.tell())
            body.seek(0)
        connection = self.connection(self.host, self.port, timeout=60)
        try:
            target = f"{path}?{query_string}" if query_string else path
            connection.request(method, target, body=body, headers=headers)
            response = connection.getresponse()
            if not 200 <= response.status < 300:
                raise BackupError(f"S3 {method} failed (HTTP {response.status})")
            yield response
        except (OSError, http.client.HTTPException, ValueError):
            raise BackupError(f"S3 {method} connection or transfer failed") from None
        finally:
            connection.close()

    def download(self, key, target):
        with self.request("GET", key) as response, target.open("wb") as output:
            shutil.copyfileobj(response, output)

    def list(self, prefix):
        params = {"list-type": "2", "prefix": prefix}
        seen = set()
        while True:
            with self.request("GET", query=params) as response:
                data = response.read(8 * 1024 * 1024 + 1)
            try:
                if len(data) > 8 * 1024 * 1024:
                    raise BackupError("S3 listing is too large")
                page = xml.fromstring(data)
                if page.tag.rsplit("}", 1)[-1] != "ListBucketResult":
                    raise BackupError("invalid S3 listing")
            except xml.ParseError:
                raise BackupError("invalid S3 listing") from None
            for item in page.findall("{*}Contents"):
                key = item.findtext("{*}Key", "")
                if key.startswith(prefix):
                    yield key
            more = page.findtext("{*}IsTruncated")
            if more == "false":
                return
            token = page.findtext("{*}NextContinuationToken")
            if more != "true" or not token or token in seen:
                raise BackupError("invalid S3 listing pagination")
            seen.add(token)
            params["continuation-token"] = token
