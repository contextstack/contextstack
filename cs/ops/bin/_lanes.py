"""Repo lanes: a coding-agent job's git worktree beside a repo's main checkout, and landing it on main.
Not an op; stdlib only. Plan.RealWork.Install 2026-11-11, decisions 4 and 5.

A job's spec names `repo:` and `lane:` (both or neither). The repo name resolves through the node's repos file,
`~/.contextstack/config/repos.json` (env CS_REPOS_FILE overrides it):

  {"contextstack": {"path": "/Users/me/src/contextstack/main",   the main checkout (required)
                    "branch": "main",                             the branch lanes start from and land on
                    "hooks_path": "scripts/git-hooks",            core.hooksPath for the landing commit
                                                                  (relative to the checkout; absent: the repo's own)
                    "gate": "cs/scripts/check",              run in the checkout before the landing commit
                    "hook_gated": ["cs/"],                   paths whose change makes the hooks run the gate
                                                                  themselves: then the explicit gate is skipped
                    "gate_timeout_s": 3600,
                    "land": "squash"},                            squash (one commit) or merge (--no-ff)
   "ai-usage-ledger": "/Users/me/sites/ai-usage-ledger"}          short form: the path, every default

Tracked secret names anywhere reachable (every ref, including stash and notes) refuse lane setup
before a worktree is created. The scan runs on the host, names only, and is cached for the repo's
current ref tips. The owner may allowlist known decoy fixtures by exact path, for example:
  "tracked_secret_allowlist": ["cs/tasks/find-password/setup/secrets.env"]
No fixture is exempt by default; purge real secrets from history instead of allowlisting them.

The lane is the worktree `<parent of path>/<lane>` on branch `lane/<lane>`, made from the branch's head (or reused).
The agent works there under a sandbox that may write the worktree and its own git metadata, and nothing else of the
repo (typed sandbox requests). On a passing run the executor commits the changes on the lane branch without hooks
(commit_lane): the repo's gate is enforced once, when the lane lands on main, which is also where contextstack's
hook accepts rules-tier commits. land() merges the lane into main, runs the gate, and commits only on green; on red
or a conflict main is reset to where it was. It never pushes.
"""
import fcntl
from contextlib import contextmanager
from contextvars import ContextVar
import fnmatch
import json
import os
import re
import secrets
import shutil
import signal
import subprocess
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
from _lane_policy import lane_changed_names, refuse_agent_configs
from _harvest_git import HARDEN as LANE_GIT_HARDEN, driver_overrides, environment as git_environment

LANE = re.compile(r"^[a-z0-9][a-z0-9._-]{0,62}$")
DEFAULTS = {"branch": "main", "hooks_path": None, "gate": None, "hook_gated": [], "gate_timeout_s": 3600,
            "land": "squash", "tracked_secret_allowlist": []}
TAIL = 4000


class LaneError(ValueError):
    """A lane request refused before anything changed; the message says why."""


def repos_file():
    return Path(os.environ.get("CS_REPOS_FILE") or Path.home() / ".contextstack/config/repos.json")


def clean_env(extra=None):
    """The caller's environment without GIT_* (a hook's GIT_DIR must not redirect these commands)."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env["GIT_TERMINAL_PROMPT"] = "0"
    env.update(extra or {})
    return env


def git_common_dir(main):
    """The repo's shared git directory (main checkout); never read from a lane worktree's .git file."""
    return Path(out(main, "rev-parse", "--path-format=absolute", "--git-common-dir")).resolve()


def expected_lane_gitdir(main, lane):
    """Administrative git dir for the lane worktree beside main (basename of the worktree path)."""
    return git_common_dir(main) / "worktrees" / lane


def expected_lane_gitfile(gitdir):
    return "gitdir: %s\n" % Path(gitdir).resolve()


def assert_lane_git_pointer(lane_path, main, lane):
    """Refuse when the lane's .git file does not point at the worktree git dir derived from main."""
    work_tree = Path(lane_path).resolve()
    expected_gitdir = expected_lane_gitdir(main, lane)
    gitfile = work_tree / ".git"
    try:
        got = gitfile.read_text(encoding="utf-8")
    except OSError as error:
        raise LaneError("%s: %s" % (gitfile, error))
    want = expected_lane_gitfile(expected_gitdir)
    if got != want:
        raise LaneError("%s: expected %r, got %r" % (gitfile, want.rstrip("\n"), got.rstrip("\n")))
    return expected_gitdir


def _admin_pointer_path(admin, name):
    """Resolve commondir or gitdir under a worktree admin dir (relative paths are from admin)."""
    path = Path(admin) / name
    try:
        raw = path.read_text(encoding="utf-8").strip()
    except OSError as error:
        raise LaneError("%s: %s" % (path, error))
    if not raw:
        raise LaneError("%s: empty" % path)
    target = Path(raw)
    if not target.is_absolute():
        target = (path.parent / target).resolve()
    else:
        target = target.resolve()
    return path, target


def assert_lane_git_admin(git_dir, main, lane, lane_path):
    """Refuse when commondir or gitdir in the lane admin dir were rewired away from this repo."""
    admin = Path(git_dir).resolve()
    common = git_common_dir(main)
    _, got_common = _admin_pointer_path(admin, "commondir")
    if got_common != common:
        raise LaneError("%s/commondir: expected %s, got %s" % (admin, common, got_common))
    want_gitdir = Path(lane_path).resolve() / ".git"
    gitdir_path, got_gitdir = _admin_pointer_path(admin, "gitdir")
    if got_gitdir != want_gitdir.resolve():
        raise LaneError("%s: expected %r, got %r" % (gitdir_path, str(want_gitdir), str(got_gitdir)))


