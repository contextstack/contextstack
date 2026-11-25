"""Hardened host Git and host-owned harvest repositories."""
import os
import subprocess
from pathlib import Path

from _estate_io import hardened_git_env

HARDEN = ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
          "-c", "commit.gpgsign=false", "-c", "gpg.program=false",
          "-c", "core.attributesFile=/dev/null", "-c", "core.excludesFile=/dev/null"]


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
    return host_git(ws, *args, git_dir=repository(ws))


def host_git(workspace, *args, git_dir=None, env=None, ident=(), index=None,
             attributes=None, timeout=None, stdout=subprocess.PIPE):
    """Run host Git with executable drivers disabled and only explicit Git overrides."""
    ws = Path(workspace).resolve()
    host_env = environment(env)
    host_env.update(GIT_OPTIONAL_LOCKS="0", GIT_ATTR_NOSYSTEM="1",
                    GIT_CEILING_DIRECTORIES=str(ws.parent))
    if index is not None:
        host_env["GIT_INDEX_FILE"] = str(index)
    if attributes is not None:
        host_env["GIT_ATTR_SOURCE"] = attributes
    cmd = ["git", "-C", str(ws), "--no-pager", *HARDEN]
    if git_dir is not None:
        cmd += ["--git-dir=" + str(git_dir), "--work-tree=" + str(ws)]
    options = dict(env=host_env, stdin=subprocess.DEVNULL, stderr=subprocess.PIPE,
                   timeout=timeout, text=True, errors="surrogateescape")
    if args[0] != "init":
        config = subprocess.run(
            [*cmd, "config", "--null", "--name-only", "--get-regexp", r"^(filter|diff|merge)\."],
            stdout=subprocess.PIPE, **options,
        )
        if config.returncode not in (0, 1):
            return config
        cmd += driver_overrides(config.stdout)
    if args[0] == "diff":
        args = ("diff", "--no-textconv", "--no-ext-diff", *args[1:])
    return subprocess.run([*cmd, *ident, *args],
                          stdout=stdout, **options)


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
    filters = set()
    for key in config.split("\0"):
        section, _, field = key.rpartition(".")
        if key.startswith("filter."):
            filters.add(section)
        elif field in ("command", "textconv", "driver"):
            drivers += ["-c", key + "=false"]
    for section in sorted(filters):
        for name, value in (("clean", ""), ("smudge", ""), ("process", ""), ("required", "false")):
            drivers += ["-c", section + "." + name + "=" + value]
    return drivers
