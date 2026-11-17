"""Protected configuration changes require an explicit owner landing."""
import inspect
import os
from pathlib import Path
from unittest.mock import patch


def run(_lanes, expect, git, state, cs_land, gwt, write_repos, tmp):
    configs = (
        "opencode.json", ".opencode/plugin.js", ".claude/settings.local.json", ".mcp.json",
        ".cursor/mcp.json", ".codex/config.toml", "nested/.claude/settings.json", ".agents/plugins/tool.json",
        ".Claude/settings.json", ".MCP.json", "OpenCode.json", "scripts/git-hooks/pre-commit",
        "owner-hooks/pre-commit", "custom-hooks/pre-commit", "nested/AGENTS.md", "CLAUDE.md", "GEMINI.md",
        ".cursorrules", ".cursor/rules/project.mdc", ".github/copilot-instructions.md",
        ".vscode/tasks.json", ".vscode/settings.json", ".vscode/launch.json", ".vscode/mcp.json",
        ".vscode/extensions.json", ".idea/project.xml", ".github/workflows/check.yml",
        ".gitlab-ci.yml", ".circleci/config.yml", ".gitattributes", ".gitmodules",
        ".windsurf/rules.md", ".windsurfrules", ".clinerules", ".roo/settings.json", ".roomodes",
        ".continue/config.json", ".aider.conf.yml", ".pre-commit-config.yaml", ".husky/pre-commit",
        ".github/actions/ci.yml", ".devcontainer/devcontainer.json",
        "cs/scripts/check", "cs/scripts/hooks/pre-commit",
    )
    # Add each path in its own lane, then seed it on main and exercise modifications.
    # Distinct repositories avoid case aliases on case-insensitive filesystems.
    for n, rel in enumerate(configs):
        main = gwt(tmp / ("policy-%d" % n))
        write_repos(policy={"path": str(main), "hooks_path": "owner-hooks"})
        git(main, "config", "core.hooksPath", str(main / "custom-hooks"))
        for action in ("add", "modify", "delete", "rename-out", "rename-in"):
            if action == "modify":
                target = main / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text("# owner seed\n")
                git(main, "add", "-A")
                git(main, "commit", "-q", "-m", "owner seed")
            lane = "%s-%d" % (action, n)
            path = Path(_lanes.ensure_lane(_lanes.repo_config("policy"), lane)["path"])
            target = path / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            if action == "delete":
                target.unlink()
            elif action == "rename-out":
                target.rename(path / "ordinary.txt")
            elif action == "rename-in":
                (path / "README").rename(target)
            else:
                target.write_text("# changed\n")
            before = state(main)
            commit = _lanes.commit_lane(path, "config change", main=main, lane=lane)
            expect(commit and git(path, "rev-parse", "HEAD") == commit,
                   action + " host commits " + rel)
            code, out = cs_land("policy", lane)
            expect(code == 2 and rel in out.get("reason", ""), action + " land refuses " + rel, out)
            expect(state(main) == before, action + " refusal preserves main " + rel)
        # The explicit owner flag permits an existing protected file's dirty tracked edit, too.
        lane = "modify-%d" % n
        path = _lanes.lane_path(_lanes.repo_config("policy"), lane)
        (path / rel).write_text("# owner approved\n")
        code, out = cs_land("policy", lane)
        expect(code == 2 and rel in out.get("reason", ""), "dirty config stays refused " + rel, out)
        expect(git(path, "show", "HEAD:" + rel) == "# owner approved",
               "dirty protected edit committed on lane " + rel)
        expect(state(main) == before, "dirty refusal preserves main " + rel)
        code, out = cs_land("policy", lane, "--allow-agent-config")
        expect(code == 0 and (main / rel).read_text() == "# owner approved\n", "owner flag lands " + rel, out)
    path = Path(_lanes.ensure_lane(_lanes.repo_config("policy"), "ordinary")["path"])
    (path / "ordinary.txt").write_text("ordinary\n")
    _lanes.commit_lane(path, "ordinary change", main=main, lane="ordinary")
    code, out = cs_land("policy", "ordinary")
    expect(code == 0, "ordinary files still land", out)

    # Exercise the real MCP method without an estate or Director connection.
    kit = Path(_lanes.__file__).resolve().parents[2]
    import sys
    sys.path.insert(0, str(kit / "scripts"))
    import cs_mcp
    cls = next(value for value in vars(cs_mcp).values()
               if isinstance(value, type) and "land_task" in value.__dict__)
    schema = next(tool[2] for tool in cs_mcp.TOOLS if tool[0] == "land_task")
    expect("allow_agent_config" not in schema and "allow-agent-config" not in schema,
           "MCP schema exposes no owner override")
    expect("allow_agent_config" not in inspect.signature(cls.land_task).parameters,
           "MCP land_task has no owner override parameter")
    import threading
    class Fake:
        land_command = None
        def _land_lock(self, repo):
            return threading.Lock()
    fake = Fake()
    fake.land_task = cls.land_task.__get__(fake)
    try:
        fake.land_task("policy", "ordinary", allow_agent_config=True)
        expect(False, "MCP rejects owner override argument")
    except TypeError:
        expect(True, "MCP rejects owner override argument")
    protected = Path(_lanes.ensure_lane(_lanes.repo_config("policy"), "mcp-config")["path"])
    (protected / ".mcp.json").write_text("{}\n")
    _lanes.commit_lane(protected, "MCP config", main=main, lane="mcp-config")
    before = state(main)
    mcp_home = tmp / "mcp-home"
    mcp_home.mkdir(exist_ok=True)
    with patch.object(cs_mcp.agents, "kit_dir", return_value=kit), patch.dict(os.environ, {"HOME": str(mcp_home)}):
        out = fake.land_task("policy", "mcp-config")
    expect(out.get("exit") == 2 and ".mcp.json" in out.get("reason", ""),
           "MCP refuses committed protected paths", out)
    expect(state(main) == before, "MCP refusal preserves main")
    seen = {}

    class FakeLand:  # land_task runs `cs land` with Popen; record the argv it would run
        def __init__(self, argv, **_kwargs):
            seen["argv"] = list(argv)
            self.pid, self.returncode = 0, 2

        def communicate(self, timeout=None):
            return '{"ok":false}', ''

    with patch.object(cs_mcp.agents, "kit_dir", return_value=kit), patch.object(cs_mcp.subprocess, "Popen", FakeLand):
        fake.land_task("policy", "ordinary")
        expect("argv" in seen and "--allow-agent-config" not in seen["argv"], "MCP never passes owner override", seen)

    def refuse_symlink_case(name, setup):
        main = gwt(tmp / ("symlink-" + name))
        write_repos(policy={"path": str(main)})
        path = Path(_lanes.ensure_lane(_lanes.repo_config("policy"), name)["path"])
        setup(path)
        _lanes.commit_lane(path, "symlink case", main=main, lane=name)
        before = state(main)
        code, out = cs_land("policy", name)
        expect(code == 2, "symlink land refuses " + name, out)
        expect(before == state(main), "symlink refusal preserves main " + name)

    def vscode_alias(path):
        (path / "editor").mkdir(parents=True, exist_ok=True)
        (path / "editor/tasks.json").write_text('{ "version": "2.0.0", "tasks": [] }\n')
        os.symlink("editor", path / ".vscode")

    def github_alias(path):
        (path / "gh/workflows").mkdir(parents=True)
        (path / "gh/workflows/x.yml").write_text("name: x\n")
        os.symlink("gh", path / ".github")

    def claude_link(path):
        (path / ".claude").mkdir(parents=True, exist_ok=True)
        (path / ".claude/settings.json").write_text("{}\n")
        os.symlink(".claude/settings.json", path / "x")

    refuse_symlink_case("vscode-alias", vscode_alias)
    refuse_symlink_case("github-alias", github_alias)
    refuse_symlink_case("claude-link", claude_link)

    from _lane_policy import AGENT_TOOL_CONFIG_PATHS, _matches_protected
    patterns = [pattern.casefold() for pattern in AGENT_TOOL_CONFIG_PATHS]
    expect(_matches_protected("nested/.envrc", patterns),
           ".envrc is protected (not writable in this sandbox)")

    main = gwt(tmp / "symlink-ok")
    write_repos(policy={"path": str(main)})
    path = Path(_lanes.ensure_lane(_lanes.repo_config("policy"), "plain-link")["path"])
    (path / "src").mkdir(parents=True, exist_ok=True)
    (path / "src/notes.txt").write_text("notes\n")
    os.symlink("src/notes.txt", path / "alias.txt")
    _lanes.commit_lane(path, "plain symlink", main=main, lane="plain-link")
    code, out = cs_land("policy", "plain-link")
    expect(code == 0, "unprotected in-repo file symlink lands", out)