def assert_lane_refs(main, lane, git_dir):
    """Read untrusted refs without Git: neither HEAD nor the branch may redirect Git."""
    ref = "refs/heads/lane/" + lane
    head = Path(git_dir) / "HEAD"
    try:
        if head.is_symlink() or head.read_bytes() != ("ref: " + ref + "\n").encode():
            raise LaneError("%s: expected ref: %s" % (head, ref))
        branch = git_common_dir(main) / ref
        # A missing loose ref is normal when refs are packed. A loose ref must be an OID.
        if branch.is_symlink():
            raise LaneError("%s: symbolic branch ref refused" % branch)
        if branch.exists() and not re.fullmatch(rb"[0-9a-f]{40}(?:[0-9a-f]{24})?\n", branch.read_bytes()):
            raise LaneError("%s: expected a direct object ID" % branch)
    except OSError as error:
        raise LaneError("lane refs: %s" % error)
    return ref


def prepare_lane_refs(main, lane, git_dir):
    """Host preparation: packed refs may have lost their loose parent directories."""
    ref = assert_lane_refs(main, lane, git_dir)
    if any(component in ("", ".", "..") for component in ref.split("/")):
        raise LaneError("invalid lane ref")
    common = git_common_dir(main)
    try:
        for relative in (ref, "logs/" + ref):
            parent = common
            for component in Path(relative).parts[:-1]:
                parent = parent / component
                if parent.is_symlink():
                    raise LaneError("%s: symbolic ref directory refused" % parent)
                parent.mkdir(exist_ok=True)
    except OSError as error:
        raise LaneError("lane refs: %s" % error)
    # Git reads the packed OID and writes a loose ref under these parents. No
    # host-side ref update (or packed-refs write grant) is needed.
    return ref


def lane_git(main, lane, lane_path, *args, ident=None, env=None):
    """Host git against a lane worktree: explicit git-dir and work-tree, hardened config."""
    work_tree = Path(lane_path).resolve()
    git_dir = assert_lane_git_pointer(work_tree, main, lane)
    assert_lane_git_admin(git_dir, main, lane, work_tree)
    prepare_lane_refs(main, lane, git_dir)
    # Lane admin config/info/hooks are denied to the sandbox. Disable even trusted
    # drivers: agent attributes must not activate an owner's configured programs.
    host_env = git_environment(env or clean_env())
    if landing := _LAND_GIT.get():
        host_env.update(GIT_ATTR_NOSYSTEM="1", GIT_ATTR_SOURCE=landing.attributes)
    cmd = ["git", "--git-dir=%s" % git_dir, "--work-tree=%s" % work_tree, *LANE_GIT_HARDEN]
    config = subprocess.run([*cmd, "config", "--name-only", "--get-regexp", r"^(filter|diff|merge)\."],
                            capture_output=True, text=True, env=host_env, stdin=subprocess.DEVNULL)
    cmd += [*driver_overrides(config.stdout), *(ident or []), *args]
    return subprocess.run(cmd, capture_output=True, text=True, env=host_env, stdin=subprocess.DEVNULL)


def lane_out(main, lane, lane_path, *args, ident=None, env=None):
    done = lane_git(main, lane, lane_path, *args, ident=ident, env=env)
    if done.returncode != 0:
        raise LaneError("git %s: %s" % (" ".join(args), (done.stderr or done.stdout).strip()))
    return done.stdout.strip()


def workspace_git(ws, *args, lane_ctx=None, env=None):
    """Host git in a workspace: pinned lane git when lane_ctx is {main, lane}, else hardened -C git."""
    env = env or clean_env()
    if lane_ctx:
        return lane_git(lane_ctx["main"], lane_ctx["lane"], ws, *args, env=env)
    return subprocess.run(
        ["git", "-C", str(Path(ws).resolve()), *LANE_GIT_HARDEN, *args],
        capture_output=True, text=True, env=env, stdin=subprocess.DEVNULL,
    )


def workspace_out(ws, *args, lane_ctx=None, env=None):
    done = workspace_git(ws, *args, lane_ctx=lane_ctx, env=env)
    if done.returncode != 0:
        raise LaneError("git %s: %s" % (" ".join(args), (done.stderr or done.stdout).strip()))
    return done.stdout.strip()


def workspace_changed_paths(ws, base, lane_ctx=None, env=None):
    """Paths touched since base through HEAD, index, and untracked (grant enforcement)."""
    names = []
    for spec in (
        ("diff", "--name-only", "%s..HEAD" % base),
        ("diff", "--cached", "--name-only"),
        ("diff", "--name-only"),
        ("ls-files", "--others", "--exclude-standard"),
    ):
        done = workspace_git(ws, *spec, lane_ctx=lane_ctx, env=env)
        if done.returncode != 0:
            raise LaneError("git %s: %s" % (" ".join(spec), (done.stderr or done.stdout).strip()))
        names.extend((done.stdout or "").splitlines())
    seen, out = set(), []
    for n in names:
        n = n.strip()
        if n and n not in seen:
            seen.add(n)
            out.append(n)
    return out


# Shared validation helpers use git()/out() too. Scope the landing policy to this
# invocation so they cannot accidentally fall back to ordinary host git.
_LAND_GIT = ContextVar("land_git", default=None)


