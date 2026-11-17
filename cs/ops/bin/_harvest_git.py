"""Host-owned harvest repository; never discover git metadata in a workspace."""
import os
import subprocess
from pathlib import Path

from _estate_io import hardened_git_env

HARDEN = ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
          "-c", "commit.gpgsign=false", "-c", "gpg.program=false",
          "-c", "core.attributesFile=/dev/null"]


def repository(workspace):
    ws = Path(workspace).resolve()
    return ws.parent / ("harvest-" + ws.name + ".git")


# Host git needs no credentials: pass only locale, identity and paths (policy/env.json's base).
ENV_ALLOW = ("PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TERM", "SHELL")


def environment(source=None):
    source = os.environ if source is None else source
    # Retain the host identity, locale and paths (including HOME) allowed by policy/env.json.
    env = hardened_git_env(extra={k: source[k] for k in ENV_ALLOW if k in source})
    # Harvest must not inherit credentials or other variables outside its original allowlist.
    return {k: v for k, v in env.items() if (k in ENV_ALLOW and k in source) or k in (
        "GIT_TERMINAL_PROMPT", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL", "GIT_PAGER")}


def git(workspace, *args):
    ws = Path(workspace).resolve()
    return subprocess.run(["git", "--git-dir=" + str(repository(ws)), "--work-tree=" + str(ws),
                           *HARDEN, *args], cwd=ws.parent, capture_output=True, text=True,
                          env=environment(), stdin=subprocess.DEVNULL)


def initialize(workspace, setup=None, commit=None):
    """Called before the sandbox starts to retain trusted setup ancestry, or for a standalone snapshot."""
    repo = repository(workspace)
    repo.mkdir(mode=0o700)  # never reuse a planted path or follow a symlink
    if setup is not None and commit:
        command = ["git", *HARDEN, "clone", "--bare", "--no-local", "--template=", str(setup), str(repo)]
    else:
        command = ["git", *HARDEN, "init", "--bare", "--template=", str(repo)]
    result = subprocess.run(command, capture_output=True, text=True, env=environment())
    if result.returncode:
        raise ValueError(result.stderr.strip())
    (repo / "info").mkdir(exist_ok=True)
    (repo / "info/attributes").write_text("* -filter -diff -merge -text\n")
    if commit:
        result = git(workspace, "reset", "--mixed", commit)
        if result.returncode:
            raise ValueError(result.stderr.strip())
    return repo


def driver_overrides(config):
    """Disable every configured driver, including drivers activated by workspace attributes."""
    drivers = []
    for key in config.splitlines():
        section, _, field = key.rpartition(".")
        if key.startswith("filter."):
            for name, value in (("clean", ""), ("smudge", ""), ("process", ""), ("required", "false")):
                drivers += ["-c", section + "." + name + "=" + value]
        elif field in ("command", "textconv", "driver"):
            drivers += ["-c", key + "=false"]
    return drivers
