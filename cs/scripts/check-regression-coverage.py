"""Require each regression entry point to have a full-check invocation or exclusion."""
import re
from pathlib import Path


CS = Path(__file__).resolve().parents[1]
root = CS / "tasks/regressions"
check = "\n".join(line for line in (CS / "scripts/check").read_text().splitlines()
                  if not line.lstrip().startswith("#"))
registered = set(re.findall(r"tasks/regressions/([a-z0-9-]+)/check\b", check))
exclusions = (root / "excluded-from-full-check.txt").read_text().splitlines()
excluded = set()
errors = []
for line in exclusions:
    if not line.strip() or line.lstrip().startswith("#"):
        continue
    name, _, reason = line.partition("#")
    name = name.strip()
    if not reason.strip() or name in excluded:
        errors.append("invalid or duplicate regression exclusion: " + line)
    excluded.add(name)

regressions = {path.parent.name for path in root.glob("*/check")}
errors.extend("regression missing from the full check: " + name
              for name in sorted(regressions - registered - excluded))
errors.extend("stale regression exclusion: " + name
              for name in sorted((excluded - regressions) | (excluded & registered)))
if errors:
    raise SystemExit("\n".join(errors))