class LandGit:
    """All main/common-dir git during land, with owner hooks and inert attributes."""

    def __init__(self, cfg):
        main = cfg["path"]
        # The sole normal-config read: retain today's owner identity, default hooks and
        # ignore file (patterns only; nothing executes) before removing global/system
        # config from every subsequent call. Without the owner's excludes file, files it
        # hides look untracked and land refuses main as dirty (2026-11-12).
        saved = subprocess.run(
            ["git", "-C", str(main), "config", "--null", "--get-regexp",
             r"^(user\.(name|email)|author\.(name|email)|committer\.(name|email)|core\.hookspath|core\.excludesfile)$"],
            capture_output=True, text=True, env=clean_env(), stdin=subprocess.DEVNULL,
        )
        if saved.returncode not in (0, 1):
            raise LaneError("cannot read landing identity: " + saved.stderr.strip())
        values = dict(record.split("\n", 1) for record in saved.stdout.split("\0") if record)
        self.options = []
        for key, value in values.items():
            if key != "core.hookspath":
                self.options += ["-c", key + "=" + value]
        self.original_hooks = values.get("core.hookspath")
        hooks = cfg["hooks_path"] or self.original_hooks
        self.hooks = "/dev/null"
        if hooks is not None:
            self.hooks = str(main / Path(hooks).expanduser()) if hooks else ""
        self.attributes = None
        # hash-object and rev-parse do not consult attributes or execute hooks.
        self.attributes = self.output(main, "hash-object", "-w", "-t", "tree", "--stdin", input="")
        if hooks is None:
            common = self.output(main, "rev-parse", "--path-format=absolute", "--git-common-dir")
            self.hooks = str(Path(common) / "hooks")

    def environment(self, extra=None):
        host_env = clean_env(extra)
        host_env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_ATTR_NOSYSTEM="1")
        if self.attributes:
            host_env["GIT_ATTR_SOURCE"] = self.attributes
        # Gates and hooks may themselves invoke git. Inherit the same overrides.
        config = ["core.hooksPath=" + self.hooks, "core.attributesFile=/dev/null", *self.options[1::2]]
        host_env["GIT_CONFIG_COUNT"] = str(len(config))
        for n, entry in enumerate(config):
            key, value = entry.split("=", 1)
            host_env["GIT_CONFIG_KEY_%d" % n] = key
            host_env["GIT_CONFIG_VALUE_%d" % n] = value
        return host_env

    def run(self, cwd, *args, env=None, input=None):
        host_env = self.environment(env)
        command = ["git", "-C", str(cwd), "-c", "core.hooksPath=" + self.hooks,
                   "-c", "core.attributesFile=/dev/null", *self.options, *args]
        return subprocess.run(command, capture_output=True, text=True, env=host_env,
                              **({"input": input} if input is not None else {"stdin": subprocess.DEVNULL}))

    def output(self, cwd, *args, **kwargs):
        done = self.run(cwd, *args, **kwargs)
        if done.returncode:
            raise LaneError("git %s: %s" % (" ".join(args), (done.stderr or done.stdout).strip()))
        return done.stdout.strip()


def git(cwd, *args, env=None):
    if landing := _LAND_GIT.get():
        return landing.run(cwd, *args, env=env)
    return subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, text=True,
                          env=env or clean_env(), stdin=subprocess.DEVNULL)


def out(cwd, *args):
    done = git(cwd, *args)
    if done.returncode != 0:
        raise LaneError("git %s: %s" % (" ".join(args), (done.stderr or done.stdout).strip()))
    return done.stdout.strip()


def repo_config(name):
    """The repos file's entry for `name`, with defaults filled and `path` a resolved Path."""
    path = repos_file()
    try:
        repos = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise LaneError("no repos file at %s (it maps repo names to main checkouts)" % path)
    except (OSError, ValueError) as error:
        raise LaneError("repos file %s: %s" % (path, error))
    if not isinstance(repos, dict) or name not in repos:
        raise LaneError("repo %r is not in %s" % (name, path))
    entry = repos[name]
    entry = {"path": entry} if isinstance(entry, str) else entry
    if not isinstance(entry, dict) or not str(entry.get("path") or "").strip():
        raise LaneError("repo %r in %s has no path" % (name, path))
    unknown = set(entry) - set(DEFAULTS) - {"path"}
    if unknown:
        raise LaneError("repo %r in %s: unknown keys %s" % (name, path, ", ".join(sorted(unknown))))
    cfg = {**DEFAULTS, **entry, "name": name}
    cfg["path"] = Path(os.path.expanduser(str(entry["path"]))).resolve()
    if cfg["land"] not in ("squash", "merge"):
        raise LaneError("repo %r: land must be squash or merge" % name)
    if not isinstance(cfg["hook_gated"], list):
        raise LaneError("repo %r: hook_gated must be a list of path prefixes" % name)
    validate_secret_allowlist(cfg)
    if not (cfg["path"] / ".git").exists():
        raise LaneError("repo %r: %s is not a git checkout" % (name, cfg["path"]))
    return cfg


def check_lane(cfg, lane):
    if not isinstance(lane, str) or not LANE.match(lane) or ".." in lane:
        raise LaneError("lane %r: want lowercase letters, digits, '.', '_' or '-' (at most 63)" % (lane,))
    if cfg["path"].parent / lane == cfg["path"] or lane == cfg["branch"]:
        raise LaneError("lane %r is the main checkout" % lane)


