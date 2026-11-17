"""Local repo registration and the public inventory sent with node status."""
import fcntl
import json
import os
import re
import subprocess
from pathlib import Path

from _estate_io import write_atomic

NAME = re.compile(r"^[a-z0-9][a-z0-9._-]{0,62}$")
DEFAULT_GATE = "no extra gate command; the repo's own commit hooks run when landing"


def repos_file():
    return Path(os.environ.get("CS_REPOS_FILE") or Path.home() / ".contextstack/config/repos.json")


def read():
    path = repos_file()
    if not path.exists():
        return {}
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError("repos.json must contain an object")
    return value


def inventory():
    rows = []
    for name, entry in sorted(read().items()):
        entry = {"path": entry} if isinstance(entry, str) else entry
        rows.append({"name": name, "path": entry["path"], "branch": entry.get("branch", "main")})
    return rows


def git(path, *args):
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    result = subprocess.run(["git", "-C", str(path), *args], env=env,
                            capture_output=True, text=True, timeout=15, stdin=subprocess.DEVNULL)
    if result.returncode:
        raise ValueError(result.stderr.strip() or "git repository has no current branch")
    return result.stdout.strip()


def main(args):
    if args.action == "list":
        for row in inventory():
            print("{name}\t{path}\t{branch}".format(**row))
        print("Default gate: " + DEFAULT_GATE)
        return 0
    path = repos_file()
    path.parent.mkdir(parents=True, exist_ok=True)
    # Serialize read-modify-replace so concurrent registrations cannot lose an entry.
    with open(path.with_suffix(".lock"), "a", encoding="utf-8") as lock:
        os.chmod(lock.name, 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX)
        repos = read()
        if args.action == "remove":
            if args.name not in repos:
                raise ValueError("unknown repo: " + args.name)
            del repos[args.name]
        else:
            checkout = Path(args.path).expanduser().resolve(strict=True)
            top = Path(git(checkout, "rev-parse", "--show-toplevel")).resolve()
            if checkout != top:
                raise ValueError("PATH must be the git repository's root: " + str(top))
            name = args.name or checkout.name
            if not NAME.fullmatch(name):
                raise ValueError("repo name must be 1-63 lowercase letters, digits, '.', '_' or '-', starting with a letter or digit")
            branch = args.branch or git(checkout, "symbolic-ref", "--quiet", "--short", "HEAD")
            git(checkout, "show-ref", "--verify", "refs/heads/" + branch)
            if name in repos:
                raise ValueError("duplicate repo name: " + name)
            for entry in repos.values():
                existing = entry if isinstance(entry, str) else entry["path"]
                if Path(existing).expanduser().resolve() == checkout:
                    raise ValueError("duplicate repo path: " + str(checkout))
            repos[name] = {"path": str(checkout), "branch": branch}
            if args.gate is not None:
                repos[name]["gate"] = args.gate
        write_atomic(path, json.dumps(repos, indent=2) + "\n", mode=0o600)
    if args.action == "add":
        print("Registered %s (%s)" % (name, branch))
        print("Gate: " + (args.gate if args.gate is not None else DEFAULT_GATE))
    else:
        print("Removed " + args.name)
    return 0
