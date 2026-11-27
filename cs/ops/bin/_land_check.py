"""Prepare and verify a detached sandboxed land checkout."""
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
from contextlib import contextmanager
from pathlib import Path
from tempfile import TemporaryDirectory
from urllib.parse import quote, quote_plus

import _toolchains
from _estate_io import land_check_request, render_land_check_request, sandbox_request

SEATBELT = "/usr/bin/sandbox-exec"
KIT = Path(__file__).resolve().parents[2]
SEATBELT_PROBE = KIT / "policy/seatbelt-probe.sb"
LINUX_BWRAP_PATH = "/usr/bin:/usr/local/bin"
KILL_SWITCH = 'set "land_check": "host" for the repo in %s to use the owner kill switch'
FLUX_HOST = "composer.fluxui.dev"


def sandbox_readiness(estate, *, platform=None):
    """The installed launcher and OS facilities needed to confine a land check."""
    platform = platform or sys.platform
    binary = estate / "current/bin/cs-sandbox"
    if platform in ("darwin", "macos"):
        try:
            result = subprocess.run([SEATBELT, "-f", str(SEATBELT_PROBE), "/usr/bin/true"],
                                    env=tool_environment(), stdin=subprocess.DEVNULL,
                                    capture_output=True, text=True, errors="replace", timeout=10)
            if result.returncode:
                return False, "cannot apply Seatbelt: " + (result.stderr.strip() or str(result.returncode))
        except (OSError, subprocess.TimeoutExpired) as error:
            return False, "cannot apply Seatbelt: " + str(error)
        backend = "Seatbelt"
    elif platform == "linux":
        from _linux_sandbox import sandbox_findings
        for finding in sandbox_findings(bwrap_path=LINUX_BWRAP_PATH):
            if not finding["ok"]:
                return False, "%s: %s" % (finding["check"], finding["detail"])
        backend = "bubblewrap (bwrap), user namespaces and seccomp"
    else:
        return False, "no land sandbox backend for " + platform
    if not binary.is_file() or not os.access(binary, os.X_OK):
        return False, "installed cs-sandbox is missing or not executable at %s" % binary
    return True, backend + " ready"


def linux_gate_command(binary, request):
    """Use the existing Linux check launcher with private scratch and no host sockets or port relays."""
    reads = [*request["toolchain_roots"], *request["toolchain_links"], request["git_dir"]]
    writes = [request["cargo_target_dir"], *request["cache_dirs"]]
    executor = sandbox_request(
        workspace=request["worktree"], tmpdir=request["tmpdir"], home=request["home"],
        executable="/bin/sh", lookup_path=(), ssh_auth_sock=request["ssh_auth_sock"],
        check_scratch=request["scratch"], read_roots=[*reads, *writes],
        caller_grants=[dict(path=path, access="read_write") for path in writes],
        deny_write_roots=[request["main_checkout"], request["git_dir"], *request["readonly_dirs"]],
        toolchains=dict(cargo_configured=True, read_roots=request["toolchain_roots"],
                        proxy_paths=request["toolchain_links"]))
    return [str(binary), "--policy", str(KIT / "policy/sandbox.json"), "--profile", "lane-check-run",
            "--request-json", json.dumps(executor, separators=(",", ":")), "--", "/bin/sh", "-c"]


def host_only_regressions(worktree):
    goals = (worktree / "cs/tasks/regressions").glob("*/goal.md")
    result = set()
    for goal in goals:
        if goal.is_symlink() or not goal.is_file() or goal.resolve() != goal:
            continue
        frontmatter = goal.read_text().split("---\n", 2)
        if len(frontmatter) == 3 and not frontmatter[0] and re.search(r"^host_only: ", frontmatter[1], re.M):
            result.add(goal.parent.name)
    return result


def uncovered(host_only, output):
    names = r"([a-z0-9][a-z0-9_.-]*(?:/[a-z0-9][a-z0-9_.-]*)?)"
    legacy = re.findall(r"^([^:\n]+): skipped: needs host \(", output, re.M)
    markers = re.findall(r"^check: host-only uncovered: " + names + r"$", output, re.M)
    return sorted(host_only.union(legacy, markers))