def worktrees(main):
    """{resolved path: branch ref or ''} for every registered worktree of the repo."""
    found, current = {}, None
    for line in out(main, "worktree", "list", "--porcelain").splitlines():
        if line.startswith("worktree "):
            current = str(Path(line[9:]).resolve())
            found[current] = ""
        elif line.startswith("branch ") and current:
            found[current] = line[7:]
    return found


def lane_path(cfg, lane):
    return cfg["path"].parent / lane


SECRET_NAMES = (".env", ".env.*", ".env_*", ".env-*", ".envrc*", "*.env", "*.env.*")
_OID = re.compile(rb"[0-9a-f]{40}(?:[0-9a-f]{24})?")
_SECRET_SCAN_CACHE = {}  # resolved repo -> (ref tips, {path: [commit, ...]})
_SECRET_CACHE_NAME = "cs-tracked-secret-scan.json"


def validate_secret_allowlist(cfg):
    allowed = cfg.get("tracked_secret_allowlist", [])
    if not isinstance(allowed, list) or any(
        not isinstance(p, str) or not p or p.startswith("/") or
        any(part in ("", ".", "..") for part in p.split("/")) or
        any(c in p for c in "*?[") for p in allowed
    ):
        raise LaneError("tracked_secret_allowlist must be a list of exact repo-relative paths")
    return allowed


def _git_bytes(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], env=clean_env(),
                          capture_output=True, stdin=subprocess.DEVNULL)


def _secret_path(path):
    """Case-insensitive basename match. `.example` and `.sample` suffixes stay exempt."""
    name = path.rsplit("/", 1)[-1]
    folded = name.casefold()
    if folded.endswith(".example") or folded.endswith(".sample"):
        return False
    return any(fnmatch.fnmatchcase(folded, pattern) for pattern in SECRET_NAMES)


def _paths_for(meta, paths):
    """Added path, or the destination of a rename/copy. Names only; the raw line has no contents."""
    if not meta or not paths:
        return []
    kind = meta.rsplit(b" ", 1)[-1][:1]
    if kind in (b"R", b"C"):
        return paths[-1:]
    if kind == b"A":
        return list(paths)
    return []


def _parse_secret_log(raw):
    """`git log --raw -z` records -> {path: sorted commits} for secret-named paths."""
    found, parts, i = {}, raw.split(b"\0"), 0
    while i < len(parts):
        if parts[i] != b"COMMIT":
            i += 1
            continue
        i += 1
        if i >= len(parts) or b"\n" not in parts[i]:
            continue
        commit_b, meta = parts[i].split(b"\n", 1)
        i += 1
        if not _OID.fullmatch(commit_b):
            continue
        current, chosen = [], []
        while i < len(parts) and parts[i] not in (b"", b"COMMIT"):
            item = parts[i]
            i += 1
            if item.startswith(b":"):
                chosen.extend(_paths_for(meta, current))
                meta, current = item, []
            else:
                current.append(item)
        chosen.extend(_paths_for(meta, current))
        if i < len(parts) and parts[i] == b"":
            i += 1
        commit = commit_b.decode("ascii")
        for raw_path in chosen:
            path = os.fsdecode(raw_path)
            if path and _secret_path(path):
                found.setdefault(path, set()).add(commit)
    return {path: sorted(commits) for path, commits in found.items()}


def _ref_tips(repo):
    """Every ref tip, plus HEAD, so a moved branch, stash, or notes ref misses the cache."""
    listed = _git_bytes(repo, "for-each-ref", "--format=%(refname) %(objectname)")
    if listed.returncode:
        raise LaneError("cannot list refs: " + os.fsdecode(listed.stderr).strip())
    tips = []
    for line in os.fsdecode(listed.stdout).splitlines():
        name, _, oid = line.partition(" ")
        if name and _OID.fullmatch(oid.encode("ascii")):
            tips.append((name, oid))
    head = _git_bytes(repo, "rev-parse", "-q", "--verify", "HEAD^{commit}")
    if head.returncode == 0:
        oid = os.fsdecode(head.stdout).strip()
        if _OID.fullmatch(oid.encode("ascii")):
            tips.append(("HEAD", oid))
    return tuple(sorted(tips))


def _secret_cache_file(repo):
    return git_common_dir(repo) / _SECRET_CACHE_NAME


def _read_secret_cache(repo, tips):
    try:
        data = json.loads(_secret_cache_file(repo).read_text(encoding="utf-8"))
    except (OSError, ValueError, LaneError):
        return None
    if not isinstance(data, dict) or data.get("v") != 1 or data.get("tips") != [list(tip) for tip in tips]:
        return None
    paths = data.get("paths")
    if not isinstance(paths, dict):
        return None
    found = {}
    for path, commits in paths.items():
        if not isinstance(path, str) or not isinstance(commits, list) or any(not isinstance(c, str) for c in commits):
            return None
        found[path] = list(commits)
    return found


def _write_secret_cache(repo, tips, found):
    try:
        path = _secret_cache_file(repo)
        body = json.dumps({"v": 1, "tips": [list(tip) for tip in tips], "paths": found}, sort_keys=True) + "\n"
        tmp = path.with_name(path.name + ".tmp")
        tmp.write_text(body, encoding="utf-8")
        os.replace(tmp, path)
    except (OSError, LaneError):
        return


