#!/usr/bin/env python3
"""Shared secure transport and evidence store for corpus web tools."""
import hashlib, html, ipaddress, json, os, re, socket, subprocess, tempfile
from datetime import datetime, timezone
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import parse_qs, quote_plus, unquote, urljoin, urlsplit, urlunsplit
from xml.etree import ElementTree

MAX_BYTES = 5 * 1024 * 1024
REDIRECTS = {301, 302, 303, 307, 308}
USER_AGENT = "ContextStack-ContextStack/0.1 (+local grounded corpus sweep)"


def fail(message):
    raise RuntimeError(message)


def safe_directory(relative):
    root = Path.cwd().resolve()
    current = root
    for part in Path(relative).parts:
        current = current / part
        if current.is_symlink():
            fail("evidence path contains a symlink: " + str(relative))
        current.mkdir(exist_ok=True)
        if current.resolve() != current or not current.resolve().is_relative_to(root):
            fail("evidence path escapes workspace")
    return current


def atomic_json(path, value, sort_keys=True):
    if path.is_symlink():
        fail("evidence record is a symlink: " + str(path))
    handle = tempfile.NamedTemporaryFile(prefix="record-", dir=path.parent, delete=False)
    handle.write((json.dumps(value, indent=2, sort_keys=sort_keys) + "\n").encode("utf-8")); handle.close()
    Path(handle.name).replace(path)


def append_manifest(path, value):
    flags = os.O_WRONLY | os.O_CREAT | os.O_APPEND
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags, 0o600)
    except OSError as error:
        fail("cannot open evidence manifest safely: " + str(error))
    with os.fdopen(descriptor, "a", encoding="utf-8") as manifest:
        manifest.write(json.dumps(value, sort_keys=True) + "\n")


def normalize_url(raw):
    if not isinstance(raw, str) or not raw.strip() or len(raw) > 4096:
        fail("url must be a nonempty string of at most 4096 characters")
    if any(ord(char) < 32 for char in raw):
        fail("url contains control characters")
    parts = urlsplit(raw.strip())
    if parts.scheme.lower() != "https":
        fail("only https URLs are allowed")
    if parts.username or parts.password:
        fail("URL credentials are not allowed")
    try:
        port = parts.port
    except ValueError as error:
        fail("invalid URL port: " + str(error))
    if port not in (None, 443):
        fail("only https port 443 is allowed")
    host = (parts.hostname or "").rstrip(".").lower()
    if not host or host == "localhost" or host.endswith(".local"):
        fail("URL host is not public")
    try:
        host = host.encode("idna").decode("ascii")
    except UnicodeError:
        fail("invalid URL host")
    netloc = "[" + host + "]" if ":" in host else host
    return urlunsplit(("https", netloc, parts.path or "/", parts.query, ""))


def public_address(host):
    try:
        rows = socket.getaddrinfo(host, 443, type=socket.SOCK_STREAM)
    except OSError as error:
        fail("DNS failed for %s: %s" % (host, error))
    addresses = sorted({row[4][0].split("%", 1)[0] for row in rows}, key=lambda x: (":" in x, x))
    if not addresses:
        fail("DNS returned no addresses for " + host)
    for raw in addresses:
        try:
            if not ipaddress.ip_address(raw).is_global:
                fail("host resolves to a non-public address: " + raw)
        except ValueError:
            fail("DNS returned an invalid address")
    return addresses[0]


def response_headers(path):
    text = path.read_bytes().decode("iso-8859-1", errors="replace")
    blocks = re.split(r"\r?\n\r?\n", text.strip())
    block = next((item for item in reversed(blocks) if item.startswith("HTTP/")), "")
    lines = block.splitlines()
    if not lines:
        fail("response did not contain HTTP headers")
    match = re.match(r"HTTP/\S+\s+(\d{3})", lines[0])
    if not match:
        fail("response status was malformed")
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            key, value = line.split(":", 1)
            headers[key.strip().lower()] = value.strip()
    return int(match.group(1)), headers


def curl_once(url, directory):
    parts = urlsplit(url)
    host, address = parts.hostname, public_address(parts.hostname)
    resolved = "%s:443:%s" % (host, "[" + address + "]" if ":" in address else address)
    header = tempfile.NamedTemporaryFile(prefix="headers-", dir=directory, delete=False)
    body = tempfile.NamedTemporaryFile(prefix="body-", dir=directory, delete=False)
    header.close(); body.close()
    env = {key: value for key, value in os.environ.items() if "proxy" not in key.lower()}
    command = ["/usr/bin/curl", "-q", "--silent", "--show-error", "--noproxy", "*",
               "--proto", "=https", "--tlsv1.2", "--connect-timeout", "8", "--max-time", "25",
               "--max-filesize", str(MAX_BYTES), "--compressed", "--user-agent", USER_AGENT,
               "--resolve", resolved, "--dump-header", header.name, "--output", body.name, "--url", url]
    result = subprocess.run(command, capture_output=True, text=True, env=env)
    hp, bp = Path(header.name), Path(body.name)
    if result.returncode != 0:
        hp.unlink(missing_ok=True); bp.unlink(missing_ok=True)
        fail("fetch failed: " + (result.stderr or result.stdout or "curl exit %d" % result.returncode).strip())
    status, headers = response_headers(hp)
    hp.unlink(missing_ok=True)
    if bp.stat().st_size > MAX_BYTES:
        bp.unlink(missing_ok=True); fail("response exceeded %d bytes" % MAX_BYTES)
    return status, headers, bp


