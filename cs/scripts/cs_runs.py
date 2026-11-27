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


def diff_paths(text):
    """List paths from Git patch metadata, including binary and mode-only changes."""
    paths = set()
    preamble, *sections = re.split(r"(?m)^diff --git ", text)
    for section in sections or ["\n" + preamble]:
        header, _, metadata = section.partition("\n")
        found = set()
        for line in metadata.splitlines():
            if line.startswith(("@@", "GIT binary patch", "Binary files ")):
                break
            if line.startswith(("rename from ", "rename to ", "copy from ", "copy to ")):
                found.add(line.split(" ", 2)[2])
            elif line.startswith(("--- ", "+++ ")):
                path = line[4:].split("\t", 1)[0]
                if path != "/dev/null":
                    found.add(re.sub(r'^("?)[ab]/', r"\1", path))
        if not found:
            same_path = re.fullmatch(r'("?)a/(.+)\1 \1b/\2\1', header)
            if same_path:
                quote, path = same_path.groups()
                found.add(quote + path + quote)
        paths.update(found)
    return sorted(paths)
