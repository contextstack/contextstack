"""Baseline manifest (Plan.ModelGateway.v2.1 §10 phase 1): what was run, on
what, how, and with which result, so a later run is comparable or visibly not."""
import hashlib
import json
import platform
import subprocess
import sys
from pathlib import Path

from .gateway import SPEC

METHOD = ("Black box at the process boundary: each case starts or reuses a gateway process with a "
          "fresh HOME and cwd, a minimal environment (no inherited variables, no *_API_KEY), credentials "
          "off unless the case supplies a temp file store with fake keys, and every outbound connection "
          "through the suite's egress proxy (provider hosts TLS-terminated locally to mocks, all others "
          "refused). Assertions are on HTTP status, headers, bodies, SSE frames, bytes sent upstream, "
          "child argv/stdin/environment, exit codes, files and log rows. Normalised: latency_ms, "
          "timestamps (RFC3339 fields compared by shape or relation), uptime, ephemeral ports and temp "
          "paths. Never normalised: resolved model, cost amount/class/est_usd/charged_usd, error class "
          "and code, missing terminal events, upstream request counts.")


def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def tree_hash(root, skip=("baselines", "__pycache__", "dispositions.json")):
    h = hashlib.sha256()
    files = sorted(p for p in Path(root).rglob("*") if p.is_file() and not any(s in p.parts or p.name == s for s in skip))
    for p in files:
        h.update(str(p.relative_to(root)).encode() + b"\0" + sha256_file(p).encode() + b"\n")
    return h.hexdigest(), len(files)


def git(*args):
    try:
        return subprocess.run(["git", *args], cwd=SPEC, capture_output=True, text=True, timeout=20).stdout.strip()
    except OSError:
        return ""


def effective_config(run):
    """The environment and models.json every shared gateway starts from, with
    run-specific ports and temp paths replaced by placeholders."""
    from .gateway import Gateway  # noqa: F401
    class Fake:
        dir = run.tmp / "manifest"
        home = dir / "home"
    Fake.dir.mkdir(exist_ok=True)
    env = run.base_env(Fake)
    cfg = run.config()
    text = json.dumps({"env": env, "models": cfg, "profile": run.profile}, sort_keys=True)
    for real, ph in ((str(run.tmp), "<TMP>"), (run.vllm_url, "<VLLM>"), (run.ollama_url, "<OLLAMA>"), (run.proxy.url, "<PROXY>"),
                     (str(Path(sys.executable).resolve()), "<PYTHON>")):
        text = text.replace(real, ph)
    return hashlib.sha256(text.encode()).hexdigest(), json.loads(text)


def build(run, results, selected, source_commit, fast, seconds):
    fx_hash, fx_files = tree_hash(SPEC)
    cfg_hash, cfg = effective_config(run)
    counts = {}
    for r in results:
        counts[r["result"]] = counts.get(r["result"], 0) + 1
    by_area = {}
    for c in selected:
        by_area[c.area] = by_area.get(c.area, 0) + 1
    toolchain = ""
    if run.impl == "go":  # the toolchain stamped into the binary; no Go install needed on the host
        import re
        m = re.search(rb"go1\.\d+(?:\.\d+)?", Path(run.binary).read_bytes())
        toolchain = m.group(0).decode() if m else "unknown"
    return {
        "schema": "modelgateway-spec.baseline.v1",
        "impl": run.impl,
        "source": {"commit": source_commit or git("rev-parse", "HEAD"),
                   "suite_commit": git("rev-parse", "HEAD"),
                   "suite_dirty": bool(git("status", "--porcelain", "--", str(SPEC)))},
        "binary": {"path": run.binary, "sha256": sha256_file(run.binary)},
        "toolchain": {"implementation": toolchain, "harness": "python " + platform.python_version(),
                      "openssl": subprocess.run(["openssl", "version"], capture_output=True, text=True).stdout.strip()},
        "os": {"system": platform.system(), "release": platform.release(), "machine": platform.machine(), "platform": platform.platform()},
        "effective_config": {"sha256": cfg_hash, "value": cfg, "dropped_disables": run.dropped_disables},
        "fixtures": {"sha256": fx_hash, "files": fx_files},
        "selection": {"fast_only": fast, "cases": len(selected), "by_area": by_area},
        "concurrency": "cases run sequentially; shared gateways per configuration (base, auth, admit, cloud) reused across cases with mocks reset between them; within a case, concurrency is explicit (admission and cancellation cases hold upstream requests).",
        "method": METHOD,
        "results": {"counts": counts, "seconds": round(seconds, 1),
                    "cases": [{k: r[k] for k in ("id", "area", "result") if k in r} for r in results]},
    }