def scan_reachable_secrets(repo):
    """Names of secret files introduced anywhere `git log --all` can reach, with the introducing commits.

    `--all` walks every ref under refs/ (branches, tags, stash, notes) and HEAD. `-m` includes files that
    exist only in a merge such as a stash commit; `-M` keeps a rename onto a secret name. Raw names only.
    """
    done = _git_bytes(repo, "log", "--all", "-m", "-M", "--diff-filter=AR", "--raw", "-z",
                      "--pretty=format:COMMIT%x00%H")
    if done.returncode:
        err = os.fsdecode(done.stderr)
        if "does not have any commits" in err:
            return {}
        raise LaneError("cannot scan tracked secret paths: " + err.strip())
    return _parse_secret_log(done.stdout)


def tracked_secret_index(repo):
    """Cached scan for this repo's current ref tips. The allowlist is applied by the caller, not stored."""
    repo = Path(repo).resolve()
    tips = _ref_tips(repo)
    key = str(repo)
    hit = _SECRET_SCAN_CACHE.get(key)
    if hit is not None and hit[0] == tips:
        return hit[1]
    found = _read_secret_cache(repo, tips)
    if found is None:
        found = scan_reachable_secrets(repo)
        _write_secret_cache(repo, tips, found)
    _SECRET_SCAN_CACHE[key] = (tips, found)
    return found


def refuse_tracked_secrets(cfg, _ref="HEAD"):
    """Refuse when any reachable commit names a secret file. Names only, never file contents.

    `_ref` is accepted so older callers keep working; one tip is not enough, so the scan is the whole history.
    """
    del _ref
    allowed = set(validate_secret_allowlist(cfg))
    denied = [(path, commits) for path, commits in sorted(tracked_secret_index(cfg["path"]).items())
              if path not in allowed]
    if not denied:
        return
    bits = []
    for path, commits in denied:
        shown = ", ".join(commits[:12])
        if len(commits) > 12:
            shown += " +%d more" % (len(commits) - 12)
        bits.append("%s (commits %s)" % (repr(path), shown))
    raise LaneError("lane refused: tracked secret files: %s; purge them from history with git filter-repo or BFG. "
                    "For known decoy fixtures only, the owner can set tracked_secret_allowlist "
                    "in config/repos.json for this repo." % "; ".join(bits))


def archive_main(cfg, destination):
    """Export the configured branch without consulting or changing its checkout."""
    main = cfg["path"]
    ref = "refs/heads/" + cfg["branch"]
    commit = out(main, "rev-parse", "--verify", ref + "^{commit}")
    refuse_tracked_secrets(cfg)
    # Give archive its own attributes and object access. Export settings in the
    # source tree must not omit files or substitute their committed contents.
    staging = destination.parent
    bare = staging / "archive.git"
    object_format = out(main, "rev-parse", "--show-object-format")
    env = git_environment()
    subprocess.run(["git", "init", "-q", "--bare", "--template=", "--object-format=" + object_format, str(bare)],
                   env=env, check=True, stdin=subprocess.DEVNULL)
    (bare / "info").mkdir(exist_ok=True)
    (bare / "info/attributes").write_text("* -export-ignore -export-subst\n")
    objects = out(main, "rev-parse", "--path-format=absolute", "--git-path", "objects")
    (bare / "objects/info/alternates").write_text(objects + "\n")
    archive = staging / "repo.tar"
    subprocess.run(["git", "-C", str(bare), "archive", "--format=tar", "--output=" + str(archive), commit],
                   env=env, check=True, stdin=subprocess.DEVNULL)
    shutil.rmtree(bare)
    destination.mkdir()
    subprocess.run(["tar", "-xf", str(archive), "-C", str(destination)], check=True)
    archive.unlink()



def prepare_repo_view(cfg, destination):
    archive_main(cfg, destination)
    main = cfg["path"]
    return [str(path) for path in (destination.resolve(), Path(main).resolve(), git_common_dir(main))]


def clone_lane_dependencies(main, lane):
    """Seed ignored dependencies as independent files, never links back into main."""
    for name in ("vendor", "node_modules", "public/build"):
        source = Path(main) / "clients/web" / name
        target = Path(lane) / "clients/web" / name
        if not source.is_dir() or target.exists() or target.is_symlink():
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        if sys.platform == "darwin":
            copied = subprocess.run(["/bin/cp", "-c", "-R", "-L", str(source), str(target)],
                                    env=clean_env(), capture_output=True)
            if copied.returncode == 0:
                continue
            # Filesystems without clone support still get an independent copy.
            if target.exists():
                shutil.rmtree(target)
        shutil.copytree(source, target)


def lane_admin(main, lane):
    # Git removes its worktree metadata when a lane lands. Keep leases in
    # stable host-owned metadata, outside the sandbox's lane write grants.
    if not isinstance(lane, str) or not LANE.fullmatch(lane) or ".." in lane:
        raise LaneError("invalid lane %r" % lane)
    return git_common_dir(main) / "cs-lanes" / lane


@contextmanager
def _lane_flock(path, lane):
    with path.open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise LaneError("lane %s is busy" % lane) from error
        yield lock


@contextmanager
def lane_lock(cfg, lane):
    check_lane(cfg, lane)
    with lane_owner(cfg["path"], lane) as binding:
        yield binding


@contextmanager
def lane_owner(main, lane):
    admin = lane_admin(main, lane)
    admin.mkdir(mode=0o700, parents=True, exist_ok=True)
    with _lane_flock(admin / "owner.lock", lane) as owner:
        binding = secrets.token_hex(32)
        # A gateway executor can outlive its launcher briefly. Keep its
        # separate lock authoritative until it exits.
        with _lane_flock(admin / "execution.lock", lane):
            owner.seek(0)
            owner.truncate()
            owner.write(binding)
            owner.flush()
        try:
            yield binding
        finally:
            owner.truncate(0)
            owner.flush()