def tool_environment():
    names = ("LANG", "LC_ALL", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "UV_PYTHON_INSTALL_DIR")
    env = {name: os.environ[name] for name in names if name in os.environ}
    path = os.environ.get("PATH", os.defpath)
    env["PATH"] = os.pathsep.join(p for p in path.split(os.pathsep) if os.path.isabs(p))
    return env


def error_tail(output):
    lines = [line.strip() for line in output.splitlines() if line.strip()]
    # Registry and proxy diagnostics can embed credentials in URLs.
    return re.sub(r"\b(?:https?|socks5h?)://[^\s<>]+", "[redacted URL]", "\n".join(lines[-8:]))[-1000:]


def prep(run, argv, cwd, env, record, *, error_detail=error_tail):
    entry = {"command": " ".join(argv[:2]), "status": "started"}
    record.append(entry)
    rc, output = run(argv, cwd, env)
    entry["status"] = rc
    if rc:
        detail = error_detail(output)
        raise RuntimeError("%s pre-fetch failed (%s)%s" % (entry["command"], rc, ": " + detail if detail else ""))


def copy_cache(source, destination):
    flags = "-cR" if sys.platform == "darwin" else "-R"
    subprocess.run(["/bin/cp", flags, str(source), str(destination)], check=True, stdin=subprocess.DEVNULL)


def npm_dependencies(worktree, run, scratch, env, record, relative="cs/tools/defuddle"):
    project = worktree / relative
    if project.resolve() != project:
        raise RuntimeError("defuddle project path must not traverse a symlink")
    lock = project / "package-lock.json"
    if not lock.exists() and not lock.is_symlink():
        return
    manifest = project / "package.json"
    if any(path.is_symlink() or not path.is_file() for path in (manifest, lock)):
        raise RuntimeError("defuddle dependency manifests must be regular files")
    modules = project / "node_modules"
    if modules.exists() or modules.is_symlink():
        raise RuntimeError("defuddle node_modules must come from the dependency install")
    staging = scratch / project.name
    staging.mkdir()
    # Keep candidate .npmrc, local dependencies and Git helpers out of the host install.
    lock_data = json.loads(lock.read_text())
    manifest_data = json.loads(manifest.read_text())
    packages = lock_data.get("packages") if isinstance(lock_data, dict) else None
    if not isinstance(packages, dict) or not isinstance(manifest_data, dict) or lock_data.get("lockfileVersion") != 3:
        raise RuntimeError("expected npm lockfile version 3 and a package manifest")
    dependency_keys = ("dependencies", "devDependencies", "optionalDependencies", "peerDependencies")
    version_range = r"[\d*^~<>=xX|. +\-]+[\w.*^~<>=| +\-]*"
    registry_spec = re.compile(r"(?:npm:(?:@[\w.-]+/)?[\w.-]+@)?" + version_range)
    for package in [manifest_data, *packages.values()]:
        if not isinstance(package, dict):
            raise RuntimeError("invalid npm dependency entry")
        for key in dependency_keys:
            specs = package.get(key, {})
            if not isinstance(specs, dict) or any(
                not isinstance(spec, str) or not registry_spec.fullmatch(spec)
                for spec in specs.values()
            ):
                raise RuntimeError("npm dependencies must use registry version ranges")
    for name, spec in packages.items():
        if not name:
            continue
        if spec.get("link") or not (
            str(spec.get("resolved", "")).startswith("https://registry.npmjs.org/") and spec.get("integrity")
            or spec.get("inBundle") is True and "resolved" not in spec and "/node_modules/" in name
        ):
            raise RuntimeError("npm dependencies must be pinned registry tarballs or bundled packages")
    # npm only needs package identity and dependency specs. Omit executable and
    # project-discovery settings such as scripts, workspaces and overrides.
    install_keys = ("name", "version", *dependency_keys)
    (staging / manifest.name).write_text(json.dumps({k: manifest_data[k] for k in install_keys if k in manifest_data}))
    (staging / lock.name).write_text(json.dumps({k: lock_data[k] for k in (*install_keys, "lockfileVersion",
                                                                        "requires", "packages") if k in lock_data}))
    # npm refuses to load one file as both user and global config, so each gets its own empty file.
    user_rc, global_rc = scratch / "npmrc-user", scratch / "npmrc-global"
    user_rc.write_text(""); global_rc.write_text("")
    prep(run, ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund",
               "--userconfig=%s" % user_rc, "--globalconfig=%s" % global_rc], staging, env, record)
    (staging / "node_modules").rename(modules)