def extension(content_type):
    kind = content_type.split(";", 1)[0].strip().lower()
    return {"text/html": ".html", "text/plain": ".txt", "application/json": ".json",
            "application/pdf": ".pdf", "application/xml": ".xml", "text/xml": ".xml"}.get(kind, ".bin")


def fetch(url, kind="fetch"):
    requested = normalize_url(url)
    evidence = safe_directory("_evidence")
    tempdir = safe_directory("_evidence/.tmp")
    current, chain = requested, []
    for _ in range(6):
        status, headers, body = curl_once(current, tempdir)
        if status not in REDIRECTS:
            break
        location = headers.get("location", "")
        body.unlink(missing_ok=True)
        if not location:
            fail("redirect omitted Location")
        nxt = normalize_url(urljoin(current, location))
        chain.append({"status": status, "from": current, "to": nxt})
        current = nxt
    else:
        fail("too many redirects")
    if status < 200 or status >= 300:
        body.unlink(missing_ok=True); fail("HTTP status %d for %s" % (status, current))
    raw = body.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    content_type = headers.get("content-type", "application/octet-stream")
    bodies = safe_directory("_evidence/bodies"); records = safe_directory("_evidence/" + kind + "es")
    body_rel = Path("_evidence/bodies") / (digest + extension(content_type))
    body_dest = Path(body_rel)
    if body_dest.is_file() and not body_dest.is_symlink() and hashlib.sha256(body_dest.read_bytes()).hexdigest() == digest:
        body.unlink(missing_ok=True)
    else:
        body.replace(body_dest)
    evidence_id = hashlib.sha256((requested + "\0" + digest).encode()).hexdigest()
    record_rel = Path("_evidence") / (kind + "es") / (evidence_id + ".json")
    record = {"schema_version": "0.1", "kind": kind, "requested_url": requested,
              "final_url": current, "status": status, "content_type": content_type,
              "retrieved_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
              "bytes": len(raw), "sha256": digest, "body_path": str(body_rel), "redirects": chain}
    atomic_json(Path(record_rel), record)
    append_manifest(evidence / "manifest.jsonl", record)
    return record, raw, str(record_rel)


class ResultsParser(HTMLParser):
    def __init__(self):
        super().__init__(); self.results = []; self.current = None
    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs); classes = attrs.get("class", "").split()
        if tag == "a" and "result__a" in classes:
            self.current = {"href": attrs.get("href", ""), "title_parts": [], "snippet_parts": []}
            self.results.append(self.current)
        elif tag == "a" and "result__snippet" in classes and self.results:
            self.current = self.results[-1]; self.current["in_snippet"] = True
    def handle_endtag(self, tag):
        if tag == "a" and self.current:
            if self.current.pop("in_snippet", False):
                self.current = None
            elif self.current.get("title_parts"):
                self.current = None
    def handle_data(self, data):
        if self.current:
            key = "snippet_parts" if self.current.get("in_snippet") else "title_parts"
            self.current[key].append(data)


def search(query, count):
    if not isinstance(query, str) or not query.strip() or len(query) > 300:
        fail("query must be 1-300 characters")
    count = int(count or 5)
    if count < 1 or count > 10:
        fail("count must be between 1 and 10")
    results = []
    try:
        record, raw, record_path = fetch("https://html.duckduckgo.com/html/?q=" + quote_plus(query.strip()), "search")
        parser = ResultsParser(); parser.feed(raw.decode("utf-8", errors="replace"))
        candidates = [(item["href"], " ".join("".join(item["title_parts"]).split()),
                       " ".join("".join(item["snippet_parts"]).split())) for item in parser.results]
        backend = "duckduckgo-html"
        skipped_hosts = {"duckduckgo.com", "www.duckduckgo.com"}
    except RuntimeError:
        candidates = []
    for raw_href, title, snippet in candidates:
        href = html.unescape(raw_href)
        if href.startswith("//duckduckgo.com/l/"):
            href = parse_qs(urlsplit("https:" + href).query).get("uddg", [""])[0]
        try:
            href = normalize_url(unquote(href))
            if urlsplit(href).hostname in skipped_hosts:
                continue
            public_address(urlsplit(href).hostname)
        except RuntimeError:
            continue
        results.append({"title": title, "url": href, "snippet": snippet})
        if len(results) >= count:
            break
    if not results:
        record, raw, record_path = fetch(
            "https://www.bing.com/search?format=rss&q=" + quote_plus(query.strip()), "search")
        try:
            root = ElementTree.fromstring(raw)
        except ElementTree.ParseError as error:
            fail("search backends returned no parseable results: " + str(error))
        backend = "bing-rss-fallback"
        for item in root.findall("./channel/item"):
            try:
                href = normalize_url(item.findtext("link", ""))
                public_address(urlsplit(href).hostname)
            except RuntimeError:
                continue
            title = " ".join(html.unescape(item.findtext("title", "")).split())
            snippet = re.sub(r"<[^>]+>", " ", item.findtext("description", ""))
            results.append({"title": title, "url": href,
                            "snippet": " ".join(html.unescape(snippet).split())})
            if len(results) >= count:
                break
    if not results:
        fail("search backends returned no parseable results")
    return {"query": query.strip(), "backend": backend, "results": results,
            "evidence": record_path, "retrieved_at": record["retrieved_at"]}