@contextmanager
def lane_execution(main, lane, binding):
    admin = lane_admin(main, lane)
    with _lane_flock(admin / "execution.lock", lane):
        # A valid binding is written by the machine launcher, never by the
        # gateway caller. Check under the execution lock against owner turnover.
        with (admin / "owner.lock").open() as owner:
            if binding and owner.read() == binding:
                try:
                    fcntl.flock(owner, fcntl.LOCK_SH | fcntl.LOCK_NB)
                except BlockingIOError:
                    yield
                    return
        raise LaneError("lane %s has no matching machine reservation" % lane)


def ensure_lane(cfg, lane):
    """Create (or reuse) the lane's worktree; returns {repo, lane, path, branch, base, created, dirty_at_start}."""
    check_lane(cfg, lane)
    main, path, branch = cfg["path"], lane_path(cfg, lane), "lane/" + lane
    head = out(main, "rev-parse", "--verify", "refs/heads/%s^{commit}" % cfg["branch"])
    refuse_tracked_secrets(cfg)
    trees = worktrees(main)
    created = False
    if str(path.resolve()) in trees and not path.is_dir():
        out(main, "worktree", "prune")  # registered but gone: forget it, then add it again
        trees = worktrees(main)
    if str(path.resolve()) in trees:
        if trees[str(path.resolve())] != "refs/heads/" + branch:
            raise LaneError("%s is a worktree on %s, not %s" % (path, trees[str(path.resolve())] or "a detached HEAD",
                                                                 branch))
    elif path.exists():
        raise LaneError("%s exists and is not the lane's worktree" % path)
    elif git(main, "rev-parse", "--verify", "-q", "refs/heads/" + branch).returncode == 0:
        out(main, "worktree", "add", "-q", str(path), branch)
        created = True
    else:
        out(main, "worktree", "add", "-q", "-b", branch, str(path), head)
        created = True
    if created:
        clone_lane_dependencies(main, path)
    dirty = bool(lane_out(main, lane, path, "status", "--porcelain"))
    return {"repo": cfg["name"], "lane": lane, "path": str(path.resolve()), "branch": branch,
            "base": lane_out(main, lane, path, "rev-parse", "HEAD"), "main": str(main), "created": created,
            "dirty_at_start": dirty}


def sandbox_read_roots(main, lane, lane_dir):
    """Read grants for Git, including HOME traversal and estate-root exceptions."""
    main, lane_dir = Path(main).resolve(), Path(lane_dir).resolve()
    gitdir = assert_lane_git_pointer(lane_dir, main, lane)
    assert_lane_refs(main, lane, gitdir)
    return (lane_dir, gitdir, git_common_dir(main))


def _lane_commit_idents(main, lane, lane_dir):
    if lane_git(main, lane, lane_dir, "config", "user.email").stdout.strip():
        return []
    return ["-c", "user.name=contextstack lane", "-c", "user.email=lane@example.test"]


def _commit_lane_index(main, lane, lane_path, message):
    """Build without HEAD, then CAS only the named ref without following symbolic refs."""
    if not isinstance(lane, str) or not LANE.fullmatch(lane) or ".." in lane:
        raise LaneError("invalid lane ref")
    ref = "refs/heads/lane/" + lane
    old = lane_out(main, lane, lane_path, "rev-parse", "--verify", ref + "^{commit}")
    if not re.fullmatch(r"[0-9a-f]{40}", old):
        raise LaneError("lane parent must be a full 40-hex commit ID")
    tree = lane_out(main, lane, lane_path, "write-tree")
    ident = _lane_commit_idents(main, lane, lane_path)
    new = lane_out(main, lane, lane_path, "commit-tree", tree, "-p", old, "-m", message, ident=ident)
    if not re.fullmatch(r"[0-9a-f]{40}", new):
        raise LaneError("lane commit must be a full 40-hex commit ID")
    lane_out(main, lane, lane_path, "update-ref", "--no-deref", "-m", message, ref, new, old)
    return new


def commit_lane(lane_dir, message, *, main, lane):
    """Commit everything in the worktree on its lane branch, without hooks: the gate runs when the lane lands.
    Returns the new commit, or None when there was nothing to commit."""
    main, lane_path = Path(main).resolve(), Path(lane_dir).resolve()
    if lane_git(main, lane, lane_path, "add", "-A", "--", ".", ":(exclude).cs-*", ":(exclude).cs-*/**").returncode != 0:
        raise LaneError("git add failed in %s" % lane_path)
    if lane_git(main, lane, lane_path, "diff", "--cached", "--quiet").returncode == 0:
        return None
    return _commit_lane_index(main, lane, lane_path, message)


def commit_tracked_lane_changes(lane_dir, message, *, main, lane):
    """Commit only tracked edits in a lane worktree; untracked files are handled by land() policy."""
    main, lane_path = Path(main).resolve(), Path(lane_dir).resolve()
    tracked = lane_git(main, lane, lane_path, "diff", "--quiet").returncode != 0 \
        or lane_git(main, lane, lane_path, "diff", "--cached", "--quiet").returncode != 0
    if not tracked:
        return None
    if lane_git(main, lane, lane_path, "add", "-u", "--", ".").returncode != 0:
        raise LaneError("git add -u failed in %s" % lane_path)
    if lane_git(main, lane, lane_path, "diff", "--cached", "--quiet").returncode == 0:
        return None
    return _commit_lane_index(main, lane, lane_path, message)


