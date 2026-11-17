"""cs mcp: reading a run's files (stream-json transcript, candidate diff) into short summaries. Stdlib only."""
import json
import re


def transcript_text(raw, tail):
    """The agent's own words from a stream-json transcript: assistant text blocks and the final result."""
    said = []
    for line in raw.decode("utf-8", "replace").splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict):
            continue
        if event.get("type") == "assistant":
            for block in (event.get("message") or {}).get("content") or []:
                if isinstance(block, dict) and block.get("type") == "text":
                    said.append(block.get("text", ""))
        elif event.get("type") == "result" and event.get("result") and str(event["result"]).strip() not in said[-1:]:
            said.append("[result] " + str(event["result"]))
    text = "\n".join(said) if said else raw.decode("utf-8", "replace")
    return text[-tail:]


def diff_stat(raw):
    files, plus, minus = set(), 0, 0
    for line in raw.decode("utf-8", "replace").splitlines():
        if line.startswith("+++ ") or line.startswith("--- "):
            name = line[4:].split("\t")[0]
            if name != "/dev/null":
                files.add(re.sub(r"^[ab]/", "", name))
        elif line.startswith("+"):
            plus += 1
        elif line.startswith("-"):
            minus += 1
    return {"files": sorted(files)[:50], "added": plus, "removed": minus}
