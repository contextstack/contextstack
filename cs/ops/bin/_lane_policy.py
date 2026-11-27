"""Keep executable configuration changes behind an explicit owner landing."""
from fnmatch import fnmatchcase
import json
import os
import posixpath
from pathlib import Path

AGENT_TOOL_CONFIG_PATHS = (
    "opencode.json", "opencode.jsonc", ".mcp.json",
    ".opencode", ".opencode/*", ".claude", ".claude/*", ".cursor", ".cursor/*",
    ".codex", ".codex/*", ".agents", ".agents/*", ".gemini", ".gemini/*",
    "scripts/git-hooks", "scripts/git-hooks/*",
    "cs/scripts/check", "cs/scripts/hooks", "cs/scripts/hooks/*",
    "cs/ops/bin/executor-templates.json", "cs/agents", "cs/agents/*",
    ".github", ".github/*", "scripts/public-export", "scripts/public-export/*",
    ".githooks", ".githooks/*",
    "AGENTS.md", "CLAUDE.md", "GEMINI.md", ".cursorrules", ".cursor/rules/*",
    ".github/copilot-instructions.md",
    ".vscode/tasks.json", ".vscode/settings.json", ".vscode/launch.json",
    ".vscode/mcp.json", ".vscode/extensions.json",
    ".idea", ".idea/*", ".github/workflows/*",
    ".gitlab-ci.yml", ".circleci", ".circleci/*", ".gitattributes", ".gitmodules",
    ".windsurf", ".windsurf/*", ".windsurfrules", ".clinerules",
    ".roo", ".roo/*", ".roomodes", ".continue", ".continue/*", ".aider*",
    ".pre-commit-config.yaml", ".husky", ".husky/*",
    ".github/actions", ".github/actions/*",
    ".devcontainer", ".devcontainer/*", ".envrc",
)
# Version-pin files (.nvmrc, .tool-versions, mise.toml, .mise.toml) are not listed: they
# record interpreter versions and do not execute on open or at land.


def _matches_protected(name, patterns):
    parts = name.casefold().split("/")
    return any(fnmatchcase("/".join(parts[i:]), pattern)
               for i in range(len(parts)) for pattern in patterns)


def _repo_relative(parent, target):
    parent = parent or "."
    parent_posix = parent.replace("\\", "/")
    target = target.replace("\\", "/")
    if posixpath.isabs(target):
        return target.lstrip("/")
    joined = posixpath.normpath(posixpath.join(parent_posix, target))
    if joined == ".." or joined.startswith("../") or "/../" in ("/" + joined + "/"):
        return None
    return joined


def _branch_symlinks(git_out, main, branch):
    mapping = {}
    for entry in git_out(main, "ls-tree", "-r", "-z", branch).split("\0"):
        if not entry or "\t" not in entry:
            continue
        meta, path = entry.split("\t", 1)
        if meta.split()[0] != "120000":
            continue
        sha = meta.split()[2]
        mapping[path] = git_out(main, "cat-file", "-p", sha).strip()
    return mapping


def lane_change_rev(git_out, main, branch):
    """Three-dot range: changes on the lane branch since it diverged from HEAD."""
    return "%s...%s" % (git_out(main, "rev-parse", "HEAD"), branch)


def lane_changed_names(git_out, main, branch):
    rev = lane_change_rev(git_out, main, branch)
    return git_out(main, "diff", "--no-renames", "--name-only", "-z", rev).split("\0")


def _diff_symlink_paths(git_out, main, branch):
    paths = set()
    rev = lane_change_rev(git_out, main, branch)
    for line in git_out(main, "diff", "--raw", "--no-renames", rev).splitlines():
        if not line.startswith(":") or "\t" not in line:
            continue
        meta, path = line[1:].split("\t", 1)
        if any(token == "120000" for token in meta.split()[:4]):
            paths.add(path)
    return paths


def _tree_kind(git_out, main, branch, relpath):
    try:
        return git_out(main, "cat-file", "-t", "%s:%s" % (branch, relpath))
    except Exception:
        return None


def _expand_effective_paths(path, link_to_target):
    paths = {path}
    parts = path.split("/")
    for link, target in link_to_target.items():
        target_parts = [p for p in target.replace("\\", "/").split("/") if p]
        if parts[:len(target_parts)] != target_parts or len(parts) <= len(target_parts):
            continue
        alias = "/".join(link.split("/") + parts[len(target_parts):])
        paths.add(alias)
    return paths


def refuse_agent_configs(main, names, git_out, error_type, repos_file, *, branch, hooks_paths=()):
    """Check every changed path as it will exist on disk, including through symlinks."""
    patterns = [pattern.casefold() for pattern in AGENT_TOOL_CONFIG_PATHS]
    hooks = [*hooks_paths, git_out(main, "config", "--default", "", "--get", "core.hooksPath")]
    try:
        entries = json.loads(Path(repos_file).read_text())
    except FileNotFoundError:
        entries = {}
    for entry in entries.values():
        if isinstance(entry, dict) and Path(entry.get("path", "")).expanduser().resolve() == main:
            hooks.append(entry.get("hooks_path"))
    for hook in filter(None, hooks):
        path = Path(hook).expanduser()
        path = Path(os.path.abspath(main / path))
        try:
            path = path.relative_to(main)
        except ValueError:
            continue
        rel = path.as_posix().casefold()
        if rel == ".":
            patterns.append("*")
        patterns.extend((rel, rel + "/*"))

    link_to_target = _branch_symlinks(git_out, main, branch)
    blocked = {}

    def block(path, reason):
        blocked.setdefault(path, reason)

    for name in set(names) - {""}:
        for effective in _expand_effective_paths(name, link_to_target):
            if _matches_protected(effective, patterns):
                block(name, "matches protected agent or gate path %s" % effective)

    for path in _diff_symlink_paths(git_out, main, branch):
        target = link_to_target.get(path)
        if target is None:
            continue
        parent = posixpath.dirname(path)
        resolved = _repo_relative(parent, target)
        link_protected = _matches_protected(path, patterns)
        target_protected = resolved and _matches_protected(resolved, patterns)
        if link_protected:
            block(path, "symlink path is protected")
            continue
        if target_protected:
            block(path, "symlink target %s is protected" % resolved)
            continue
        if resolved is None:
            block(path, "symlink points outside the repository")
            continue
        kind = _tree_kind(git_out, main, branch, resolved)
        if kind == "tree":
            block(path, "symlink points at directory %s (hides protected layout on disk)" % resolved)
            continue
        if kind not in ("blob", "commit"):
            block(path, "symlink target %s is not an ordinary in-repo file" % resolved)

    if blocked:
        detail = "; ".join("%s (%s)" % item for item in sorted(blocked.items()))
        raise error_type("agent configuration paths require the owner to land by hand "
                         "(or use cs land --allow-agent-config): " + detail)