# ---- landing -----------------------------------------------------------------------------------------------------

def failed_lines(text):
    lines = [line.strip() for line in text.splitlines() if re.search(r"\bFAIL\b|^FAILED|^error:", line.strip())]
    return lines[:20] or [line.strip() for line in text.strip().splitlines()[-3:]]


# After land() decides, `cs land` reports the outcome to the Director (ops/bin/_land_record.py)
def land(name, lane, message=None, *, allow_agent_config=False):
    """Land lane/<lane> on the repo's main branch. Returns (exit code, result): 0 landed, 1 red or conflict (main
    reset to where it was), 2 refused before anything changed."""
    result = {"ok": False, "repo": name, "lane": lane, "branch": "lane/%s" % lane}

    def refused(reason, **more):
        result.update(status="refused", reason=reason, **more)
        return 2, result

    try:
        cfg = repo_config(name)
        check_lane(cfg, lane)
    except LaneError as error:
        return refused(str(error))
    main = cfg["path"]
    result["main"] = str(main)
    try:
        landing = LandGit(cfg)
    except LaneError as error:
        return refused(str(error))
    token = _LAND_GIT.set(landing)
    # A kernel lock on a file that is never removed: it dies with its process, so a killed
    # land leaves no stale lock, and no second land can lock a fresh inode after an unlink.
    fd = None
    try:
        lock_path = git_common_dir(main) / "cs-land.lock"
        fd = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return refused("another landing is running in %s" % main)
        try:
            with lane_lock(cfg, lane):
                previous = os.read(fd, 128).strip()
                dead_lock = bool(previous) and _land_lock_holder(main) is None
                os.ftruncate(fd, 0)
                os.lseek(fd, 0, os.SEEK_SET)
                os.write(fd, ("%d\n" % os.getpid()).encode())
                try:
                    return _land(cfg, lane, message, result, refused, allow_agent_config, dead_lock)
                finally:
                    os.ftruncate(fd, 0)
        except LaneError as error:
            return refused(str(error))
    except OSError as error:
        return refused("cannot hold the landing lock in %s: %s" % (main, error))
    finally:
        _LAND_GIT.reset(token)
        if fd is not None:
            os.close(fd)


def _land_lock_holder(main):
    lock_path = git_common_dir(main) / "cs-land.lock"
    if not lock_path.is_file():
        return None
    try:
        pid = int(lock_path.read_text(encoding="utf-8").strip().split()[0])
        if pid <= 0:
            return None
    except (OSError, ValueError, IndexError):
        return None
    try:
        os.kill(pid, 0)
        return pid
    except PermissionError:
        return pid
    except (OSError, ValueError, TypeError):
        return None


def _lane_branch_heads(main):
    heads = []
    for ref in out(main, "for-each-ref", "--format=%(refname:short)", "refs/heads/lane/").splitlines():
        if ref:
            heads.append(ref)
    return heads


def _squash_merge_tree(main, branch):
    done = git(main, "merge-tree", "--write-tree", "HEAD", branch)
    if done.returncode != 0:
        return None
    line = (done.stdout or "").strip().splitlines()
    return line[0].strip() if line else None


def _recover_leftover_squash(main, dead_lock=False):
    if not dead_lock or git(main, "rev-parse", "-q", "--verify", "MERGE_HEAD").returncode == 0:
        return False
    if any(git(main, *args).returncode != expected for args, expected in (
        (("diff", "--cached", "--quiet"), 1), (("diff", "--quiet"), 0),
    )):
        return False
    index_tree = out(main, "write-tree")
    if any(_squash_merge_tree(main, branch) == index_tree for branch in _lane_branch_heads(main)):
        # The index and tracked working tree are the squash. Preserve untracked files.
        return git(main, "read-tree", "-m", "-u", "HEAD").returncode == 0
    return False


def red_marker(message):
    """contextstack's commit-msg lets a red rules commit through under a check-red: subject; a landing never does."""
    return message.lstrip().lower().startswith("check-red:")