def cargo_dependencies(worktree, target, scratch, env, run, record):
    manifest = worktree / "Cargo.toml"
    if manifest.exists() or manifest.is_symlink():
        import tomllib

        lock = worktree / "Cargo.lock"
        for path in (manifest, lock):
            if path.is_symlink() or not path.is_file():
                raise RuntimeError(f"{path.name} must exist and be a regular file")
        checkout_target = worktree / "target"
        if checkout_target.exists() or checkout_target.is_symlink():
            raise RuntimeError("tracked target path conflicts with the private Cargo target directory")
        for package in tomllib.loads(lock.read_text()).get("package", []):
            if package.get("source") not in (None, "registry+https://github.com/rust-lang/crates.io-index"):
                raise RuntimeError("land Cargo dependencies must use crates.io")
        # Outside the checkout, Cargo cannot discover candidate config or rustup overrides.
        cargo_env = _toolchains.cargo_lane_env(scratch, land=True)
        registry = Path(cargo_env["CARGO_HOME"]) / "registry"
        if registry.is_symlink():
            host_registry = registry.resolve()
            registry.unlink()
            # Cargo's cache lock is relative to CARGO_HOME. Use a private
            # registry with that lock rather than writing to the host cache.
            copy_cache(host_registry, registry)
        prep(run, ["cargo", "fetch", "--locked", "--manifest-path", str(manifest)], scratch,
             {**env, **cargo_env, "CARGO_NET_OFFLINE": "false", "RUSTUP_AUTO_INSTALL": "0",
              "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}, record)
        env.update(cargo_env)
        # The check also locates built binaries using checkout-relative paths.
        checkout_target.symlink_to(target, target_is_directory=True)


def stage_lock(project, staging, names):
    if project.resolve() != project:
        raise RuntimeError("dependency project path must not traverse a symlink")
    staging.mkdir()
    for name in names:
        source = project / name
        if source.is_symlink() or not source.is_file():
            raise RuntimeError("%s must be a regular dependency input" % source)
        shutil.copyfile(source, staging / name)


def python_dependencies(worktree, scratch, env, run, record):
    project = worktree / "cs"
    lock = project / "uv.lock"
    if not (lock.exists() or lock.is_symlink()) or not shutil.which("uv", path=env["PATH"]):
        return
    import tomllib

    staging = scratch / "python"
    stage_lock(project, staging, ("pyproject.toml", lock.name, ".python-version"))
    config = tomllib.loads((staging / "pyproject.toml").read_text())
    if set(config.get("tool", {}).get("uv", {})) - {"package", "python-preference"}:
        raise RuntimeError("land Python pre-fetch does not accept project uv overrides")
    project_config = config.get("project", {})
    dependency_groups = (project_config.get("dependencies", []),
                         *project_config.get("optional-dependencies", {}).values(),
                         *config.get("dependency-groups", {}).values())
    if any("@" in requirement for group in dependency_groups for requirement in group
           if isinstance(requirement, str)):
        raise RuntimeError("land Python pre-fetch does not accept direct dependency URLs")
    for package in tomllib.loads((staging / lock.name).read_text()).get("package", []):
        if "source" not in package:
            raise RuntimeError("cs/uv.lock package is missing 'source'")
        if package["source"] not in ({"virtual": "."}, {"registry": "https://pypi.org/simple"}):
            raise RuntimeError("land Python dependencies must use PyPI wheels")
        if any(not re.fullmatch(r"https://files\.pythonhosted\.org/[^\s]+", wheel.get("url", ""))
               for wheel in package.get("wheels", [])):
            raise RuntimeError("cs/uv.lock wheel is missing a valid 'url' on files.pythonhosted.org")
    uv_env = _toolchains.uv_lane_env(scratch, land=True)
    uv_env.update(UV_LINK_MODE="copy", UV_NO_BUILD="1", UV_KEYRING_PROVIDER="disabled")
    host_cache = _toolchains.land_uv_cache()
    host_env = {**env, **uv_env, "UV_OFFLINE": "0"}
    if host_cache is not None:
        host_env["UV_CACHE_DIR"] = str(host_cache)
    prep(run, ["uv", "sync", "--locked", "--no-install-project", "--no-build"], staging,
         host_env, record)
    if host_cache is not None:
        # uv takes cache locks even offline. Give it a private copy while the
        # shared cache remains read-only in the gate.
        copy_cache(host_cache, uv_env["UV_CACHE_DIR"])
    env.update(uv_env)


def read_composer_auth(path):
    try:
        info = path.lstat()
    except FileNotFoundError:
        return ""
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o044:
        raise RuntimeError("unsafe Composer credentials file %s: use a regular file you own with mode 0600" % path)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, encoding="utf-8") as stream:
        if os.fstat(stream.fileno()) != info:
            raise RuntimeError("Composer credentials file changed while opening %s" % path)
        return stream.read()


def composer_auth(main):
    home = Path.home()
    config = Path(os.environ.get("XDG_CONFIG_HOME", home / ".config"))
    directories = [home / ".composer", config / "composer", main / "clients/web"]
    if "COMPOSER_HOME" in os.environ:
        directories.insert(0, Path(os.environ["COMPOSER_HOME"]))
    for source in (None, *(directory / "auth.json" for directory in directories)):
        try:
            raw = os.environ.get("COMPOSER_AUTH") if source is None else read_composer_auth(source)
            auth = json.loads(raw) if raw else {}
            if not isinstance(auth, dict):
                raise ValueError
        except ValueError:
            raise RuntimeError("invalid Composer credentials in %s" % (source or "COMPOSER_AUTH")) from None
        filtered = {method: {FLUX_HOST: hosts[FLUX_HOST]} for method, hosts in auth.items()
                    if isinstance(hosts, dict) and hosts.get(FLUX_HOST)}
        if filtered:
            return filtered
    return {}


def composer_error(output, auth):
    # Composer can append the install command synopsis after its exception.
    line = next((line.strip() for line in reversed(output.splitlines())
                 if line.strip() and not line.lstrip().startswith("install [")), "")
    line = re.sub(r"/download/[^/\s?#]+", "/download/[redacted]", line)
    line = re.sub(r"(https?://)[^/\s@]+@", r"\1[redacted]@", line)
    for hosts in auth.values():
        credentials = hosts[FLUX_HOST]
        for value in credentials.values() if isinstance(credentials, dict) else (credentials,):
            if isinstance(value, str) and value and any(
                    candidate in line for candidate in (value, quote(value, safe=""), quote_plus(value))):
                return "[redacted Composer error]"
    return line


def composer_dependencies(main, worktree, scratch, env, run, record):
    project = worktree / "clients/web"
    lock = project / "composer.lock"
    if not (lock.exists() or lock.is_symlink()):
        return False
    missing = [tool for tool in ("composer", "php", "npm") if not shutil.which(tool, path=env["PATH"])]
    if missing:
        record.append({"command": "client-web asset build", "status": "skipped",
                       "reason": "missing tools: " + ", ".join(missing)})
        return False
    staging = scratch / "composer"
    stage_lock(project, staging, ("composer.json", lock.name))
    manifest = staging / "composer.json"
    data = json.loads(manifest.read_text())
    # Omit project commands, plugins, repositories, and host path overrides.
    manifest.write_text(json.dumps({k: data[k] for k in ("name", "require", "autoload") if k in data}))
    staged_lock = staging / lock.name
    data = json.loads(staged_lock.read_text())
    needs_flux = False
    packages = data.get("packages") if isinstance(data, dict) else None
    if not isinstance(packages, list):
        raise RuntimeError("clients/web/composer.lock requires a 'packages' list")
    for package in packages:
        dist = package.get("dist", {})
        if dist.get("type") != "zip" or not re.fullmatch(
            r"https://(?:api\.github\.com/repos/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/zipball/[a-f0-9]+"
            r"|composer\.fluxui\.dev/download/[a-f0-9-]+/flux-pro-[0-9.]+\.zip)", dist.get("url", "")
        ):
            raise RuntimeError("land Composer dependencies must use trusted ZIP distributions")
        needs_flux |= dist["url"].startswith("https://" + FLUX_HOST + "/")
        package.pop("source", None)
        package.pop("notification-url", None)
    staged_lock.write_text(json.dumps(data))
    auth = composer_auth(main)
    if needs_flux and not auth:
        raise RuntimeError("no credentials for composer.fluxui.dev: put them in ~/.composer/auth.json or %s"
                           % (main / "clients/web/auth.json"))
    composer_env = {**env, "COMPOSER_HOME": str(scratch / "composer-home"),
                    "COMPOSER_NO_INTERACTION": "1", "COMPOSER_AUTH": json.dumps(auth)}
    prep(run, ["composer", "install", "--prefer-dist", "--no-dev", "--no-plugins", "--no-scripts", "--no-ansi"],
         staging, composer_env, record, error_detail=lambda output: composer_error(output, auth))
    vendor = project / "vendor"
    if vendor.exists() or vendor.is_symlink():
        raise RuntimeError("web vendor must come from the dependency install")
    (staging / "vendor").rename(vendor)
    return True


def prepare_dependencies(main, worktree, target, scratch, home, env, run, record):
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_OPTIONAL_LOCKS="0")
    env["PATH"] = os.pathsep.join((*_toolchains.discover_path_dirs(land=True), env["PATH"]))
    cargo_dependencies(worktree, target, scratch, env, run, record)
    python_dependencies(worktree, scratch, env, run, record)
    npm_env = {**env, "npm_config_cache": str(home / ".npm")}
    npm_dependencies(worktree, run, scratch, npm_env, record)
    web_build = composer_dependencies(main, worktree, scratch, env, run, record)
    if web_build:
        npm_dependencies(worktree, run, scratch, npm_env, record, "clients/web")
    env.update(npm_config_cache=npm_env["npm_config_cache"], npm_config_offline="true",
               COMPOSER_DISABLE_NETWORK="1")
    return _toolchains.discover(land=True), web_build


