"""Compare the documented policy inventory with the Rust route patterns."""
from itertools import product
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[2]
ARM = re.compile(r"(.*?)=>\s*\{?\s*Operation\((\w+),[^\n]*", re.S)
PATTERN = re.compile(r'\(\s*((?:"[A-Z]+"(?:\s*\|\s*)?)+)\s*,\s*((?:\s*\|?\s*\[[^\]]+\])+)')


def policy_routes(source):
    table = source.split("fn route<'a>", 1)[1].split("_ => return None", 1)[0]
    table = table.split("match (method, path) {", 1)[1]
    table = re.sub(r"//[^\n]*", "", table)
    end = 0
    for arm in ARM.finditer(table):
        end = arm.end()
        patterns, capability = arm.groups()
        patterns = re.sub(r"^\s*\}", "", patterns).strip()
        tuples = PATTERN.findall(patterns)
        remaining = PATTERN.sub("", patterns)
        assert not remaining.translate(str.maketrans("", "", "),|")).strip(), patterns
        for methods, paths in tuples:
            for path in re.findall(r"\[([^\]]+)\]", paths):
                segments = []
                for segment in path.split(","):
                    segment = segment.strip()
                    if not segment:
                        continue
                    literals = re.findall(r'"([^"]+)"', segment)
                    segments.append(literals or ["{rest...}" if segment == ".." else "{id}"])
                for method, parts in product(re.findall(r'"([A-Z]+)"', methods), product(*segments)):
                    yield method, "/" + "/".join(parts), capability
    assert table[end:].strip() in ("", "}", "},"), "Unparsed final policy arm"


def check():
    policy = set(policy_routes((ROOT / "director/src/policy.rs").read_text()))
    document = (ROOT / "tests/director-spec/CLIENT-API.md").read_text()
    inventory = document.split("## Policy route inventory", 1)[1]
    documented = set(re.findall(r'^\| `([A-Z]+)` \| `(/[^\n]+)` \| `(\w+)` \|$', inventory, re.M))
    assert policy and documented, "Policy inventory must be populated"
    assert documented == policy, (
        f"Missing or changed in CLIENT-API.md: {sorted(policy - documented)}\n"
        f"Stale in CLIENT-API.md: {sorted(documented - policy)}"
    )


if __name__ == "__main__":
    check()