def _land(cfg, lane, message, result, refused, allow_agent_config, dead_lock):
    from _process_group import stop_group

    main, branch = cfg["path"], result["branch"]
    if message is not None and red_marker(message):
        return refused("a landing message may not start with check-red: (landing refuses on red)")
    current = git(main, "symbolic-ref", "-q", "--short", "HEAD").stdout.strip()
    if current != cfg["branch"]:
        return refused("%s is on %s, not %s" % (main, current or "a detached HEAD", cfg["branch"]))
    _recover_leftover_squash(main, dead_lock)
    dirty = out(main, "status", "--porcelain")
    if dirty:
        return refused("main has uncommitted changes: " + "; ".join(dirty.splitlines()[:10]))
    if git(main, "rev-parse", "-q", "--verify", "MERGE_HEAD").returncode == 0:
        return refused("main has a merge in progress")
    if git(main, "rev-parse", "-q", "--verify", "refs/heads/" + branch).returncode != 0:
        return refused("no branch %s" % branch)
    path = lane_path(cfg, lane)
    registered = str(path.resolve()) in worktrees(main)
    if registered and path.is_dir():
        untracked = [p for p in lane_out(main, lane, path, "ls-files", "--others", "--exclude-standard").splitlines()
                     if p]
        if untracked:
            return refused("the lane has untracked files; commit, ignore or remove them before landing",
                           paths=untracked[:100])
        lane_commit = commit_tracked_lane_changes(path, "land %s: tracked changes before landing" % branch, main=main,
                                                  lane=lane)
        if lane_commit:
            result["lane_commit"] = lane_commit
    # Preserve tracked work on the lane before enforcing the owner-only landing policy.
    if not allow_agent_config:
        refuse_agent_configs(main, lane_changed_names(out, main, branch),
                             out, LaneError, repos_file(), branch=branch,
                             hooks_paths=(_LAND_GIT.get().original_hooks,))
    assert_lane_refs(main, lane, expected_lane_gitdir(main, lane))
    lane_tip = out(main, "rev-parse", "--verify", "refs/heads/" + branch + "^{commit}")
    before = out(main, "rev-parse", "HEAD")
    result["before"] = before
    subjects = out(main, "log", "--reverse", "--format=%s", "%s..%s" % (before, lane_tip)).splitlines()
    if not subjects:
        return refused("nothing to land: %s has no commits that %s lacks" % (branch, cfg["branch"]))

    def restore():
        git(main, "reset", "-q", "--hard", before)
        git(main, "merge", "--abort")
        git(main, "clean", "-fd")
        result["main_unchanged"] = out(main, "rev-parse", "HEAD") == before and not out(main, "status", "--porcelain",
                                                                                         "--untracked-files=no")

    merge = ["merge", "--squash"] if cfg["land"] == "squash" else ["merge", "--no-ff", "--no-commit"]
    committed = False
    interrupted = False
    gate_proc = None
    old_handlers = {}

    class GateInterrupted(Exception):
        pass

    def _on_land_signal(_signum, _frame):
        nonlocal interrupted
        if not interrupted:
            interrupted = True
            if gate_proc is not None:
                raise GateInterrupted

    try:
        for sig in (signal.SIGTERM, signal.SIGINT):
            old_handlers[sig] = signal.signal(sig, _on_land_signal)
        done = git(main, *merge, lane_tip)
        if done.returncode != 0:
            conflicts = git(main, "diff", "--name-only", "--diff-filter=U").stdout.split()
            restore()
            result.update(status="conflict", files=conflicts, reason="the lane does not merge cleanly",
                          output_tail=(done.stdout + done.stderr)[-TAIL:])
            return 1, result
        files = out(main, "diff", "--cached", "--name-only", before).splitlines()
        result["files"] = files
        if not files:
            restore()
            return refused("nothing to land: main already has the lane's changes")

        if interrupted:
            return refused("land interrupted")
        hooks_cover = bool(cfg["hooks_path"]) and any(f.startswith(tuple(cfg["hook_gated"])) for f in files)
        result["gate"] = {"hooks": cfg["hooks_path"] or "repo default", "command": None}
        if cfg["gate"] and not hooks_cover:
            result["gate"]["command"] = cfg["gate"]
            try:
                gate_proc = subprocess.Popen(["/bin/sh", "-c", cfg["gate"]], cwd=str(main), stdout=subprocess.PIPE,
                                             stderr=subprocess.STDOUT, text=True,
                                             env=_LAND_GIT.get().environment(), stdin=subprocess.DEVNULL, start_new_session=True)
                try:
                    if interrupted:
                        raise GateInterrupted
                    gate_out, _ = gate_proc.communicate(timeout=int(cfg["gate_timeout_s"]))
                except subprocess.TimeoutExpired:
                    stop_group(gate_proc)
                    gate_out, _ = gate_proc.communicate()
                    gate_rc = 124
                else:
                    gate_rc = gate_proc.returncode
                gate_proc = None
                gate_out = gate_out or ""
            except GateInterrupted:
                return refused("land interrupted")
            except OSError as error:
                gate_rc, gate_out = 1, str(error)
            if gate_rc != 0:
                restore()
                result.update(status="red", stage="gate", failed=failed_lines(gate_out), output_tail=gate_out[-TAIL:])
                return 1, result

        if message is None:
            if len(subjects) == 1:
                message = out(main, "log", "-1", "--format=%B", lane_tip)
            else:
                message = "Land %s\n\n%s\n" % (branch, "\n".join("- " + s for s in subjects))
            if red_marker(message):
                restore()
                return refused("the lane's commit message starts with check-red:; land it with -m")
        commit = _LAND_GIT.get().run(main, "commit", "-q", "-F", "-", input=message,
                                    env={"CS_LAND_COMMIT": "1"})
        after = out(main, "rev-parse", "HEAD")
        if commit.returncode != 0 or after == before:
            text = commit.stdout + commit.stderr
            restore()
            result.update(status="red", stage="commit", failed=failed_lines(text), output_tail=text[-TAIL:])
            return 1, result
        committed = True
    finally:
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)
        try:
            if gate_proc is not None:
                stop_group(gate_proc)
                gate_proc.wait()
                gate_proc.stdout.close()
        finally:
            if not committed:
                restore()

    result.update(ok=True, status="landed", commit=after, removed={"worktree": None, "branch": None})
    if registered:
        removal = git(main, "worktree", "remove", str(path))
        if removal.returncode == 0:
            result["removed"]["worktree"] = str(path)
        else:
            result["cleanup"] = "worktree not removed: " + (removal.stderr or removal.stdout).strip()
    if not result.get("cleanup"):
        if git(main, "branch", "-D", branch).returncode == 0:
            result["removed"]["branch"] = branch
    return 0, result