def land_scratch(state, common):
    # Different repositories can land concurrently under the same state root.
    key = hashlib.sha256(os.fsencode(common)).hexdigest()
    return (state / "land-check").resolve() / key


def clean_stale_land(main, common, scratch, git, out):
    prune = False
    for directory in scratch.glob("land-*"):
        if directory.is_dir() and not directory.is_symlink():
            shutil.rmtree(directory)
            prune = True
    # Older releases used a shared scratch directory. Only reclaim entries
    # whose registration belongs to this repository.
    for entry in out(main, "worktree", "list", "--porcelain", "-z").split("\0"):
        if entry.startswith("worktree "):
            path = Path(entry.removeprefix("worktree "))
            if (path.name == "wt" and path.parent.parent == scratch.parent
                    and path.parent.name.startswith("land-") and not path.parent.is_symlink()):
                if path.exists():
                    out(main, "worktree", "remove", "--force", str(path))
                if path.parent.is_dir():
                    shutil.rmtree(path.parent)
                prune = True
    if prune:
        out(main, "worktree", "prune", "--expire=now")
    for receipt in common.glob("cs-land-check-*"):
        if re.fullmatch(r"cs-land-check-[0-9a-f]{32}", receipt.name):
            receipt.unlink(missing_ok=True)


@contextmanager
def detached_checkout(main, before, tree, worktree, git, out):
    detached = out(main, "commit-tree", tree, "-p", before, "-m", "Detached land check")
    out(main, "worktree", "add", "--detach", "--no-checkout", str(worktree), detached)
    try:
        out(worktree, "read-tree", "--reset", "-u", tree)
        yield worktree
    finally:
        removed = git(main, "worktree", "remove", "--force", str(worktree))
        if removed.returncode:
            raise RuntimeError("cannot remove land worktree: " + removed.stderr.strip())


