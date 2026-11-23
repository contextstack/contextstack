"""A throwaway installation tick can run in: every op copied, the side passes stubbed, git
initialised, policy/env.json from the live tree. Shared by spool-delivery and error-classes."""
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

CODE_ROOT = Path(__file__).resolve().parents[3]
STUB_OK = "#!/bin/sh\necho '{\"ok\":true,\"skip\":\"fixture\"}'\n"
SPEC = """id: %s
model: %s
system: prompts/coder-min.md
context: []
tools: []
post_turn: []
mcp: []
%sbudget: {turns: 1, tool_calls: 0, usd: 0, seconds: 30}
tasks: [tasks/fx]
trials: 1
hypothesis: "fixture-1984: %s"
"""


def build(root, spool=None, local_ready=True):
    root = Path(root)
    for d in ("ledger/runs", "specs", "tasks/fx", "ops/bin", "policy", "grants", "prompts", "home", "tmp"):
        (root / d).mkdir(parents=True, exist_ok=True)
    (root / "INVARIANTS.md").write_text("# fixture estate\n", encoding="utf-8")
    (root / "ESTATE").write_text("dev\n", encoding="utf-8")
    shutil.copy2(CODE_ROOT / "prompts/coder-min.md", root / "prompts/coder-min.md")
    shutil.copy2(CODE_ROOT / "policy/env.json", root / "policy/env.json")
    (root / "tasks/fx/goal.md").write_text("fixture\n", encoding="utf-8")
    (root / "tasks/fx/check").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    (root / "tasks/fx/check").chmod(0o755)
    for p in (CODE_ROOT / "ops/bin").iterdir():
        if p.is_file():
            shutil.copy2(p, root / "ops/bin" / p.name)
    for name in ("corpus-audit", "void-watch", "shadow-cycle"):
        (root / "ops/bin" / name).write_text(STUB_OK, encoding="utf-8")
        (root / "ops/bin" / name).chmod(0o755)
    (root / "ops/bin/local-model-ready").write_text("#!/bin/sh\nexit %d\n" % (0 if local_ready else 1), encoding="utf-8")
    (root / "ops/bin/local-model-ready").chmod(0o755)
    if spool is not None:
        set_spool(root, spool)
    env = git_env(root)
    subprocess.run(["git", "init", "-q", str(root)], check=True, env=env)
    subprocess.run(["git", "-C", str(root), "-c", "user.name=Fixture", "-c", "user.email=f@example.invalid",
                    "commit", "--allow-empty", "-qm", "fixture"], check=True, env=env)
    return root


def set_spool(root, spool):
    (Path(root) / "policy/spool.json").write_text(json.dumps(spool) + "\n", encoding="utf-8")


def git_env(root, **extra):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(Path(root) / "home"),
           "TMPDIR": str(Path(root) / "tmp"), "LANG": os.environ.get("LANG", "en_US.UTF-8"),
           "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
    env.update(extra)
    return env


def spec(root, name, model="fake", env_request=None):
    extra = "env: [%s]\n" % ", ".join(env_request) if env_request else ""
    (Path(root) / "specs" / (name + ".yaml")).write_text(SPEC % (name, model, extra, name), encoding="utf-8")
    return "specs/%s.yaml" % name


def op(root, name, payload=None, **extra):
    r = subprocess.run([sys.executable, str(Path(root) / "ops/bin" / name)], input=json.dumps(payload or {}),
                       text=True, capture_output=True, cwd=str(root), env=git_env(root, **extra), timeout=120)
    try:
        out = json.loads(r.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        out = {"raw": r.stdout[-500:], "stderr": r.stderr[-800:]}
    return r.returncode, out, r.stderr


def dispatch(root):
    path = Path(root) / "ledger/dispatch.jsonl"
    return [json.loads(l) for l in path.read_text(encoding="utf-8").splitlines() if l.strip()] if path.is_file() else []