@contextmanager
def sandbox_gate(main, common, before, tree, scratch_root, estate, git, out, run, record):
    binary = (estate / "current/bin/cs-sandbox").resolve(strict=True)
    darwin_temp = None
    if sys.platform == "darwin":
        darwin_temp = Path(subprocess.check_output(
            ["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"], text=True).strip()).resolve()
    validate_land_scratch(scratch_root, main, common, binary, *((darwin_temp.parent,) if darwin_temp else ()))
    scratch_root.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(prefix="land-", dir=scratch_root) as tmp:
        root = Path(tmp)
        target = root / "target"
        target.mkdir()
        # No post-checkout hook, and no writes to the main index or working tree.
        with detached_checkout(main, before, tree, root / "wt", git, out) as worktree:
            pointer = (worktree / ".git").read_bytes()
            env = tool_environment()
            run_tmp, home, tmpdir, scratch = (root / name for name in ("run", "run/home", "run/tmp", "run/scratch"))
            for path in (run_tmp, home, tmpdir, scratch):
                path.mkdir()
            env.update(HOME=str(home), TMPDIR=str(tmpdir), CARGO_TARGET_DIR=str(target), CS_SANDBOXED_LAND_CHECK="1")
            if darwin_temp:
                for variable, name in (("DARWIN_USER_TEMP_DIR", "T"), ("DARWIN_USER_CACHE_DIR", "C"),
                                       ("XDG_CACHE_HOME", "xdg-cache")):
                    private = scratch / name
                    private.mkdir()
                    env[variable] = str(private)
            toolchains, web_build = prepare_dependencies(main, worktree, target, scratch, home, env, run, record)
            request = land_check_request(
                root=estate, home=Path.home(), main_checkout=main, worktree=worktree,
                run_tmp=run_tmp, cargo_target_dir=target, tmpdir=tmpdir, scratch=scratch,
                darwin_user_temp=darwin_temp, cache_dirs=(home,), toolchain_roots=toolchains,
                ssh_auth_sock=os.environ.get("SSH_AUTH_SOCK"),
                toolchain_links=_toolchains.discover_sandbox_links(land=True), git_dir=common,
                readonly_dirs=(worktree / "clients/web/vendor",) if web_build else ())
            if darwin_temp:
                profile = root / "seatbelt.sb"
                profile.write_bytes(render_land_check_request(binary, request))
                argv = [SEATBELT, "-f", str(profile), "/bin/sh", "-c"]
            else:
                argv = linux_gate_command(binary, request)
            if web_build:
                rc, output = run([*argv, "cd clients/web && npm run build"], worktree, env)
                if rc:
                    raise RuntimeError("offline client-web asset build failed (%s): %s" % (rc, error_tail(output)))
            yield (worktree, argv, env,
                   host_only_regressions(worktree), pointer)


def validate_land_scratch(scratch, *protected_paths):
    if scratch.is_symlink():
        raise RuntimeError("land-check repository scratch must not be a symlink")
    for path in protected_paths:
        if path.is_relative_to(scratch) or scratch.is_relative_to(path):
            raise RuntimeError("land-check writable paths overlap %s" % path)


def verify_worktree(main, worktree, tree, pointer, git, out):
    gitfile = worktree / ".git"
    if gitfile.is_symlink() or not gitfile.is_file() or gitfile.read_bytes() != pointer:
        raise RuntimeError("land check modified the worktree Git pointer")
    # The linked index is in the protected common dir. Verify tracked bytes
    # separately and avoid running Git against the just-tested checkout.
    if not tracked_matches(main, worktree, tree, git, out):
        raise RuntimeError("land check modified the tracked merge tree")


def tracked_matches(main, worktree, tree, git, out):
    """Hash tracked bytes directly, bypassing Git's stat cache and clean filters."""
    algorithm = out(main, "rev-parse", "--show-object-format")
    listing = git(main, "ls-tree", "-rz", tree)
    if listing.returncode:
        raise RuntimeError("cannot read the merge tree")
    for entry in filter(None, listing.stdout.split("\0")):
        metadata, relative = entry.split("\t", 1)
        mode, _, oid = metadata.split()
        path = worktree / relative
        try:
            if path.parent.resolve() != path.parent:
                return False
            info = path.lstat()
            if mode == "160000" and stat.S_ISDIR(info.st_mode):
                if any(path.iterdir()):
                    return False
                continue
            digest = hashlib.new(algorithm)
            if mode == "120000" and stat.S_ISLNK(info.st_mode):
                data = os.fsencode(os.readlink(path))
                digest.update(b"blob %d\0" % len(data) + data)
            elif stat.S_ISREG(info.st_mode) and mode == ("100755" if info.st_mode & stat.S_IXUSR else "100644"):
                digest.update(b"blob %d\0" % info.st_size)
                fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
                with os.fdopen(fd, "rb") as source:
                    if os.fstat(source.fileno()) != info:
                        return False
                    for chunk in iter(lambda: source.read(1024 * 1024), b""):
                        digest.update(chunk)
            else:
                return False
            if digest.hexdigest() != oid:
                return False
        except OSError:
            return False
    return True
